use kelp_client::{Error, ResolveMode, ResolveOutcome, plugins};
use kelp_core::LocalClId;
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_testkit::net::{ADMIN, TestClient, TestServer};
use kelp_testkit::{depot_path, wasm};
use kelp_workspace::OpenAction;

const TYPEMAP: &str = r#"
[[rule]]
pattern = "**/*.tags"
kind = "text"
merge = "plugin:line-merger@1"
differ = "plugin:size-differ@1"

[[rule]]
pattern = "**/*.bin"
kind = "binary"
chunker = "plugin:fixed-chunker@1"

[[trigger]]
pattern = "**"
plugin = "plugin:todo-trigger@1"
"#;

async fn setup() -> TestServer {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let admin = server.login(ADMIN).await;
    let cache = TestClient::new().cache();
    for name in [
        "line-merger",
        "size-differ",
        "fixed-chunker",
        "todo-trigger",
    ] {
        plugins::register(&admin, &cache, name, "1", &wasm::plugin(name))
            .await
            .unwrap();
    }
    server
        .admin(Request::SetTypemap {
            depot: "game".into(),
            source: TYPEMAP.into(),
        })
        .await;
    server
}

#[tokio::test]
async fn plugins_are_registered_by_administrators_and_validated() {
    let server = setup().await;
    let alice = server.add_user("alice").await;
    let cache = TestClient::new().cache();
    let forbidden = plugins::register(&alice, &cache, "x", "1", &wasm::plugin("size-differ")).await;
    assert!(matches!(forbidden, Err(Error::Remote(ref e)) if e.kind == ErrorKind::Forbidden));
    let admin = server.login(ADMIN).await;
    let invalid = plugins::register(&admin, &cache, "x", "1", b"not a component").await;
    assert!(matches!(invalid, Err(Error::Remote(ref e)) if e.kind == ErrorKind::Invalid));
    let again = plugins::register(
        &admin,
        &cache,
        "size-differ",
        "1",
        &wasm::plugin("size-differ"),
    )
    .await;
    assert!(matches!(again, Err(Error::Remote(ref e)) if e.kind == ErrorKind::AlreadyExists));

    let Reply::Plugins(list) = alice.call(&Request::ListPlugins).await.unwrap() else {
        panic!("unexpected reply");
    };
    let merger = list.iter().find(|p| p.name == "line-merger").unwrap();
    assert_eq!(merger.worlds, ["merger"]);
}

#[tokio::test]
async fn triggers_chunkers_mergers_and_differs_run_where_the_typemap_says() {
    let server = setup().await;
    let (alice_conn, bob_conn) = (server.add_user("alice").await, server.add_user("bob").await);
    let (alice, bob) = (TestClient::new(), TestClient::new());
    let stream = depot_path("//game/main");
    let mut ws_a = alice.create_workspace(&server, "ws", &stream);
    let mut ws_b = bob.create_workspace(&server, "ws", &stream);

    alice
        .sandbox()
        .write("ws/Data/levels.tags", "forest\ndesert\n");
    alice.sandbox().write("ws/Data/blob.bin", vec![3u8; 2500]);
    ws_a.reconcile(&alice_conn).await.unwrap();
    let rejected = ws_a
        .submit(
            &alice_conn,
            &alice.cache(),
            LocalClId::DEFAULT,
            Some("no ticket".into()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(rejected, Error::Remote(ref e) if e.kind == ErrorKind::Conflict && e.message.contains("rejected by plugin:todo-trigger@1")),
        "{rejected}"
    );
    ws_a.submit(
        &alice_conn,
        &alice.cache(),
        LocalClId::DEFAULT,
        Some("levels #1".into()),
    )
    .await
    .unwrap();

    let blob = ws_a.state().have[&depot_path("//game/main/Data/blob.bin")].clone();
    let Reply::Manifests(manifests) = alice_conn
        .call(&Request::GetManifests {
            ids: vec![blob.content],
        })
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    let lens: Vec<u32> = manifests[0]
        .as_ref()
        .unwrap()
        .chunks
        .iter()
        .map(|c| c.len)
        .collect();
    assert_eq!(lens, [1000, 1000, 500], "split by the chunker plugin");

    ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();
    let tags = depot_path("//game/main/Data/levels.tags");
    let old = ws_b.state().have[&tags].content;
    for (ws, conn, client, content) in [
        (&mut ws_a, &alice_conn, &alice, "forest\ndesert\nswamp\n"),
        (&mut ws_b, &bob_conn, &bob, "desert\ncity\n"),
    ] {
        ws.open_files(
            conn,
            std::slice::from_ref(&tags),
            OpenAction::Edit,
            LocalClId::DEFAULT,
        )
        .await
        .unwrap();
        client.sandbox().write("ws/Data/levels.tags", content);
    }
    ws_b.submit(
        &bob_conn,
        &bob.cache(),
        LocalClId::DEFAULT,
        Some("city #2".into()),
    )
    .await
    .unwrap();
    ws_a.sync(&alice_conn, &alice.cache(), None).await.unwrap();
    let resolved = ws_a
        .resolve(&alice_conn, &alice.cache(), &[], ResolveMode::Auto)
        .await
        .unwrap();
    assert_eq!(resolved, [(tags.clone(), ResolveOutcome::Merged)]);
    assert_eq!(
        alice.sandbox().read("ws/Data/levels.tags"),
        b"city\ndesert\nswamp\n"
    );

    let new = ws_b.state().have[&tags].content;
    let summary = plugins::describe(
        &bob_conn,
        &bob.cache(),
        "plugin:size-differ@1",
        "Data/levels.tags",
        Some(old),
        Some(new),
    )
    .await
    .unwrap();
    assert_eq!(summary, "Data/levels.tags: 14 -> 12 bytes");
}

#[tokio::test]
async fn diff_shows_files_as_their_differ_plugin_describes_them() {
    let server = setup().await;
    let conn = server.add_user("alice").await;
    let alice = TestClient::new();
    let mut ws = alice.create_workspace(&server, "ws", &depot_path("//game/main"));
    alice.sandbox().write("ws/Data/levels.tags", "forest\n");
    alice.sandbox().write("ws/notes.txt", "one\n");
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &alice.cache(),
        LocalClId::DEFAULT,
        Some("start #1".into()),
    )
    .await
    .unwrap();

    alice
        .sandbox()
        .write("ws/Data/levels.tags", "forest\ndesert\n");
    alice.sandbox().write("ws/notes.txt", "two\n");
    let diffs = ws.diff(&conn, &alice.cache(), &[]).await.unwrap();
    let rendered: Vec<String> = diffs.iter().map(kelp_client::render_diff).collect();
    assert_eq!(
        rendered[0],
        "//game/main/Data/levels.tags (plugin:size-differ@1)\nData/levels.tags: 7 -> 14 bytes\n"
    );
    assert!(diffs[1].described.is_none());
    assert!(rendered[1].contains("-one\n+two\n"), "{}", rendered[1]);
}
