//! Listing depots, streams and directories from the server.

use kelp_client::{Connection, Error, list_depot, render_tree};
use kelp_core::{DepotPath, LocalClId};
use kelp_proto::Request;
use kelp_proto::rpc::{DirEntry, DirEntryKind, ListOptions};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};

/// Starts a server where alice submitted two changelists to `//game/main`,
/// and bob may read neither `Secret` nor `Mixed/hidden`.
async fn depot() -> (TestServer, TestClient, Connection, Connection) {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    server.create_depot("tools").await;
    let (alice, bob) = (server.add_user("alice").await, server.add_user("bob").await);
    let table = "write * //...\n\
                 none user:bob //game/main/Secret/...\n\
                 none user:bob //game/main/Mixed/hidden/...\n\
                 none user:bob //game/main/Mixed/plan.txt\n";
    server
        .admin(Request::SetProtections {
            source: table.into(),
        })
        .await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    for (path, text) in [
        ("ws/Art/Maps/one.map", "1"),
        ("ws/Art/tree.png", "tree"),
        ("ws/Secret/plan.txt", "plan"),
        ("ws/Mixed/hidden/a.txt", "a"),
        ("ws/Mixed/plan.txt", "plan"),
        ("ws/Mixed/shown/b.txt", "b"),
        ("ws/readme.md", "hi"),
    ] {
        client.sandbox().write(path, text);
    }
    ws.reconcile(&alice).await.unwrap();
    ws.submit(
        &alice,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("first".into()),
    )
    .await
    .unwrap();
    client.sandbox().write("ws/Art/added.png", "new");
    ws.reconcile(&alice).await.unwrap();
    ws.submit(
        &alice,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("second".into()),
    )
    .await
    .unwrap();
    (server, client, alice, bob)
}

async fn paths(
    conn: &Connection,
    path: Option<&str>,
    at: Option<u64>,
    depth: Option<u32>,
) -> Vec<String> {
    let path = path.map(depot_path);
    let options = ListOptions {
        at,
        depth,
        ..ListOptions::default()
    };
    list_depot(conn, path.as_ref(), options)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect()
}

#[tokio::test]
async fn lists_one_level_or_all_of_them() {
    let (_server, _client, alice, _bob) = depot().await;
    assert_eq!(
        paths(&alice, Some("//game/main"), None, Some(1)).await,
        ["Art", "Mixed", "readme.md", "Secret"]
    );
    assert_eq!(
        paths(&alice, Some("//game/main/Art"), None, None).await,
        ["added.png", "Maps", "Maps/one.map", "tree.png"]
    );
    assert_eq!(
        paths(&alice, Some("//game/main/Art"), None, Some(1)).await,
        ["added.png", "Maps", "tree.png"]
    );
}

#[tokio::test]
async fn paths_ignore_case_and_a_file_lists_itself() {
    let (_server, _client, alice, _bob) = depot().await;
    assert_eq!(
        paths(&alice, Some("//GAME/main/art/MAPS"), None, None).await,
        ["one.map"]
    );
    assert_eq!(
        paths(&alice, Some("//game/main/art/TREE.PNG"), None, None).await,
        ["tree.png"]
    );
}

#[tokio::test]
async fn lists_as_of_a_changelist() {
    let (_server, _client, alice, _bob) = depot().await;
    let head = alice.resolve_stream("//game/main", None).await.unwrap();
    assert_eq!(
        paths(&alice, Some("//game/main/Art"), Some(head.cl - 1), Some(1)).await,
        ["Maps", "tree.png"]
    );
}

#[tokio::test]
async fn missing_paths_are_errors() {
    let (_server, _client, alice, _bob) = depot().await;
    for path in ["//game/main/nope", "//game/main/readme.md/x", "//game/nope"] {
        let result = list_depot(&alice, Some(&depot_path(path)), ListOptions::default()).await;
        assert!(result.is_err(), "{path}");
    }
    let options = ListOptions {
        at: Some(1),
        ..ListOptions::default()
    };
    let above = list_depot(&alice, Some(&depot_path("//game")), options).await;
    assert!(matches!(above, Err(Error::Invalid(_))));
}

#[tokio::test]
async fn above_the_streams_lists_depots_and_streams() {
    let (server, _client, alice, _bob) = depot().await;
    server
        .admin(Request::CreateStream {
            path: "//game/dev".into(),
            kind: kelp_proto::rpc::StreamKind::Development,
            parent: Some("//game/main".into()),
        })
        .await;
    assert_eq!(paths(&alice, None, None, Some(1)).await, ["game", "tools"]);
    assert_eq!(
        paths(&alice, None, None, None).await,
        ["game", "game/dev", "game/main", "tools", "tools/main"]
    );
    assert_eq!(
        paths(&alice, Some("//game"), None, None).await,
        ["dev", "main"]
    );
}

#[tokio::test]
async fn unreadable_entries_are_left_out() {
    let (server, _client, alice, bob) = depot().await;
    assert_eq!(
        paths(&bob, Some("//game/main"), None, Some(1)).await,
        ["Art", "Mixed", "readme.md"],
        "Secret has nothing bob may read"
    );
    assert_eq!(
        paths(&bob, Some("//game/main/Mixed"), None, None).await,
        ["shown", "shown/b.txt"]
    );
    assert_eq!(
        paths(&alice, Some("//game/main/Mixed"), None, Some(1)).await,
        ["hidden", "plan.txt", "shown"]
    );
    for path in [
        "//game/main/Secret",
        "//game/main/Secret/plan.txt",
        "//game/main/Mixed/plan.txt",
    ] {
        let options = ListOptions::default();
        let hidden = list_depot(&bob, Some(&depot_path(path)), options).await;
        let missing = list_depot(&bob, Some(&depot_path("//game/main/nope")), options).await;
        match (hidden, missing) {
            (Err(Error::Remote(hidden)), Err(Error::Remote(missing))) => {
                assert_eq!(hidden.kind, missing.kind, "{path} looks like it is missing")
            }
            other => panic!("expected server errors, got {other:?}"),
        }
    }

    // A rule below a denied directory shows the way down to it.
    let table = "write * //...\n\
                 none user:bob //game/main/Secret/...\n\
                 read user:bob //game/main/Secret/plan.txt\n";
    server
        .admin(Request::SetProtections {
            source: table.into(),
        })
        .await;
    assert_eq!(
        paths(&bob, Some("//game/main/Secret"), None, None).await,
        ["plan.txt"]
    );
}

#[tokio::test]
async fn lists_only_directories() {
    let (_server, _client, alice, _bob) = depot().await;
    let options = ListOptions {
        dirs_only: true,
        ..ListOptions::default()
    };
    let entries = list_depot(&alice, Some(&depot_path("//game/main/Art")), options)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "Maps");
}

#[tokio::test]
async fn details_name_the_revision_that_gave_a_file_its_content() {
    let (server, client, alice, _bob) = depot().await;
    let details = |at| ListOptions {
        at,
        depth: Some(1),
        details: true,
        ..ListOptions::default()
    };
    let last = |entries: Vec<DirEntry>, name: &str| {
        entries
            .into_iter()
            .find(|e| e.path == name)
            .unwrap_or_else(|| panic!("no {name}"))
            .last
    };
    let art = depot_path("//game/main/Art");
    let head = alice.resolve_stream("//game/main", None).await.unwrap();
    let entries = list_depot(&alice, Some(&art), details(None)).await.unwrap();
    assert_eq!(last(entries.clone(), "Maps"), None);
    let added = last(entries.clone(), "added.png").unwrap();
    assert_eq!((added.rev, added.cl), (1, head.cl));
    assert_eq!(added.author, "alice");
    let tree = last(entries, "tree.png").unwrap();
    assert_eq!((tree.rev, tree.cl), (1, head.cl - 1));

    // Without being asked for, they are not looked up.
    let plain = paths(&alice, Some("//game/main/Art"), None, Some(1)).await;
    assert_eq!(plain.len(), 3);
    let options = ListOptions {
        depth: Some(1),
        ..ListOptions::default()
    };
    let entries = list_depot(&alice, Some(&art), options).await.unwrap();
    assert!(entries.iter().all(|e| e.last.is_none()));

    // A branch shows where its inherited files came from, and its own edits.
    server
        .admin(Request::CreateStream {
            path: "//game/dev".into(),
            kind: kelp_proto::rpc::StreamKind::Development,
            parent: Some("//game/main".into()),
        })
        .await;
    let dev_art = depot_path("//game/dev/Art");
    let entries = list_depot(&alice, Some(&dev_art), details(None))
        .await
        .unwrap();
    assert_eq!(last(entries, "tree.png").unwrap().cl, head.cl - 1);
    let mut ws = client.create_workspace(&server, "dev", &depot_path("//game/dev"));
    ws.sync(&alice, &client.cache(), None).await.unwrap();
    client.sandbox().write("dev/Art/tree.png", "dev tree");
    ws.reconcile(&alice).await.unwrap();
    let edit = ws
        .submit(
            &alice,
            &client.cache(),
            LocalClId::DEFAULT,
            Some("edit".into()),
        )
        .await
        .unwrap();
    let entries = list_depot(&alice, Some(&dev_art), details(None))
        .await
        .unwrap();
    assert_eq!(last(entries, "tree.png").unwrap().cl, edit);

    // As of an earlier changelist, the revision is that changelist's.
    let entries = list_depot(&alice, Some(&dev_art), details(Some(edit - 1)))
        .await
        .unwrap();
    assert_eq!(last(entries, "tree.png").unwrap().cl, head.cl - 1);
}

#[tokio::test]
async fn a_workspace_root_is_its_stream() {
    let (_server, client, _alice, _bob) = depot().await;
    let server = TestServer::start().await;
    server.create_depot("other").await;
    let ws = client.create_workspace(&server, "other-ws", &depot_path("//other/main"));
    let root = client.sandbox().path("other-ws");
    let main: DepotPath = depot_path("//other/main");
    assert_eq!(ws.depot_dir_of(&root).unwrap(), main);
    assert_eq!(
        ws.depot_dir_of(&root.join("Art")).unwrap(),
        depot_path("//other/main/Art")
    );
}

#[test]
fn draws_a_tree() {
    let dir = |path: &str| DirEntry {
        path: path.into(),
        kind: DirEntryKind::Dir,
        last: None,
    };
    let link = |path: &str| DirEntry {
        path: path.into(),
        kind: DirEntryKind::Symlink {
            target: "elsewhere".into(),
        },
        last: None,
    };
    let entries = [
        dir("Art"),
        dir("Art/Maps"),
        link("Art/Maps/one"),
        link("Art/tree"),
        dir("Empty"),
        dir("Source"),
        dir("Source/core"),
        link("Source/core/a"),
        link("z"),
    ];
    let expected = "\
├── Art/
│   ├── Maps/
│   │   └── one -> elsewhere
│   └── tree -> elsewhere
├── Empty/
├── Source/
│   └── core/
│       └── a -> elsewhere
└── z -> elsewhere
";
    assert_eq!(render_tree(&entries), expected);
    assert_eq!(render_tree(&[]), "");
}
