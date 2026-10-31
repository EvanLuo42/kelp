//! Regression tests for ways the metadata could lose or misplace data.

use kelp_meta::record::ReviewState;
use kelp_meta::{ArtifactContents, Command, Error, Governance, ShelfContents};
use kelp_testkit::depot_path as p;
use kelp_testkit::meta::MetaFixture;

fn obliterate(f: &MetaFixture, path: &str) -> kelp_meta::Result<()> {
    f.db()
        .commit(Command::Obliterate {
            paths: vec![path.into()],
        })
        .map(|_| ())
}

#[test]
fn a_recursive_obliterate_reaches_past_similarly_named_siblings() {
    let f = MetaFixture::new();
    f.submit(
        "//game/main",
        vec![
            f.add("//game/main/foo/a.txt", b"secret"),
            f.add("//game/main/foo-bar", b"keep 1"),
            f.add("//game/main/foo.txt", b"keep 2"),
        ],
    )
    .unwrap();
    obliterate(&f, "//game/main/foo/...").unwrap();
    let r = f.db().read().unwrap();
    assert!(r.revisions(&p("//game/main/foo/a.txt")).unwrap().is_empty());
    assert_eq!(r.revisions(&p("//game/main/foo-bar")).unwrap().len(), 1);
    assert_eq!(r.revisions(&p("//game/main/foo.txt")).unwrap().len(), 1);
    let secret = f.data(b"secret").content;
    assert!(
        !r.live_contents().unwrap().contains(&secret),
        "the content can be collected"
    );
    assert!(
        r.live_contents()
            .unwrap()
            .contains(&f.data(b"keep 2").content)
    );
}

#[test]
fn an_obliterate_pattern_must_match_what_it_names() {
    let f = MetaFixture::new();
    f.submit(
        "//game/main",
        vec![
            f.add("//game/main/Content/a.txt", b"a"),
            f.add("//game/main/b.txt", b"b"),
        ],
    )
    .unwrap();
    assert!(
        matches!(obliterate(&f, "//game/main/Content"), Err(Error::Invalid(ref m)) if m.contains("directory"))
    );
    assert!(
        matches!(obliterate(&f, "//game/main/b.txt/..."), Err(Error::Invalid(ref m)) if m.contains("file"))
    );
    let r = f.db().read().unwrap();
    assert_eq!(
        r.revisions(&p("//game/main/Content/a.txt")).unwrap().len(),
        1,
        "nothing changed"
    );
}

#[test]
fn obliterating_a_whole_shelf_closes_its_review() {
    let f = MetaFixture::new();
    f.submit(
        "//game/main",
        vec![f.add("//game/main/x/base.txt", b"base")],
    )
    .unwrap();
    let shelf = f
        .db()
        .shelve(
            None,
            ShelfContents {
                stream: p("//game/main"),
                owner: "alice".into(),
                workspace: "ws".into(),
                description: "wip".into(),
                files: vec![f.add("//game/main/x/new.txt", b"new")],
                at: 3,
            },
        )
        .unwrap();
    f.db()
        .commit(Command::CreateUser {
            name: "alice".into(),
            password_hash: Some("h".into()),
            admin: false,
            replica: false,
            at: 1,
        })
        .unwrap();
    f.db()
        .commit(Governance::OpenReview {
            shelf,
            reviewers: vec![],
            by: "alice".into(),
            at: 3,
        })
        .unwrap();
    obliterate(&f, "//game/main/x/...").unwrap();
    let r = f.db().read().unwrap();
    assert!(r.shelf(shelf).unwrap().is_none());
    assert_eq!(r.review(shelf).unwrap().unwrap().state, ReviewState::Closed);
}

#[test]
fn a_retention_of_zero_is_refused() {
    let f = MetaFixture::new();
    let set = f.db().commit(Command::SetTypemap {
        depot: "game".into(),
        source: "[[rule]]\npattern = \"**\"\nretain = 0\n".into(),
        by: "admin".into(),
        at: 1,
    });
    assert!(
        matches!(set, Err(Error::Invalid(ref m)) if m.contains("at least 1")),
        "{set:?}"
    );
}

#[test]
fn artifact_paths_differing_only_in_case_are_refused() {
    let f = MetaFixture::new();
    let cl = f
        .submit("//game/main", vec![f.add("//game/main/a.txt", b"a")])
        .unwrap();
    let publish = f.db().publish_artifact(ArtifactContents {
        stream: p("//game/main"),
        cl,
        name: "build".into(),
        tags: vec![],
        files: vec![
            ("Bin/A.dll".into(), f.data(b"1")),
            ("bin/a.dll".into(), f.data(b"2")),
        ],
        by: "ci".into(),
        at: 4,
    });
    assert!(
        matches!(publish, Err(Error::DuplicateEntry(_))),
        "{publish:?}"
    );
}
