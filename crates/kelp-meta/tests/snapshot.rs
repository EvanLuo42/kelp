use kelp_meta::{Error, MetaDb};
use kelp_testkit::meta::MetaFixture;
use kelp_testkit::{Sandbox, depot_path};

#[test]
fn a_snapshot_is_a_complete_database() {
    let f = MetaFixture::new();
    f.submit_linked(
        "//game/main",
        vec![
            f.add("//game/main/a.txt", b"a"),
            f.add("//game/main/d/b.txt", b"b"),
        ],
        &["GAME-1"],
        &[],
    )
    .unwrap();
    let sandbox = Sandbox::new();
    let path = sandbox.path("copy.redb");
    f.db().snapshot_to(&path).unwrap();
    assert!(
        matches!(f.db().snapshot_to(&path), Err(Error::AlreadyExists(_))),
        "an existing file is not overwritten"
    );

    let copy = MetaDb::open(&path).unwrap();
    let (ours, theirs) = (f.db().read().unwrap(), copy.read().unwrap());
    assert_eq!(theirs.verify_audit().unwrap(), ours.verify_audit().unwrap());
    let stream = depot_path("//game/main");
    assert_eq!(
        theirs.stream(&stream).unwrap(),
        ours.stream(&stream).unwrap()
    );
    assert_eq!(theirs.live_nodes().unwrap(), ours.live_nodes().unwrap());
    assert_eq!(
        theirs.live_contents().unwrap(),
        ours.live_contents().unwrap()
    );
    assert_eq!(
        theirs.issue_changes("GAME-1").unwrap(),
        ours.issue_changes("GAME-1").unwrap()
    );
    let a = f.data(b"a").content;
    assert_eq!(theirs.manifest(&a).unwrap(), ours.manifest(&a).unwrap());
}
