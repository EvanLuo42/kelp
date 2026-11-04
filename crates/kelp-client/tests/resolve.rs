use kelp_client::{ResolveMode, ResolveOutcome, SkipReason, Workspace};
use kelp_core::{DepotPath, LocalClId};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

const BASE: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";

struct Pair {
    server: TestServer,
    alice: (TestClient, Workspace, kelp_client::Connection),
    bob: (TestClient, Workspace, kelp_client::Connection),
}

/// Alice adds `notes.txt` with `content`; both workspaces then open it.
async fn both_editing(content: &[u8]) -> (Pair, DepotPath) {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let stream = depot_path("//game/main");
    let path = depot_path("//game/main/notes.txt");
    let (a, b) = (TestClient::new(), TestClient::new());
    let (ca, cb) = (server.add_user("alice").await, server.add_user("bob").await);
    let (mut wa, mut wb) = (
        a.create_workspace(&server, "ws", &stream),
        b.create_workspace(&server, "ws", &stream),
    );
    a.sandbox().write("ws/notes.txt", content);
    wa.reconcile(&ca).await.unwrap();
    wa.submit(&ca, &a.cache(), LocalClId::DEFAULT, Some("base".into()))
        .await
        .unwrap();
    wb.sync(&cb, &b.cache(), None).await.unwrap();
    for (ws, conn) in [(&mut wa, &ca), (&mut wb, &cb)] {
        ws.open_files(
            conn,
            std::slice::from_ref(&path),
            OpenAction::Edit,
            LocalClId::DEFAULT,
        )
        .await
        .unwrap();
    }
    (
        Pair {
            server,
            alice: (a, wa, ca),
            bob: (b, wb, cb),
        },
        path,
    )
}

/// Alice submits `theirs`, Bob writes `yours` and syncs.
async fn diverge(pair: &mut Pair, path: &DepotPath, theirs: &[u8], yours: &[u8]) {
    let (a, wa, ca) = &mut pair.alice;
    a.sandbox().write("ws/notes.txt", theirs);
    wa.submit(ca, &a.cache(), LocalClId::DEFAULT, Some("alice".into()))
        .await
        .unwrap();
    let (b, wb, cb) = &mut pair.bob;
    b.sandbox().write("ws/notes.txt", yours);
    let report = wb.sync(cb, &b.cache(), None).await.unwrap();
    assert_eq!(report.skipped, vec![(path.clone(), SkipReason::Opened)]);
    assert_eq!(wb.status().unwrap().unresolved, vec![path.clone()]);
    let err = wb
        .submit(cb, &b.cache(), LocalClId::DEFAULT, Some("too early".into()))
        .await;
    assert!(err.is_err(), "unresolved files cannot be submitted");
}

#[tokio::test]
async fn non_overlapping_edits_merge_automatically() {
    let (mut pair, path) = both_editing(BASE.as_bytes()).await;
    let theirs = BASE.replace("one", "ONE");
    let yours = BASE.replace("seven", "SEVEN");
    diverge(&mut pair, &path, theirs.as_bytes(), yours.as_bytes()).await;

    let (b, wb, cb) = &mut pair.bob;
    let outcome = wb
        .resolve(cb, &b.cache(), &[], ResolveMode::Auto)
        .await
        .unwrap();
    assert_eq!(outcome, vec![(path.clone(), ResolveOutcome::Merged)]);
    let merged = BASE.replace("one", "ONE").replace("seven", "SEVEN");
    assert_eq!(b.sandbox().read("ws/notes.txt"), merged.as_bytes());
    wb.submit(cb, &b.cache(), LocalClId::DEFAULT, Some("bob".into()))
        .await
        .unwrap();
    drop(pair.server);
}

#[tokio::test]
async fn overlapping_edits_leave_markers_until_accepted() {
    let (mut pair, path) = both_editing(BASE.as_bytes()).await;
    diverge(
        &mut pair,
        &path,
        BASE.replace("four", "alice").as_bytes(),
        BASE.replace("four", "bob").as_bytes(),
    )
    .await;

    let (b, wb, cb) = &mut pair.bob;
    let outcome = wb
        .resolve(cb, &b.cache(), &[], ResolveMode::Auto)
        .await
        .unwrap();
    assert_eq!(outcome, vec![(path.clone(), ResolveOutcome::Conflicted)]);
    let text = String::from_utf8(b.sandbox().read("ws/notes.txt")).unwrap();
    assert!(
        text.contains("<<<<<<<") && text.contains("alice") && text.contains("bob"),
        "{text}"
    );

    b.sandbox()
        .write("ws/notes.txt", BASE.replace("four", "alice and bob"));
    let outcome = wb
        .resolve(cb, &b.cache(), &[], ResolveMode::AcceptYours)
        .await
        .unwrap();
    assert_eq!(outcome, vec![(path, ResolveOutcome::TookYours)]);
    wb.submit(cb, &b.cache(), LocalClId::DEFAULT, Some("both".into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_merge_tool_gets_all_three_versions_before_markers_overwrite_yours() {
    let (mut pair, path) = both_editing(BASE.as_bytes()).await;
    let (theirs, yours) = (BASE.replace("four", "alice"), BASE.replace("four", "bob"));
    diverge(&mut pair, &path, theirs.as_bytes(), yours.as_bytes()).await;

    let (b, wb, cb) = &mut pair.bob;
    let inputs = wb.merge_inputs(cb, &b.cache(), &[]).await.unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].path, path);
    // Windows may name the same file with a `\\?\` prefix.
    assert_eq!(
        std::fs::canonicalize(&inputs[0].local).unwrap(),
        std::fs::canonicalize(b.sandbox().path("ws/notes.txt")).unwrap()
    );
    assert_eq!(inputs[0].base.as_deref(), Some(BASE.as_bytes()));
    assert_eq!(inputs[0].yours.as_deref(), Some(yours.as_bytes()));
    assert_eq!(inputs[0].theirs.as_deref(), Some(theirs.as_bytes()));

    // What the tool leaves in the file is what `--accept yours` takes.
    b.sandbox()
        .write("ws/notes.txt", BASE.replace("four", "alice and bob"));
    let outcome = wb
        .resolve(cb, &b.cache(), &[], ResolveMode::AcceptYours)
        .await
        .unwrap();
    assert_eq!(outcome, vec![(path.clone(), ResolveOutcome::TookYours)]);
    assert!(wb.merge_inputs(cb, &b.cache(), &[path]).await.is_err());
}

#[tokio::test]
async fn binary_files_need_an_explicit_choice() {
    let (mut pair, path) = both_editing(&[0, 1, 2, 255]).await;
    diverge(&mut pair, &path, &[0, 9, 9, 255], &[0, 7, 7, 255]).await;

    let (b, wb, cb) = &mut pair.bob;
    let outcome = wb
        .resolve(cb, &b.cache(), &[], ResolveMode::Auto)
        .await
        .unwrap();
    assert_eq!(outcome, vec![(path.clone(), ResolveOutcome::NeedsChoice)]);
    let outcome = wb
        .resolve(cb, &b.cache(), &[], ResolveMode::AcceptTheirs)
        .await
        .unwrap();
    assert_eq!(outcome, vec![(path, ResolveOutcome::TookTheirs)]);
    assert_eq!(b.sandbox().read("ws/notes.txt"), [0, 9, 9, 255]);
    wb.submit(
        cb,
        &b.cache(),
        LocalClId::DEFAULT,
        Some("took theirs".into()),
    )
    .await
    .unwrap();
}
