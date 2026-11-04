use kelp_core::{ClNumber, ContentId};
use kelp_meta::{Command, Dirs, StreamKind};
use kelp_testkit::depot_path as p;
use kelp_testkit::meta::MetaFixture;

#[test]
fn retain_purges_old_revisions() {
    let f = MetaFixture::new();
    let typemap = "[[rule]]\npattern = \"Baked/**\"\nretain = 2\n";
    let cmd = Command::SetTypemap {
        depot: "game".into(),
        source: typemap.into(),
        by: "admin".into(),
        at: 1,
    };
    f.db().commit(cmd).unwrap();
    let path = "//game/main/Baked/lighting.bin";
    f.submit("//game/main", vec![f.add(path, b"v1")]).unwrap();
    f.submit("//game/main", vec![f.edit(path, b"v1", b"v2")])
        .unwrap();
    f.submit("//game/main", vec![f.edit(path, b"v2", b"v3")])
        .unwrap();
    f.submit("//game/main", vec![f.add("//game/main/keep.txt", b"v1")])
        .unwrap();

    let r = f.db().read().unwrap();
    let purged: Vec<_> = r
        .revisions(&p(path))
        .unwrap()
        .iter()
        .map(|r| (r.rev, r.purged))
        .collect();
    assert_eq!(purged, [(3, false), (2, false), (1, true)]);
    let live = r.live_contents().unwrap();
    assert!(
        live.contains(&ContentId::of(b"v1")),
        "the same bytes are still live through keep.txt"
    );
    assert!(live.contains(&ContentId::of(b"v3")));
}

#[test]
fn obliterate_rewrites_history_and_branched_copies() {
    let f = MetaFixture::new();
    let secret = "//game/main/Secret/key.pem";
    f.submit(
        "//game/main",
        vec![f.add(secret, b"secret"), f.add("//game/main/a.txt", b"a")],
    )
    .unwrap();
    f.create_stream("//game/dev", StreamKind::Development, "//game/main");
    f.submit("//game/main", vec![f.edit(secret, b"secret", b"secret 2")])
        .unwrap();
    let (nodes_before, _) = f.db().read().unwrap().live_nodes().unwrap();

    f.db()
        .commit_by(
            "admin",
            Command::Obliterate {
                paths: vec!["//game/main/Secret/...".into()],
            },
        )
        .unwrap();

    let r = f.db().read().unwrap();
    assert!(r.revisions(&p(secret)).unwrap().is_empty());
    let nodes = r.nodes().unwrap();
    let dirs = Dirs::new(&nodes);
    for cl in [ClNumber(1), ClNumber(2)] {
        let record = r.changelist(cl).unwrap().unwrap();
        assert!(
            dirs.lookup(record.root, "Secret/key.pem")
                .unwrap()
                .is_none(),
            "removed from change {cl}"
        );
        assert!(
            record
                .changes
                .iter()
                .all(|c| !c.path.as_str().contains("Secret"))
        );
    }
    assert!(
        dirs.lookup(r.stream_at(&p("//game/main"), None).unwrap().1, "a.txt")
            .unwrap()
            .is_some()
    );

    // The development stream branched from the old tree loses its copy too,
    // or a copy-up would bring the secret back.
    let dev = r.stream(&p("//game/dev")).unwrap().unwrap();
    assert!(dirs.lookup(dev.root, "Secret/key.pem").unwrap().is_none());
    let live = r.live_contents().unwrap();
    assert!(!live.contains(&ContentId::of(b"secret")));
    assert!(!live.contains(&ContentId::of(b"secret 2")));

    let (live_nodes, through) = r.live_nodes().unwrap();
    drop(r);
    assert!(live_nodes.len() < nodes_before.len() + 10);
    let before = f.db().read().unwrap().node_count().unwrap();
    let swept = f.db().sweep_nodes(&live_nodes, through).unwrap();
    assert!(swept > 0, "the old trees' nodes are unreachable now");
    assert_eq!(f.db().read().unwrap().node_count().unwrap(), before - swept);
    let r = f.db().read().unwrap();
    let nodes = r.nodes().unwrap();
    let dirs = Dirs::new(&nodes);
    assert_eq!(
        dirs.diff_paths(None, r.stream_at(&p("//game/main"), None).unwrap().1)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn obliterate_rejects_whole_streams() {
    let f = MetaFixture::new();
    let err = f.db().commit(Command::Obliterate {
        paths: vec!["//game/main/...".into()],
    });
    assert!(err.is_err());
}

#[test]
fn the_audit_log_chains_every_command() {
    let f = MetaFixture::new();
    f.submit("//game/main", vec![f.add("//game/main/a.txt", b"a")])
        .unwrap();
    let user = Command::CreateUser {
        name: "bob".into(),
        password_hash: Some("$argon2id$secret".into()),
        admin: false,
        replica: false,
        at: 3,
    };
    f.db().commit_by("admin", user).unwrap();

    let r = f.db().read().unwrap();
    let entries = r.audit(0, 100).unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[2].actor, "alice");
    assert!(entries[2].summary.starts_with("submit change 1"));
    assert_eq!(entries[3].actor, "admin");
    assert!(
        !entries[3].summary.contains("argon2"),
        "secrets stay out of the audit log"
    );
    for pair in entries.windows(2) {
        assert_eq!(pair[1].prev, pair[0].hash);
    }
    let (count, head) = r.verify_audit().unwrap();
    assert_eq!((count, head), (4, entries[3].hash));
}

#[test]
fn an_obliterate_between_marking_and_sweeping_keeps_the_trees_it_wrote() {
    let f = MetaFixture::new();
    f.submit(
        "//game/main",
        vec![
            f.add("//game/main/Docs/a.txt", b"a"),
            f.add("//game/main/Docs/b.txt", b"b"),
        ],
    )
    .unwrap();
    f.submit("//game/main", vec![f.add("//game/main/c.txt", b"c")])
        .unwrap();
    let (live, through) = f.db().read().unwrap().live_nodes().unwrap();

    // The obliterate gives changelists 1 and 2 trees the marking never saw.
    f.db()
        .commit(Command::Obliterate {
            paths: vec!["//game/main/Docs/a.txt".into()],
        })
        .unwrap();
    f.db().sweep_nodes(&live, through).unwrap();

    let r = f.db().read().unwrap();
    r.live_nodes().expect("every tree is whole");
    let nodes = r.nodes().unwrap();
    let dirs = Dirs::new(&nodes);
    let first = r.changelist(ClNumber(1)).unwrap().unwrap();
    assert!(dirs.lookup(first.root, "Docs/b.txt").unwrap().is_some());
    assert!(dirs.lookup(first.root, "Docs/a.txt").unwrap().is_none());
    drop((nodes, r));

    // Once a changelist follows the obliterate, sweeping trusts its marking
    // again and still keeps everything in use.
    f.submit("//game/main", vec![f.add("//game/main/d.txt", b"d")])
        .unwrap();
    let (live, through) = f.db().read().unwrap().live_nodes().unwrap();
    f.db().sweep_nodes(&live, through).unwrap();
    f.db().read().unwrap().live_nodes().unwrap();
}
