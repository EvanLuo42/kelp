//! Regression tests for workspace files that Kelp must not misjudge or
//! overwrite.

use kelp_client::{Connection, SkipReason, Workspace};
use kelp_core::{DepotPath, LocalClId};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::Config;

struct Setup {
    server: TestServer,
    conn: Connection,
    client: TestClient,
    ws: Workspace,
}

fn stream() -> DepotPath {
    depot_path("//game/main")
}

fn path(rel: &str) -> DepotPath {
    depot_path(&format!("//game/main/{rel}"))
}

/// A workspace of `//game/main` that has submitted `files`.
async fn setup(files: &[(&str, &str)]) -> Setup {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &stream());
    for (path, text) in files {
        client.sandbox().write(&format!("ws/{path}"), text);
    }
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("base".into()),
    )
    .await
    .unwrap();
    Setup {
        server,
        conn,
        client,
        ws,
    }
}

#[tokio::test]
async fn sync_skips_files_that_map_into_kelp_directories() {
    let s = setup(&[("ok.txt", "ok\n")]).await;
    // Another view can give such depot paths harmless local names.
    let mut config = Config::new("author", s.server.addr(), &stream());
    config.view = "//game/main/... ...\n\
                   //game/main/.KELP/... upper/...\n\
                   //game/main/Mod/.kelp/... Mod/meta/..."
        .into();
    let mut author = Workspace::create(&s.client.sandbox().dir("author"), config).unwrap();
    author.sync(&s.conn, &s.client.cache(), None).await.unwrap();
    s.client.sandbox().write("author/upper/state.bin", "evil");
    s.client
        .sandbox()
        .write("author/Mod/meta/config.toml", "name = \"evil\"\n");
    s.client.sandbox().write("author/new.txt", "new\n");
    author.reconcile(&s.conn).await.unwrap();
    author
        .submit(
            &s.conn,
            &s.client.cache(),
            LocalClId::DEFAULT,
            Some("sneaky".into()),
        )
        .await
        .unwrap();

    let other = TestClient::new();
    let mut ws = other.create_workspace(&s.server, "ws", &stream());
    let report = ws.sync(&s.conn, &other.cache(), None).await.unwrap();
    assert_eq!(report.updated, [path("new.txt"), path("ok.txt")]);
    let mut skipped = report.skipped.clone();
    skipped.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        skipped,
        [
            (path(".KELP/state.bin"), SkipReason::Untracked),
            (path("Mod/.kelp/config.toml"), SkipReason::Untracked),
        ]
    );
    assert!(!other.sandbox().exists("ws/Mod/.kelp"));
    assert_ne!(other.sandbox().read("ws/.kelp/state.bin"), b"evil");
    let root = other.workspace_root("ws");
    Workspace::open(&root).expect("the workspace state is intact");
    let status = ws.status().unwrap();
    assert!(!status.has_unopened_changes(), "{status:?}");

    // Later syncs keep refusing them.
    let report = ws.sync(&s.conn, &other.cache(), None).await.unwrap();
    assert_eq!(report.skipped.len(), 2);
}
