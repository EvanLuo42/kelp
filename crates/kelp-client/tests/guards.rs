//! Regression tests for workspace files that Kelp must not misjudge or
//! overwrite.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kelp_client::{Connection, DiskIndex, SkipReason, Workspace};
use kelp_core::{ContentId, DepotPath, LocalClId};
use kelp_testkit::net::{TestClient, TestServer};
use kelp_testkit::{Sandbox, depot_path};
use kelp_workspace::{Config, HaveEntry, OpenAction, State};

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

fn set_mtime(path: &Path, at: SystemTime) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(at).unwrap();
}

fn nanos(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
}

/// A workspace without a server that has `a.txt` with `v1\n`, recorded with
/// the modification time `mtime`, and holds `disk` in it, modified at
/// `disk_mtime`. The state is saved at `saved`.
fn offline(
    sandbox: &Sandbox,
    mtime: u64,
    disk: &str,
    disk_mtime: SystemTime,
    saved: SystemTime,
) -> PathBuf {
    let root = sandbox.dir("ws");
    Workspace::create(&root, Config::new("ws", "localhost:7474", &stream())).unwrap();
    let mut state = State::load(&root).unwrap();
    state.have.insert(
        path("a.txt"),
        HaveEntry {
            local: "a.txt".into(),
            content: ContentId::of(b"v1\n"),
            size: 3,
            executable: false,
            mtime,
        },
    );
    state.save(&root).unwrap();
    set_mtime(&root.join(".kelp/state.bin"), saved);
    let file = sandbox.write("ws/a.txt", disk);
    set_mtime(&file, disk_mtime);
    root
}

#[test]
fn a_time_recorded_in_the_tick_of_the_save_is_not_trusted() {
    // Not whole seconds, and long enough ago that no wall clock rule fires.
    let tick = SystemTime::now() - Duration::from_secs(60) + Duration::from_nanos(123_400);
    let sandbox = Sandbox::new();
    // The file changed in the same tick in which its time was recorded and
    // saved, so the time cannot tell.
    let root = offline(&sandbox, nanos(tick), "v2\n", tick, tick);
    let mut ws = Workspace::open(&root).unwrap();
    assert_eq!(ws.status().unwrap().modified, [path("a.txt")]);

    // Once hashing finds it unchanged and the state is saved later, the time
    // is trusted.
    let file = sandbox.write("ws/a.txt", "v1\n");
    set_mtime(&file, tick);
    assert!(!ws.refresh_status(None).unwrap().has_unopened_changes());
    let saved = State::saved_at(&root).unwrap();
    assert!(saved > nanos(tick));
    assert_eq!(
        State::load(&root).unwrap().have[&path("a.txt")].mtime,
        nanos(tick)
    );
    sandbox.write("ws/a.txt", "v3\n");
    set_mtime(&file, tick);
    assert!(
        Workspace::open(&root).unwrap().status().unwrap().modified.is_empty(),
        "a trusted time is not hashed again"
    );
}

#[test]
fn an_unknown_time_never_matches_an_unreadable_one() {
    let sandbox = Sandbox::new();
    let root = offline(&sandbox, 0, "v2\n", UNIX_EPOCH, SystemTime::now());
    let ws = Workspace::open(&root).unwrap();
    assert_eq!(ws.status().unwrap().modified, [path("a.txt")]);
}

#[test]
fn refreshing_the_index_skips_artifact_directories() {
    let sandbox = Sandbox::new();
    let mut config = Config::new("ws", "localhost:7474", &stream());
    config.artifacts.push(kelp_workspace::ArtifactMount {
        name: "tools".into(),
        to: "Tools".into(),
        stream: None,
        exact: false,
        tags: Default::default(),
    });
    let ws = Workspace::create(&sandbox.dir("ws"), config).unwrap();
    let mut index = DiskIndex::scan(&ws).unwrap();
    sandbox.write("ws/Tools/tool.exe", "built\n");
    sandbox.write("ws/notes.txt", "mine\n");
    // A watcher reports paths below the canonical root.
    index.refresh(&ws, &ws.root().join("Tools/tool.exe")).unwrap();
    index.refresh(&ws, &ws.root().join("notes.txt")).unwrap();
    let listed: Vec<&str> = index.entries().map(|(l, _)| l).collect();
    assert_eq!(listed, ["notes.txt"]);
    assert_eq!(ws.status_indexed(&index).unwrap().added, [path("notes.txt")]);
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

#[tokio::test]
async fn reverting_a_delete_snapshots_a_file_created_again() {
    let mut s = setup(&[("a.txt", "a\n")]).await;
    s.ws.open_files(
        &s.conn,
        &[path("a.txt")],
        OpenAction::Delete,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    assert!(!s.client.sandbox().exists("ws/a.txt"));
    s.client.sandbox().write("ws/a.txt", "written again\n");
    s.ws.revert(&s.conn, &s.client.cache(), &[path("a.txt")])
        .await
        .unwrap();
    assert_eq!(s.client.sandbox().read("ws/a.txt"), b"a\n");

    let snapshot = s.ws.state().snapshots.last().expect("a snapshot was taken");
    assert!(snapshot.automatic);
    let file = &snapshot.files[0];
    assert_eq!(
        (file.path.clone(), file.action),
        (path("a.txt"), OpenAction::Delete)
    );
    let saved = file.content.expect("the file on disk was saved");
    assert_eq!(
        s.client.cache().read(&saved.content).await.unwrap(),
        b"written again\n"
    );

    // Restoring it brings back the delete and the file.
    let id = snapshot.id;
    s.ws.restore(&s.conn, &s.client.cache(), id).await.unwrap();
    assert_eq!(
        s.ws.state().pending.action(&path("a.txt")),
        Some(OpenAction::Delete)
    );
    assert_eq!(s.client.sandbox().read("ws/a.txt"), b"written again\n");
}

#[tokio::test]
async fn tracked_files_that_become_ignored_are_not_deleted() {
    let mut s = setup(&[("a.txt", "a\n"), ("Saved/log.txt", "log\n")]).await;
    s.client.sandbox().write("ws/.kelpignore", "Saved/\n");
    let status = s.ws.status().unwrap();
    assert!(status.deleted.is_empty(), "{status:?}");
    assert_eq!(status.added, [path(".kelpignore")]);

    let opened = s.ws.reconcile(&s.conn).await.unwrap();
    assert_eq!(opened, [(path(".kelpignore"), OpenAction::Add)]);
    assert!(s.client.sandbox().exists("ws/Saved/log.txt"));

    // They are still compared with what was synced.
    s.client.sandbox().write("ws/Saved/log.txt", "new log\n");
    assert_eq!(s.ws.status().unwrap().modified, [path("Saved/log.txt")]);
    let report =
        s.ws.clean(&s.conn, &s.client.cache(), &[], true)
            .await
            .unwrap();
    assert_eq!(report.restored, [path("Saved/log.txt")]);
    assert!(report.removed.is_empty());
}
