use std::collections::BTreeMap;

use kelp_client::artifact::{self, ArtifactQuery};
use kelp_client::{Error, Workspace, is_read_only};
use kelp_core::LocalClId;
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_testkit::net::{TestClient, TestServer};
use kelp_testkit::{depot_path, pseudo_random};
use kelp_workspace::{ArtifactMount, Config};

fn tags(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test]
async fn sync_places_the_artifact_built_from_the_synced_changelist() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let stream = depot_path("//game/main");
    let (alice_conn, bob_conn) = (server.add_user("alice").await, server.add_user("bob").await);
    let (alice, bob) = (TestClient::new(), TestClient::new());

    let mut ws = alice.create_workspace(&server, "ws", &stream);
    let mut cls = Vec::new();
    for i in 0..3 {
        alice
            .sandbox()
            .write("ws/Source/main.rs", format!("// {i}\n"));
        if i == 0 {
            ws.reconcile(&alice_conn).await.unwrap();
        } else {
            let path = depot_path("//game/main/Source/main.rs");
            ws.open_files(
                &alice_conn,
                &[path],
                kelp_workspace::OpenAction::Edit,
                LocalClId::DEFAULT,
            )
            .await
            .unwrap();
        }
        let cl = ws
            .submit(
                &alice_conn,
                &alice.cache(),
                LocalClId::DEFAULT,
                Some(format!("change {i}")),
            )
            .await
            .unwrap();
        cls.push(cl);
    }

    let linux = tags(&[("platform", "linux")]);
    alice
        .sandbox()
        .write("build1/bin/editor", pseudo_random(200_000, 1));
    alice.sandbox().write("build1/bin/old.so", "old");
    alice
        .sandbox()
        .write("build1/data.pak", pseudo_random(50_000, 2));
    let build1 = artifact::publish(
        &alice_conn,
        &alice.cache(),
        &stream,
        cls[0],
        "editor",
        &linux,
        &alice.sandbox().path("build1"),
    )
    .await
    .unwrap();
    alice
        .sandbox()
        .write("build2/bin/editor", pseudo_random(200_000, 1));
    alice
        .sandbox()
        .write("build2/data.pak", pseudo_random(50_000, 3));
    let build2 = artifact::publish(
        &alice_conn,
        &alice.cache(),
        &stream,
        cls[1],
        "editor",
        &linux,
        &alice.sandbox().path("build2"),
    )
    .await
    .unwrap();
    alice.sandbox().write("windows/bin/editor.exe", "win");
    artifact::publish(
        &alice_conn,
        &alice.cache(),
        &stream,
        cls[2],
        "editor",
        &tags(&[("platform", "win64")]),
        &alice.sandbox().path("windows"),
    )
    .await
    .unwrap();

    let mut config = Config::new("ws", server.addr(), &stream);
    config.artifacts.push(ArtifactMount {
        name: "editor".into(),
        to: "Binaries".into(),
        stream: None,
        exact: false,
        tags: linux.clone(),
    });
    config.artifacts.push(ArtifactMount {
        name: "editor".into(),
        to: "Exact".into(),
        stream: None,
        exact: true,
        tags: linux.clone(),
    });
    let mut ws_b = Workspace::create(&bob.sandbox().dir("ws"), config).unwrap();

    let report = ws_b
        .sync(&bob_conn, &bob.cache(), Some(cls[0]))
        .await
        .unwrap();
    let placed: Vec<_> = report
        .artifacts
        .iter()
        .map(|a| (a.to.as_str(), a.now))
        .collect();
    assert_eq!(
        placed,
        [
            ("Binaries", Some((build1, cls[0]))),
            ("Exact", Some((build1, cls[0])))
        ]
    );
    assert_eq!(bob.sandbox().read("ws/Binaries/bin/old.so"), b"old");
    assert!(is_read_only(&bob.sandbox().path("ws/Binaries/data.pak")).unwrap());
    let status = ws_b.status().unwrap();
    assert!(
        !status.has_unopened_changes(),
        "artifact directories are not workspace files: {status:?}"
    );

    let report = ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();
    let placed: Vec<_> = report
        .artifacts
        .iter()
        .map(|a| (a.to.as_str(), a.now))
        .collect();
    assert_eq!(
        placed,
        [("Binaries", Some((build2, cls[1]))), ("Exact", None)],
        "the newest linux build at or below the head, and none built from exactly the head"
    );
    assert!(!bob.sandbox().exists("ws/Binaries/bin/old.so"));
    assert!(!bob.sandbox().exists("ws/Exact"));
    assert_eq!(
        bob.sandbox().read("ws/Binaries/data.pak"),
        pseudo_random(50_000, 3)
    );

    let report = ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();
    assert!(
        report.artifacts.is_empty(),
        "nothing changes on a second sync"
    );

    let fetched = artifact::fetch(&bob_conn, &bob.cache(), build1, &bob.sandbox().dir("out"))
        .await
        .unwrap();
    assert_eq!(fetched.len(), 3);
    assert_eq!(
        bob.sandbox().read("out/bin/editor"),
        pseudo_random(200_000, 1)
    );
}

#[tokio::test]
async fn retention_and_pins_are_for_administrators() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let stream = depot_path("//game/main");
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &stream);
    client.sandbox().write("ws/a.txt", "a");
    ws.reconcile(&conn).await.unwrap();
    let cl = ws
        .submit(&conn, &client.cache(), LocalClId::DEFAULT, Some("a".into()))
        .await
        .unwrap();

    let mut ids = Vec::new();
    for i in 0..3 {
        client
            .sandbox()
            .write(&format!("build{i}/out.bin"), format!("{i}"));
        let dir = client.sandbox().path(&format!("build{i}"));
        ids.push(
            artifact::publish(
                &conn,
                &client.cache(),
                &stream,
                cl,
                "cooked",
                &BTreeMap::new(),
                &dir,
            )
            .await
            .unwrap(),
        );
    }

    let forbidden = conn
        .call(&Request::PinArtifact {
            id: ids[0],
            pinned: true,
        })
        .await;
    assert!(matches!(forbidden, Err(Error::Remote(ref e)) if e.kind == ErrorKind::Forbidden));
    server
        .admin(Request::PinArtifact {
            id: ids[0],
            pinned: true,
        })
        .await;
    server
        .admin(Request::SetArtifactRetention {
            stream: stream.to_string(),
            name: "cooked".into(),
            keep_last: Some(1),
        })
        .await;

    let query = ArtifactQuery {
        name: "cooked".into(),
        ..ArtifactQuery::default()
    };
    let left: Vec<_> = artifact::list(&conn, &stream, &query, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|a| (a.id, a.pinned))
        .collect();
    assert_eq!(left, [(ids[2], false), (ids[0], true)]);

    let missing = artifact::files(&conn, ids[1]).await;
    assert!(matches!(missing, Err(Error::Remote(ref e)) if e.kind == ErrorKind::NotFound));

    server.clock().advance(2 * 3600 * 1000);
    let Reply::Gc(report) = server.admin(Request::Gc { dry_run: false }).await else {
        panic!("unexpected reply");
    };
    assert_eq!(
        report.chunks_deleted, 1,
        "only the deleted artifact's file is collected"
    );
    let other = TestClient::new();
    artifact::fetch(
        &conn,
        &other.cache(),
        ids[0],
        &other.sandbox().dir("pinned"),
    )
    .await
    .unwrap();
    assert_eq!(other.sandbox().read("pinned/out.bin"), b"0");
}
