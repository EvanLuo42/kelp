//! Projected workspaces through kelpd; skipped where FUSE is unavailable.

#![cfg(target_os = "linux")]

use std::path::Path;

use kelp_client::Workspace;
use kelp_client::ipc::call;
use kelp_client::ops::{Op, Outcome};
use kelp_core::LocalClId;
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::{Config, OpenAction};

fn cached_chunks(client: &TestClient) -> usize {
    let dir = client.home().dir().join("cache/chunks");
    walk_count(&dir)
}

fn walk_count(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .map(|e| e.unwrap().path())
            .map(|p| if p.is_dir() { walk_count(&p) } else { 1 })
            .sum()
    })
}

async fn op(client: &TestClient, op: Op) -> Outcome {
    call(client.home(), &client.workspace_root("ws"), op)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn projected_workspaces_download_on_first_use() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let stream = depot_path("//game/main");

    // Alice submits from an ordinary workspace.
    let alice = TestClient::new();
    server.add_user("alice").await;
    alice.login(&server, "alice").await;
    alice.create_workspace(&server, "ws", &stream);
    alice.start_daemon().await;
    alice.sandbox().write(
        "ws/Content/Hero.png",
        kelp_testkit::pseudo_random(200_000, 3),
    );
    alice.sandbox().write("ws/Source/main.rs", "fn main() {}\n");
    alice.sandbox().write("ws/readme.txt", "hello\n");
    op(&alice, Op::Reconcile).await;
    op(
        &alice,
        Op::Submit {
            cl: LocalClId::DEFAULT,
            message: Some("base".into()),
            fixes: Vec::new(),
        },
    )
    .await;

    // Bob mounts a projected workspace.
    let bob = TestClient::new();
    server.add_user("bob").await;
    bob.login(&server, "bob").await;
    bob.start_daemon().await;
    let upper = bob.home().dir().join("vfs/ws");
    let mut config = Config::new("ws", server.addr(), &stream);
    config.projected = true;
    Workspace::create(&upper, config).unwrap();
    let root = bob.sandbox().dir("ws");
    if let Err(e) = call(
        bob.home(),
        &root,
        Op::Mount {
            upper: upper.clone(),
        },
    )
    .await
    {
        eprintln!("FUSE unavailable ({e}); skipping");
        return;
    }

    let Outcome::Synced(report) = op(&bob, Op::Sync { at: None }).await else {
        panic!()
    };
    assert_eq!(report.updated.len(), 3);
    assert_eq!(
        std::fs::metadata(root.join("Content/Hero.png"))
            .unwrap()
            .len(),
        200_000
    );
    assert_eq!(cached_chunks(&bob), 0, "sync downloads nothing");
    let modified = std::fs::metadata(root.join("readme.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    assert!(age.as_secs() < 60, "synced files look new to build tools");
    let Outcome::Status { status, .. } = op(&bob, Op::Status).await else {
        panic!()
    };
    assert!(!status.has_unopened_changes(), "{status:?}");
    assert_eq!(cached_chunks(&bob), 0, "status downloads nothing");

    assert_eq!(
        std::fs::read_to_string(root.join("Source/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert_eq!(cached_chunks(&bob), 1, "reading downloads only that file");

    let edit = Op::Open {
        paths: vec![root.join("readme.txt")],
        action: OpenAction::Edit,
        cl: LocalClId::DEFAULT,
    };
    op(&bob, edit).await;
    std::fs::write(
        root.join("readme.txt"),
        "hello from a projected workspace\n",
    )
    .unwrap();
    std::fs::remove_file(root.join("Source/main.rs")).unwrap();
    let Outcome::Status { status, .. } = op(&bob, Op::Status).await else {
        panic!()
    };
    assert_eq!(
        status.deleted,
        vec![depot_path("//game/main/Source/main.rs")]
    );
    op(&bob, Op::Reconcile).await;
    let Outcome::Submitted(cl) = op(
        &bob,
        Op::Submit {
            cl: LocalClId::DEFAULT,
            message: Some("bob".into()),
            fixes: Vec::new(),
        },
    )
    .await
    else {
        panic!()
    };
    assert_eq!(cl, 2);

    let Outcome::Synced(_) = op(&alice, Op::Sync { at: None }).await else {
        panic!()
    };
    assert_eq!(
        alice.sandbox().read("ws/readme.txt"),
        b"hello from a projected workspace\n"
    );
    assert!(!alice.sandbox().exists("ws/Source/main.rs"));

    // A newer revision from Alice appears in Bob's workspace without downloading.
    let edit = Op::Open {
        paths: vec![alice.workspace_root("ws").join("Content/Hero.png")],
        action: OpenAction::Edit,
        cl: LocalClId::DEFAULT,
    };
    op(&alice, edit).await;
    alice.sandbox().write(
        "ws/Content/Hero.png",
        kelp_testkit::pseudo_random(300_000, 4),
    );
    op(
        &alice,
        Op::Submit {
            cl: LocalClId::DEFAULT,
            message: Some("bigger hero".into()),
            fixes: Vec::new(),
        },
    )
    .await;
    let before = cached_chunks(&bob);
    op(&bob, Op::Sync { at: None }).await;
    assert_eq!(
        std::fs::metadata(root.join("Content/Hero.png"))
            .unwrap()
            .len(),
        300_000
    );
    assert_eq!(cached_chunks(&bob), before);

    // Unmounting and mounting again keeps the workspace.
    op(&bob, Op::Unmount).await;
    assert!(std::fs::read_dir(&root).unwrap().next().is_none());
    call(bob.home(), &root, Op::Mount { upper }).await.unwrap();
    assert!(!root.join("Source/main.rs").exists());
    assert_eq!(
        std::fs::read(root.join("Content/Hero.png")).unwrap(),
        kelp_testkit::pseudo_random(300_000, 4)
    );
    op(&bob, Op::Unmount).await;
}
