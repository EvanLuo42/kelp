use kelp_meta::{EntryKind, Error, StreamKind};
use kelp_testkit::depot_path as p;
use kelp_testkit::meta::MetaFixture;

#[test]
fn a_file_can_become_a_directory_in_one_change() {
    let f = MetaFixture::new();
    f.submit("//game/main", vec![f.add("//game/main/x", b"file")])
        .unwrap();
    f.submit(
        "//game/main",
        vec![
            f.delete("//game/main/x", b"file"),
            f.add("//game/main/x/y", b"inside"),
        ],
    )
    .unwrap();
    let r = f.db().read().unwrap();
    let stream = r.stream(&p("//game/main")).unwrap().unwrap();
    let nodes = r.nodes().unwrap();
    let entry = kelp_meta::Dirs::new(&nodes)
        .lookup(stream.root, "x")
        .unwrap()
        .unwrap();
    assert!(matches!(entry.kind, EntryKind::Dir { .. }));
}

#[test]
fn a_directory_can_become_a_file_in_one_change() {
    let f = MetaFixture::new();
    f.submit("//game/main", vec![f.add("//game/main/x/y", b"inside")])
        .unwrap();
    let still_full = f.submit("//game/main", vec![f.add("//game/main/x", b"file")]);
    assert!(still_full.is_err(), "x still has y");
    f.submit(
        "//game/main",
        vec![
            f.delete("//game/main/x/y", b"inside"),
            f.add("//game/main/x", b"file"),
        ],
    )
    .unwrap();
}

#[test]
fn obliterate_reaches_streams_branched_from_the_stream() {
    let f = MetaFixture::new();
    f.submit("//game/main", vec![f.add("//game/main/secret.txt", b"key")])
        .unwrap();
    f.create_stream("//game/dev", StreamKind::Development, "//game/main");
    f.create_stream("//game/task", StreamKind::Task, "//game/dev");
    f.db()
        .commit(kelp_meta::Command::Obliterate {
            paths: vec!["//game/main/secret.txt".into()],
        })
        .unwrap();
    let r = f.db().read().unwrap();
    let nodes = r.nodes().unwrap();
    for stream in ["//game/main", "//game/dev", "//game/task"] {
        let record = r.stream(&p(stream)).unwrap().unwrap();
        for root in [record.root, record.base_root] {
            assert!(
                kelp_meta::Dirs::new(&nodes)
                    .lookup(root, "secret.txt")
                    .unwrap()
                    .is_none(),
                "{stream} still has it"
            );
        }
    }
}

#[test]
fn a_merge_down_without_files_records_what_the_child_already_has() {
    let f = MetaFixture::new();
    f.submit("//game/main", vec![f.add("//game/main/a.txt", b"v1")])
        .unwrap();
    f.create_stream("//game/dev", StreamKind::Development, "//game/main");
    let main_cl = f
        .submit(
            "//game/main",
            vec![f.edit("//game/main/a.txt", b"v1", b"v2")],
        )
        .unwrap();
    let merge = |f: &MetaFixture| {
        f.submit_as(
            "//game/dev",
            "alice",
            "ws",
            Vec::new(),
            Some(("//game/main", main_cl.0)),
        )
    };
    let err = merge(&f).unwrap_err();
    assert!(
        matches!(err, Error::Conflict(ref m) if m[0].contains("lacks")),
        "{err:?}"
    );

    f.submit("//game/dev", vec![f.edit("//game/dev/a.txt", b"v1", b"v2")])
        .unwrap();
    merge(&f).unwrap();
    let r = f.db().read().unwrap();
    assert_eq!(
        r.integration(&p("//game/dev"), &p("//game/main")).unwrap(),
        Some(main_cl)
    );
    drop(r);
    assert!(
        f.submit("//game/main", Vec::new()).is_err(),
        "other submits still need files"
    );
}

#[test]
fn history_follows_a_moved_file() {
    use kelp_meta::db::Proposal;
    let f = MetaFixture::new();
    f.submit("//game/main", vec![f.add("//game/main/a.txt", b"v1")])
        .unwrap();
    f.submit(
        "//game/main",
        vec![f.edit("//game/main/a.txt", b"v1", b"v2")],
    )
    .unwrap();
    let submission = kelp_meta::Submission {
        stream: p("//game/main"),
        author: "alice".into(),
        workspace: "ws".into(),
        description: "rename".into(),
        changes: vec![
            f.delete("//game/main/a.txt", b"v2"),
            f.add("//game/main/b.txt", b"v2"),
        ],
        integration: None,
        issues: Vec::new(),
        fixes: Vec::new(),
        review: None,
        at: 3,
    };
    let moves = vec![(p("//game/main/a.txt"), p("//game/main/b.txt"))];
    f.db()
        .propose(&Proposal::SubmitMoves { submission, moves })
        .unwrap();
    let history = f
        .db()
        .read()
        .unwrap()
        .history(&p("//game/main/b.txt"))
        .unwrap();
    let seen: Vec<(String, u64)> = history
        .iter()
        .map(|r| (r.path.to_string(), r.cl.0))
        .collect();
    assert_eq!(
        seen,
        [
            ("//game/main/b.txt".to_owned(), 3),
            ("//game/main/a.txt".to_owned(), 2),
            ("//game/main/a.txt".to_owned(), 1),
        ]
    );
}
