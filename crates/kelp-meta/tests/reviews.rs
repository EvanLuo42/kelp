use kelp_core::ClNumber;
use kelp_meta::record::{CheckState, ReviewState};
use kelp_meta::{Command, Error, FileChange, Governance, ShelfContents};
use kelp_testkit::depot_path as p;
use kelp_testkit::meta::MetaFixture;

const RULES: &str = "approve 1 group:leads //game/main/Code/...\nstatus build //game/...\n";

fn setup() -> MetaFixture {
    let f = MetaFixture::new();
    for (name, admin) in [
        ("alice", false),
        ("bob", false),
        ("carol", false),
        ("root", true),
    ] {
        f.db()
            .commit(Command::CreateUser {
                name: name.into(),
                password_hash: Some("hash".into()),
                admin,
                replica: false,
                at: 1,
            })
            .unwrap();
    }
    f.db()
        .commit(Command::SetGroup {
            name: "leads".into(),
            members: vec!["bob".into()],
        })
        .unwrap();
    f.db()
        .commit(Governance::SetReviewRules {
            source: RULES.into(),
        })
        .unwrap();
    f
}

fn shelve(f: &MetaFixture, shelf: Option<u64>, files: Vec<FileChange>) -> u64 {
    let contents = ShelfContents {
        stream: p("//game/main"),
        owner: "alice".into(),
        workspace: "ws".into(),
        description: "render fix".into(),
        files,
        at: 3,
    };
    f.db().shelve(shelf.map(ClNumber), contents).unwrap().0
}

fn vote(f: &MetaFixture, shelf: u64, user: &str, approve: bool) {
    f.db()
        .commit(Governance::Vote {
            shelf: ClNumber(shelf),
            user: user.into(),
            approve: Some(approve),
            at: 4,
        })
        .unwrap();
}

fn build(f: &MetaFixture, shelf: u64, version: u32, state: CheckState) {
    f.db()
        .commit(Governance::ReportStatus {
            shelf: ClNumber(shelf),
            version,
            context: "build".into(),
            state,
            url: None,
            by: "ci".into(),
            at: 5,
        })
        .unwrap();
}

fn unmet(err: Error) -> Vec<String> {
    match err {
        Error::Conflict(problems) => problems,
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[test]
fn covered_changes_need_an_approved_review() {
    let f = setup();
    let files = vec![f.add("//game/main/Code/render.rs", b"fix")];
    let err = f
        .submit_reviewed("//game/main", "alice", files.clone(), None)
        .unwrap_err();
    assert!(unmet(err)[0].contains("needs a review"));

    let shelf = shelve(&f, None, files.clone());
    f.db()
        .commit(Governance::OpenReview {
            shelf: ClNumber(shelf),
            reviewers: vec!["bob".into()],
            by: "alice".into(),
            at: 3,
        })
        .unwrap();
    vote(&f, shelf, "carol", true);
    vote(&f, shelf, "alice", true);
    let err = f
        .submit_reviewed("//game/main", "alice", files.clone(), Some(shelf))
        .unwrap_err();
    assert_eq!(
        unmet(err).len(),
        2,
        "no lead approved and the build has not run"
    );

    vote(&f, shelf, "bob", true);
    build(&f, shelf, 1, CheckState::Success);
    let review = f
        .db()
        .read()
        .unwrap()
        .review(ClNumber(shelf))
        .unwrap()
        .unwrap();
    let checks = f.db().read().unwrap().review_checks(&review).unwrap();
    assert!(checks.iter().all(|c| c.met), "{checks:?}");

    let cl = f
        .submit_reviewed("//game/main", "alice", files, Some(shelf))
        .unwrap();
    let r = f.db().read().unwrap();
    assert_eq!(
        r.review(ClNumber(shelf)).unwrap().unwrap().state,
        ReviewState::Submitted(cl)
    );
    assert!(
        r.shelf(ClNumber(shelf)).unwrap().is_none(),
        "the shelf is consumed"
    );
}

#[test]
fn a_new_version_needs_new_votes_and_a_rejection_blocks() {
    let f = setup();
    let v1 = vec![f.add("//game/main/Code/render.rs", b"fix")];
    let shelf = shelve(&f, None, v1.clone());
    f.db()
        .commit(Governance::OpenReview {
            shelf: ClNumber(shelf),
            reviewers: vec![],
            by: "alice".into(),
            at: 3,
        })
        .unwrap();
    vote(&f, shelf, "bob", true);
    build(&f, shelf, 1, CheckState::Success);

    let v2 = vec![f.add("//game/main/Code/render.rs", b"better fix")];
    assert_eq!(shelve(&f, Some(shelf), v2.clone()), shelf);
    let err = f
        .submit_reviewed("//game/main", "alice", v1, Some(shelf))
        .unwrap_err();
    assert!(unmet(err)[0].contains("differ from version 2"));
    let err = f
        .submit_reviewed("//game/main", "alice", v2.clone(), Some(shelf))
        .unwrap_err();
    assert_eq!(
        unmet(err).len(),
        2,
        "version 1's approval and build no longer count"
    );

    vote(&f, shelf, "bob", true);
    build(&f, shelf, 2, CheckState::Success);
    vote(&f, shelf, "carol", false);
    let err = f
        .submit_reviewed("//game/main", "alice", v2.clone(), Some(shelf))
        .unwrap_err();
    assert_eq!(
        unmet(err),
        [format!("review {shelf} needs not rejected by carol")]
    );

    f.db()
        .commit(Governance::Vote {
            shelf: ClNumber(shelf),
            user: "carol".into(),
            approve: None,
            at: 6,
        })
        .unwrap();
    f.submit_reviewed("//game/main", "alice", v2, Some(shelf))
        .unwrap();
}

#[test]
fn uncovered_changes_and_administrators_need_no_review() {
    let f = setup();
    f.submit_reviewed("//elsewhere/x", "alice", vec![], None)
        .unwrap_err();
    f.db()
        .commit(Governance::SetReviewRules {
            source: "approve 1 * //game/main/Code/...".into(),
        })
        .unwrap();
    f.submit_reviewed(
        "//game/main",
        "alice",
        vec![f.add("//game/main/readme.txt", b"hi")],
        None,
    )
    .unwrap();
    f.submit_reviewed(
        "//game/main",
        "root",
        vec![f.add("//game/main/Code/a.rs", b"a")],
        None,
    )
    .unwrap();
    let bad = f.db().commit(Governance::SetReviewRules {
        source: "approve many * //...".into(),
    });
    assert!(matches!(bad, Err(Error::Invalid(_))));
}

#[test]
fn only_the_shelf_owner_opens_a_review() {
    let f = setup();
    let shelf = shelve(&f, None, vec![f.add("//game/main/Code/a.rs", b"a")]);
    let open = |by: &str| Governance::OpenReview {
        shelf: ClNumber(shelf),
        reviewers: vec![],
        by: by.into(),
        at: 3,
    };
    assert!(matches!(f.db().commit(open("bob")), Err(Error::Invalid(_))));
    f.db().commit(open("alice")).unwrap();
    assert!(matches!(
        f.db().commit(open("alice")),
        Err(Error::AlreadyExists(_))
    ));
    f.db()
        .commit(Command::DeleteShelf {
            cl: ClNumber(shelf),
        })
        .unwrap();
    let review = f
        .db()
        .read()
        .unwrap()
        .review(ClNumber(shelf))
        .unwrap()
        .unwrap();
    assert_eq!(review.state, ReviewState::Closed);
}

fn open_review(f: &MetaFixture, shelf: u64) {
    f.db()
        .commit(Governance::OpenReview {
            shelf: ClNumber(shelf),
            reviewers: vec!["bob".into()],
            by: "alice".into(),
            at: 3,
        })
        .unwrap();
}

#[test]
fn a_vote_concerns_the_version_the_reviewer_saw() {
    let f = setup();
    let shelf = shelve(&f, None, vec![f.add("//game/main/Code/a.rs", b"one")]);
    open_review(&f, shelf);
    shelve(
        &f,
        Some(shelf),
        vec![f.add("//game/main/Code/a.rs", b"two")],
    );
    let stale = f.db().commit(Governance::VoteOn {
        shelf: ClNumber(shelf),
        user: "bob".into(),
        version: 1,
        approve: Some(true),
        at: 4,
    });
    assert!(matches!(stale, Err(Error::Conflict(_))), "{stale:?}");
    f.db()
        .commit(Governance::VoteOn {
            shelf: ClNumber(shelf),
            user: "bob".into(),
            version: 2,
            approve: Some(true),
            at: 4,
        })
        .unwrap();
}

#[test]
fn one_person_counts_once_after_accounts_merge() {
    let f = setup();
    f.db()
        .commit(Governance::SetReviewRules {
            source: "approve 2 * //game/...\n".into(),
        })
        .unwrap();
    let files = vec![f.add("//game/main/a.txt", b"a")];
    let shelf = shelve(&f, None, files.clone());
    open_review(&f, shelf);
    vote(&f, shelf, "bob", true);
    vote(&f, shelf, "carol", true);
    f.db()
        .commit(Command::MergeUser {
            from: "carol".into(),
            into: "bob".into(),
        })
        .unwrap();
    let r = f.db().read().unwrap();
    let review = r.review(ClNumber(shelf)).unwrap().unwrap();
    assert_eq!(review.votes.len(), 1);
    assert!(r.review_checks(&review).unwrap().iter().any(|c| !c.met));
    drop(r);
    let err = f
        .submit_reviewed("//game/main", "alice", files, Some(shelf))
        .unwrap_err();
    assert!(!unmet(err).is_empty(), "one approval of two");
}

#[test]
fn a_status_rule_can_name_who_reports() {
    let f = setup();
    f.db()
        .commit(Command::CreateUser {
            name: "ci-bot".into(),
            password_hash: None,
            admin: false,
            replica: false,
            at: 1,
        })
        .unwrap();
    f.db()
        .commit(Command::SetGroup {
            name: "ci".into(),
            members: vec!["ci-bot".into()],
        })
        .unwrap();
    f.db()
        .commit(Governance::SetReviewRules {
            source: "status build //game/...   by group:ci\n".into(),
        })
        .unwrap();
    let files = vec![f.add("//game/main/a.txt", b"a")];
    let shelf = shelve(&f, None, files.clone());
    open_review(&f, shelf);
    let report = |by: &str| {
        f.db()
            .commit(Governance::ReportStatus {
                shelf: ClNumber(shelf),
                version: 1,
                context: "build".into(),
                state: CheckState::Success,
                url: None,
                by: by.into(),
                at: 5,
            })
            .unwrap();
    };
    report("alice");
    let err = f
        .submit_reviewed("//game/main", "alice", files.clone(), Some(shelf))
        .unwrap_err();
    assert!(
        !unmet(err).is_empty(),
        "the author's own report does not count"
    );
    report("ci-bot");
    f.submit_reviewed("//game/main", "alice", files, Some(shelf))
        .unwrap();
}
