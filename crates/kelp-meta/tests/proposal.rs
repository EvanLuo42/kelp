use std::collections::BTreeMap;

use kelp_meta::db::{Outcome, Proposal};
use kelp_meta::{Command, Error, MetaDb};
use kelp_testkit::meta::MetaFixture;
use kelp_testkit::{Sandbox, depot_path};

fn marks(applied: &[u8]) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([("applied".to_owned(), applied.to_vec())])
}

#[test]
fn consensus_entries_are_stored_with_the_change_or_without_it() {
    let f = MetaFixture::new();
    let create = Proposal::Command {
        actor: "alice".into(),
        command: Command::CreateDepot {
            name: "engine".into(),
            at: 1,
        },
    };
    let outcome = f
        .db()
        .apply_with_consensus(Some(&create), &marks(b"1"))
        .unwrap();
    assert!(matches!(outcome, Outcome::Logged { .. }));
    let r = f.db().read().unwrap();
    assert_eq!(r.consensus("applied").unwrap(), Some(b"1".to_vec()));
    let before = r.last_index().unwrap();
    drop(r);

    let err = f
        .db()
        .apply_with_consensus(Some(&create), &marks(b"2"))
        .unwrap_err();
    assert!(matches!(err, Error::AlreadyExists(_)), "{err:?}");
    let r = f.db().read().unwrap();
    assert_eq!(
        r.consensus("applied").unwrap(),
        Some(b"2".to_vec()),
        "a failed proposal still counts as applied"
    );
    assert_eq!(r.last_index().unwrap(), before, "and logs nothing");
    drop(r);

    f.db().apply_with_consensus(None, &marks(b"3")).unwrap();
    assert_eq!(
        f.db().read().unwrap().consensus("applied").unwrap(),
        Some(b"3".to_vec())
    );
}

#[test]
fn numbers_are_assigned_when_a_proposal_is_applied() {
    let f = MetaFixture::new();
    let submission = |text: &[u8]| kelp_meta::Submission {
        stream: depot_path("//game/main"),
        author: "alice".into(),
        workspace: "ws".into(),
        description: "x".into(),
        changes: vec![f.add(&format!("//game/main/{}.txt", text[0] as char), text)],
        integration: None,
        issues: Vec::new(),
        fixes: Vec::new(),
        review: None,
        at: 1,
    };
    let first = f.db().propose(&Proposal::Submit(submission(b"a"))).unwrap();
    let second = f.db().propose(&Proposal::Submit(submission(b"b"))).unwrap();
    assert_eq!(first.submitted().1.0 + 1, second.submitted().1.0);
}

#[test]
fn a_database_can_be_replaced_by_a_snapshot() {
    let source = MetaFixture::new();
    source
        .submit("//game/main", vec![source.add("//game/main/a.txt", b"a")])
        .unwrap();
    source
        .db()
        .apply_with_consensus(None, &marks(b"7"))
        .unwrap();
    let sandbox = Sandbox::new();
    let snapshot = sandbox.path("snapshot.redb");
    source.db().snapshot_to(&snapshot).unwrap();

    let target = MetaDb::open(&sandbox.path("target.redb")).unwrap();
    target
        .commit(Command::CreateDepot {
            name: "other".into(),
            at: 1,
        })
        .unwrap();
    target.replace_with(&snapshot).unwrap();
    let (ours, theirs) = (source.db().read().unwrap(), target.read().unwrap());
    assert_eq!(theirs.verify_audit().unwrap(), ours.verify_audit().unwrap());
    assert_eq!(
        theirs.depots().unwrap(),
        ours.depots().unwrap(),
        "nothing left over"
    );
    assert_eq!(theirs.consensus("applied").unwrap(), Some(b"7".to_vec()));
    let stream = depot_path("//game/main");
    assert_eq!(
        theirs.stream(&stream).unwrap(),
        ours.stream(&stream).unwrap()
    );
}
