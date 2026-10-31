use kelp_core::ClNumber;
use kelp_meta::{Error, Governance, StreamKind};
use kelp_testkit::depot_path as p;
use kelp_testkit::meta::MetaFixture;

#[test]
fn issues_are_followed_through_merges_and_copies() {
    let f = MetaFixture::new();
    let base = f
        .submit_linked(
            "//game/main",
            vec![f.add("//game/main/a.txt", b"a")],
            &["GAME-1"],
            &[],
        )
        .unwrap();
    f.create_stream("//game/dev", StreamKind::Development, "//game/main");
    f.create_stream("//game/rel", StreamKind::Release, "//game/main");
    let fix = f
        .submit_linked(
            "//game/dev",
            vec![f.edit("//game/dev/a.txt", b"a", b"fixed")],
            &["GAME-2", "art-1"],
            &["GAME-2"],
        )
        .unwrap();
    let r = f.db().read().unwrap();
    assert_eq!(
        r.changelist(fix).unwrap().unwrap().issues,
        ["GAME-2", "ART-1"]
    );
    assert_eq!(
        r.issue_reach("game-2").unwrap(),
        [(p("//game/dev"), fix)],
        "only the stream it was submitted to"
    );
    let rel_issues = r.issues_in(&p("//game/rel"), ClNumber(0), None).unwrap();
    assert_eq!(
        rel_issues.keys().collect::<Vec<_>>(),
        ["GAME-1"],
        "inherited at creation"
    );
    drop(r);

    let copy = f
        .db()
        .copy_up(&p("//game/dev"), "alice".into(), "copy up".into(), 3)
        .unwrap()
        .1;
    let r = f.db().read().unwrap();
    let reach = r.issue_reach("GAME-2").unwrap();
    assert_eq!(reach, [(p("//game/dev"), fix), (p("//game/main"), copy)]);
    let main = r.issues_in(&p("//game/main"), copy, Some(base)).unwrap();
    assert_eq!(main.keys().collect::<Vec<_>>(), ["ART-1", "GAME-2"]);
    assert_eq!(main["GAME-2"], [fix]);
    assert!(
        !r.issues_in(&p("//game/rel"), ClNumber(0), None)
            .unwrap()
            .contains_key("GAME-2"),
        "the release stream was not merged"
    );
}

#[test]
fn links_are_canonical_and_can_be_corrected() {
    let f = MetaFixture::new();
    let cl = f
        .submit_linked(
            "//game/main",
            vec![f.add("//game/main/a.txt", b"a")],
            &["game-1"],
            &["Game-2"],
        )
        .unwrap();
    let r = f.db().read().unwrap();
    let change = r.changelist(cl).unwrap().unwrap();
    assert_eq!(
        change.issues,
        ["GAME-1", "GAME-2"],
        "fixed issues are linked too"
    );
    assert_eq!(change.fixes, ["GAME-2"]);
    drop(r);

    f.db()
        .commit(Governance::LinkIssues {
            cl,
            add: vec!["ART-7".into()],
            remove: vec!["GAME-2".into()],
        })
        .unwrap();
    let r = f.db().read().unwrap();
    let change = r.changelist(cl).unwrap().unwrap();
    assert_eq!(
        (change.issues, change.fixes),
        (vec!["GAME-1".to_owned(), "ART-7".to_owned()], vec![])
    );
    assert!(r.issue_changes("GAME-2").unwrap().is_empty());
    assert_eq!(r.issue_changes("art-7").unwrap()[0].number, cl);
    drop(r);
    let bad = f.submit_linked(
        "//game/main",
        vec![f.add("//game/main/c.txt", b"c")],
        &["not a key"],
        &[],
    );
    assert!(matches!(bad, Err(Error::Invalid(_))));
}

#[test]
fn an_issue_rule_requires_a_link() {
    let f = MetaFixture::new();
    f.db()
        .commit(Governance::SetReviewRules {
            source: "issue //game/main/...".into(),
        })
        .unwrap();
    let err = f
        .submit_linked(
            "//game/main",
            vec![f.add("//game/main/a.txt", b"a")],
            &[],
            &[],
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::Conflict(ref p) if p[0].contains("linked issue")),
        "{err:?}"
    );
    f.submit_linked(
        "//game/main",
        vec![f.add("//game/main/a.txt", b"a")],
        &["GAME-9"],
        &[],
    )
    .unwrap();
}
