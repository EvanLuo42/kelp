use kelp_core::ClNumber;
use kelp_meta::{Error, StreamKind};
use kelp_testkit::depot_path as p;
use kelp_testkit::meta::MetaFixture;

#[test]
fn child_streams_branch_in_constant_time_and_copy_up_after_merging() {
    let f = MetaFixture::new();
    f.submit(
        "//game/main",
        vec![
            f.add("//game/main/a.txt", b"a1"),
            f.add("//game/main/b.txt", b"b1"),
        ],
    )
    .unwrap();
    f.create_stream("//game/dev", StreamKind::Development, "//game/main");

    let r = f.db().read().unwrap();
    let (main, dev) = (
        r.stream(&p("//game/main")).unwrap().unwrap(),
        r.stream(&p("//game/dev")).unwrap().unwrap(),
    );
    assert_eq!(dev.root, main.root);
    assert_eq!(
        r.stream_at(&p("//game/dev"), None).unwrap(),
        (ClNumber(0), main.root)
    );
    assert_eq!(
        r.integration(&p("//game/dev"), &p("//game/main")).unwrap(),
        Some(ClNumber(1))
    );
    drop(r);

    f.submit("//game/dev", vec![f.edit("//game/dev/a.txt", b"a1", b"a2")])
        .unwrap();
    let main_cl = f
        .submit(
            "//game/main",
            vec![f.edit("//game/main/b.txt", b"b1", b"b2")],
        )
        .unwrap();

    // The parent moved on, so copying up would discard b2.
    assert!(matches!(
        f.db()
            .copy_up(&p("//game/dev"), "alice".into(), "copy".into(), 3),
        Err(Error::Conflict(_))
    ));

    f.submit_as(
        "//game/dev",
        "alice",
        "ws",
        vec![f.edit("//game/dev/b.txt", b"b1", b"b2")],
        Some(("//game/main", main_cl.0)),
    )
    .unwrap();
    let stale_merge = f.submit_as(
        "//game/dev",
        "alice",
        "ws",
        vec![f.add("//game/dev/c.txt", b"c")],
        Some(("//game/main", 1)),
    );
    assert!(
        matches!(stale_merge, Err(Error::Conflict(_))),
        "watermarks only move forward"
    );

    let (_, cl, _) = f
        .db()
        .copy_up(&p("//game/dev"), "alice".into(), "copy".into(), 3)
        .unwrap();
    let r = f.db().read().unwrap();
    let record = r.changelist(cl).unwrap().unwrap();
    assert_eq!(
        record.changes.len(),
        1,
        "only the child's own change is copied"
    );
    assert_eq!(
        r.stream(&p("//game/main")).unwrap().unwrap().root,
        r.stream(&p("//game/dev")).unwrap().unwrap().root
    );
    drop(r);
    assert!(matches!(
        f.db()
            .copy_up(&p("//game/dev"), "alice".into(), "again".into(), 4),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn merging_a_child_from_a_parent_workspace_leaves_it_to_merge_down() {
    let f = MetaFixture::new();
    f.submit("//game/main", vec![f.add("//game/main/a.txt", b"a1")])
        .unwrap();
    f.create_stream("//game/dev", StreamKind::Development, "//game/main");
    let dev_cl = f
        .submit("//game/dev", vec![f.add("//game/dev/c.txt", b"c")])
        .unwrap();
    f.submit(
        "//game/main",
        vec![f.edit("//game/main/a.txt", b"a1", b"a2")],
    )
    .unwrap();

    // A workspace of main merges dev and keeps main's a2.
    let merge = f
        .submit_as(
            "//game/main",
            "alice",
            "ws",
            vec![f.add("//game/main/c.txt", b"c")],
            Some(("//game/dev", dev_cl.0)),
        )
        .unwrap();
    let r = f.db().read().unwrap();
    assert_eq!(
        r.integration(&p("//game/main"), &p("//game/dev")).unwrap(),
        Some(dev_cl)
    );
    assert_eq!(
        r.integration(&p("//game/dev"), &p("//game/main")).unwrap(),
        Some(ClNumber(1)),
        "dev still lacks a2, so it has not merged main through {merge}"
    );
    drop(r);
    assert!(
        matches!(
            f.db()
                .copy_up(&p("//game/dev"), "alice".into(), "copy".into(), 3),
            Err(Error::Conflict(_))
        ),
        "copying up would revert a2"
    );
}

#[test]
fn release_streams_do_not_copy_up() {
    let f = MetaFixture::new();
    f.submit("//game/main", vec![f.add("//game/main/a.txt", b"a")])
        .unwrap();
    f.create_stream("//game/rel", StreamKind::Release, "//game/main");
    f.submit("//game/rel", vec![f.edit("//game/rel/a.txt", b"a", b"fix")])
        .unwrap();
    assert!(matches!(
        f.db()
            .copy_up(&p("//game/rel"), "alice".into(), "x".into(), 3),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn stream_creation_rules() {
    let f = MetaFixture::new();
    let create = |path: &str, kind, parent: Option<&str>| {
        f.db().commit(kelp_meta::Command::CreateStream {
            path: p(path),
            kind,
            parent: parent.map(p),
            at: 1,
        })
    };
    assert!(matches!(
        create("//game/dev", StreamKind::Development, None),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        create("//game/x", StreamKind::Mainline, Some("//game/main")),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        create("//game/dev", StreamKind::Development, Some("//game/none")),
        Err(Error::NotFound(_))
    ));
    f.db()
        .commit(kelp_meta::Command::CreateDepot {
            name: "engine".into(),
            at: 1,
        })
        .unwrap();
    assert!(matches!(
        create("//engine/dev", StreamKind::Development, Some("//game/main")),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn a_merge_must_carry_every_change_of_its_source() {
    let f = MetaFixture::new();
    f.submit(
        "//game/main",
        vec![
            f.add("//game/main/a.txt", b"a1"),
            f.add("//game/main/b.txt", b"b1"),
            f.add("//game/main/c.txt", b"c1"),
        ],
    )
    .unwrap();
    f.create_stream("//game/dev", StreamKind::Development, "//game/main");
    f.submit(
        "//game/dev",
        vec![f.edit("//game/dev/b.txt", b"b1", b"mine")],
    )
    .unwrap();
    f.submit(
        "//game/main",
        vec![f.edit("//game/main/a.txt", b"a1", b"a2")],
    )
    .unwrap();
    let head = f
        .submit(
            "//game/main",
            vec![
                f.edit("//game/main/b.txt", b"b1", b"b2"),
                f.edit("//game/main/c.txt", b"c1", b"c2"),
            ],
        )
        .unwrap();

    // Leaving out c.txt, as a workspace whose view lacks it would.
    let partial = f.submit_as(
        "//game/dev",
        "alice",
        "ws",
        vec![
            f.edit("//game/dev/a.txt", b"a1", b"a2"),
            f.edit("//game/dev/b.txt", b"mine", b"merged"),
        ],
        Some(("//game/main", head.0)),
    );
    let Err(Error::Conflict(lines)) = partial else {
        panic!("a partial merge was accepted: {partial:?}");
    };
    assert!(lines.iter().any(|l| l.contains("c.txt")), "{lines:?}");
    assert!(!lines.iter().any(|l| l.contains("b.txt")), "{lines:?}");

    // A resolved file may differ from both sides.
    f.submit_as(
        "//game/dev",
        "alice",
        "ws",
        vec![
            f.edit("//game/dev/a.txt", b"a1", b"a2"),
            f.edit("//game/dev/b.txt", b"mine", b"merged"),
            f.edit("//game/dev/c.txt", b"c1", b"c2"),
        ],
        Some(("//game/main", head.0)),
    )
    .unwrap();
}
