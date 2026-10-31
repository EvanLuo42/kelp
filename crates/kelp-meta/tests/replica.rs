use kelp_meta::{Error, MetaDb};
use kelp_testkit::Sandbox;
use kelp_testkit::meta::MetaFixture;

fn replica(sandbox: &Sandbox) -> MetaDb {
    MetaDb::open(&sandbox.path("replica.redb")).unwrap()
}

#[test]
fn a_replica_replays_the_log_without_the_content() {
    let f = MetaFixture::new();
    f.submit("//game/main", vec![f.add("//game/main/a.txt", b"a")])
        .unwrap();
    f.submit(
        "//game/main",
        vec![
            f.edit("//game/main/a.txt", b"a", b"b"),
            f.add("//game/main/c.txt", b"c"),
        ],
    )
    .unwrap();
    let sandbox = Sandbox::new();
    let replica = replica(&sandbox);
    let entries = f.db().read().unwrap().log_entries(1, 100).unwrap();
    for entry in &entries {
        replica.replicate(entry).unwrap();
    }

    let (ours, theirs) = (f.db().read().unwrap(), replica.read().unwrap());
    assert_eq!(theirs.last_index().unwrap(), ours.last_index().unwrap());
    assert_eq!(theirs.verify_audit().unwrap(), ours.verify_audit().unwrap());
    let stream = kelp_testkit::depot_path("//game/main");
    assert_eq!(
        theirs.stream(&stream).unwrap().map(|s| s.root),
        ours.stream(&stream).unwrap().map(|s| s.root),
        "replaying the log rebuilds the same trees"
    );
    let content = f.data(b"b").content;
    assert!(
        theirs.manifest(&content).unwrap().is_none(),
        "manifests are fetched on demand"
    );
}

#[test]
fn a_replica_rejects_gaps_and_other_histories() {
    let f = MetaFixture::new();
    f.submit("//game/main", vec![f.add("//game/main/a.txt", b"a")])
        .unwrap();
    let entries = f.db().read().unwrap().log_entries(1, 100).unwrap();
    let sandbox = Sandbox::new();
    let replica = replica(&sandbox);

    let gap = replica.replicate(&entries[1]);
    assert!(matches!(gap, Err(Error::Invalid(_))), "{gap:?}");

    let mut forged = entries[0].clone();
    forged.actor = "mallory".into();
    let forged = replica.replicate(&forged);
    assert!(matches!(forged, Err(Error::AuditMismatch(1))), "{forged:?}");
    assert_eq!(
        replica.read().unwrap().last_index().unwrap().0,
        0,
        "nothing was applied"
    );
    replica.replicate(&entries[0]).unwrap();
}
