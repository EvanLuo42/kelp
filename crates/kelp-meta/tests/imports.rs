use kelp_core::ClNumber;
use kelp_meta::record::Import;
use kelp_meta::{Command, Dirs, EntryKind, Error, MetaDb, StreamKind};
use kelp_testkit::depot_path as p;
use kelp_testkit::meta::MetaFixture;

/// A fixture with `//engine/main` holding `Source/a.h` and `//game/main`
/// holding `Code/main.cpp`.
fn setup() -> MetaFixture {
    let f = MetaFixture::new();
    f.db()
        .commit(Command::CreateDepot {
            name: "engine".into(),
            at: 1,
        })
        .unwrap();
    f.db()
        .commit(Command::CreateStream {
            path: p("//engine/main"),
            kind: StreamKind::Mainline,
            parent: None,
            at: 1,
        })
        .unwrap();
    f.submit(
        "//engine/main",
        vec![f.add("//engine/main/Source/a.h", b"v1")],
    )
    .unwrap();
    f.submit(
        "//game/main",
        vec![f.add("//game/main/Code/main.cpp", b"main")],
    )
    .unwrap();
    f
}

fn import(at: Option<u64>, writable: bool) -> Import {
    Import {
        source: p("//engine/main"),
        from: "Source".into(),
        to: "Engine".into(),
        at: at.map(ClNumber),
        writable,
    }
}

fn set(f: &MetaFixture, imports: Vec<Import>) -> kelp_meta::Result<()> {
    f.db()
        .commit(Command::SetImports {
            stream: p("//game/main"),
            imports,
        })
        .map(|_| ())
}

/// Returns the content of `path` in what `//game/main` shows at `at`.
fn seen(db: &MetaDb, path: &str, at: Option<u64>) -> Option<Vec<u8>> {
    let r = db.read().unwrap();
    let (_, root) = r.view_at(&p("//game/main"), at.map(ClNumber)).unwrap();
    let nodes = r.nodes().unwrap();
    match Dirs::new(&nodes).lookup(root, path).unwrap()?.kind {
        EntryKind::File { content, .. } => Some(content.0.0.to_vec()),
        _ => None,
    }
}

fn id(f: &MetaFixture, bytes: &[u8]) -> Vec<u8> {
    f.data(bytes).content.0.0.to_vec()
}

#[test]
fn a_followed_import_shows_the_source_as_it_changes() {
    let f = setup();
    set(&f, vec![import(None, false)]).unwrap();
    assert_eq!(seen(f.db(), "Engine/a.h", None), Some(id(&f, b"v1")));
    assert_eq!(seen(f.db(), "Code/main.cpp", None), Some(id(&f, b"main")));
    let before = f
        .db()
        .read()
        .unwrap()
        .view_at(&p("//game/main"), None)
        .unwrap()
        .0;

    let v2 = f
        .submit(
            "//engine/main",
            vec![f.edit("//engine/main/Source/a.h", b"v1", b"v2")],
        )
        .unwrap();
    let (cl, _) = f
        .db()
        .read()
        .unwrap()
        .view_at(&p("//game/main"), None)
        .unwrap();
    assert_eq!(cl, v2, "the source's changelist is the view's");
    assert_eq!(seen(f.db(), "Engine/a.h", None), Some(id(&f, b"v2")));
    assert_eq!(
        seen(f.db(), "Engine/a.h", Some(before.0)),
        Some(id(&f, b"v1")),
        "history is kept"
    );
}

#[test]
fn a_pinned_import_stays_and_imports_are_read_only_unless_writable() {
    let f = setup();
    set(&f, vec![import(Some(1), false)]).unwrap();
    f.submit(
        "//engine/main",
        vec![f.edit("//engine/main/Source/a.h", b"v1", b"v2")],
    )
    .unwrap();
    assert_eq!(seen(f.db(), "Engine/a.h", None), Some(id(&f, b"v1")));
    let err = f
        .submit(
            "//game/main",
            vec![f.edit("//game/main/Engine/a.h", b"v1", b"x")],
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::Invalid(ref m) if m.contains("read-only")),
        "{err:?}"
    );
}

#[test]
fn a_writable_import_submits_to_both_streams_at_once() {
    let f = setup();
    set(&f, vec![import(None, true)]).unwrap();
    let cl = f
        .submit(
            "//game/main",
            vec![
                f.edit("//game/main/Engine/a.h", b"v1", b"engine fix"),
                f.edit("//game/main/Code/main.cpp", b"main", b"uses fix"),
            ],
        )
        .unwrap();
    let r = f.db().read().unwrap();
    let engine = r.stream(&p("//engine/main")).unwrap().unwrap();
    assert_eq!(engine.head, cl, "one changelist for both streams");
    let revs = r.revisions(&p("//engine/main/Source/a.h")).unwrap();
    assert_eq!(revs[0].cl, cl);
    assert!(
        r.revisions(&p("//game/main/Engine/a.h"))
            .unwrap()
            .is_empty()
    );
    let (at, root) = r.stream_at(&p("//engine/main"), Some(cl)).unwrap();
    assert_eq!(at, cl);
    let nodes = r.nodes().unwrap();
    assert!(
        Dirs::new(&nodes)
            .lookup(root, "Source/a.h")
            .unwrap()
            .is_some()
    );
    drop(r);
    assert_eq!(
        seen(f.db(), "Engine/a.h", None),
        Some(id(&f, b"engine fix"))
    );
    assert_eq!(
        seen(f.db(), "Code/main.cpp", None),
        Some(id(&f, b"uses fix"))
    );

    let stale = f.submit(
        "//game/main",
        vec![f.edit("//game/main/Engine/a.h", b"v1", b"y")],
    );
    assert!(
        matches!(stale, Err(Error::Conflict(_))),
        "checked against the source's head"
    );
}

#[test]
fn imports_are_validated() {
    let f = setup();
    let mut overlapping = import(None, false);
    overlapping.to = "Engine/Sub".into();
    assert!(set(&f, vec![import(None, false), overlapping]).is_err());
    let mut occupied = import(None, false);
    occupied.to = "Code".into();
    assert!(set(&f, vec![occupied]).is_err(), "the path must be empty");
    assert!(
        set(&f, vec![import(Some(1), true)]).is_err(),
        "writable imports follow"
    );
    let mut missing = import(None, false);
    missing.from = "Nope".into();
    assert!(set(&f, vec![missing]).is_err());

    set(&f, vec![import(None, false)]).unwrap();
    let nested = f.db().commit(Command::SetImports {
        stream: p("//engine/main"),
        imports: vec![Import {
            source: p("//game/main"),
            from: String::new(),
            to: "Game".into(),
            at: None,
            writable: false,
        }],
    });
    assert!(nested.is_err(), "an imported stream cannot import");
}

#[test]
fn views_survive_collection_and_follow_obliteration() {
    let f = setup();
    set(&f, vec![import(None, false)]).unwrap();
    f.submit(
        "//engine/main",
        vec![f.add("//engine/main/Source/secret.h", b"s")],
    )
    .unwrap();
    let (live, through) = f.db().read().unwrap().live_nodes().unwrap();
    f.db().sweep_nodes(&live, through).unwrap();
    assert_eq!(seen(f.db(), "Engine/a.h", None), Some(id(&f, b"v1")));

    f.db()
        .commit(Command::Obliterate {
            paths: vec!["//engine/main/Source/secret.h".into()],
        })
        .unwrap();
    assert_eq!(seen(f.db(), "Engine/secret.h", None), None);
    assert_eq!(seen(f.db(), "Engine/a.h", None), Some(id(&f, b"v1")));

    set(&f, vec![]).unwrap();
    f.submit("//game/main", vec![f.add("//game/main/Code/b.cpp", b"b")])
        .unwrap();
    assert_eq!(
        seen(f.db(), "Engine/a.h", None),
        None,
        "removed imports disappear"
    );
}

#[test]
fn obliterating_keeps_the_imports_each_view_had() {
    let f = setup();
    set(&f, vec![import(None, false)]).unwrap();
    let with_imports = f
        .db()
        .read()
        .unwrap()
        .view_at(&p("//game/main"), None)
        .unwrap()
        .0;
    set(&f, vec![]).unwrap();
    let (without, _) = f
        .db()
        .read()
        .unwrap()
        .view_at(&p("//game/main"), None)
        .unwrap();
    assert!(
        without > with_imports,
        "changing imports takes a new changelist"
    );
    assert_eq!(seen(f.db(), "Engine/a.h", None), None);

    f.submit(
        "//game/main",
        vec![f.add("//game/main/Code/junk.cpp", b"junk")],
    )
    .unwrap();
    f.db()
        .commit(Command::Obliterate {
            paths: vec!["//game/main/Code/junk.cpp".into()],
        })
        .unwrap();
    assert_eq!(
        seen(f.db(), "Engine/a.h", Some(with_imports.0)),
        Some(id(&f, b"v1")),
        "the old view still shows what was imported then"
    );
    assert_eq!(seen(f.db(), "Code/junk.cpp", None), None);
}

#[test]
fn obliterating_a_source_file_scrubs_what_importers_submitted_and_shelved() {
    let f = setup();
    set(&f, vec![import(None, true)]).unwrap();
    let cl = f
        .submit(
            "//game/main",
            vec![
                f.edit("//game/main/Engine/a.h", b"v1", b"secret"),
                f.edit("//game/main/Code/main.cpp", b"main", b"uses it"),
            ],
        )
        .unwrap();
    let shelf = f
        .db()
        .shelve(
            None,
            kelp_meta::ShelfContents {
                stream: p("//game/main"),
                owner: "alice".into(),
                workspace: "ws".into(),
                description: "wip".into(),
                files: vec![
                    f.edit("//game/main/Engine/a.h", b"secret", b"more secret"),
                    f.edit("//game/main/Code/main.cpp", b"uses it", b"wip"),
                ],
                at: 3,
            },
        )
        .unwrap();

    f.db()
        .commit(Command::Obliterate {
            paths: vec!["//engine/main/Source/a.h".into()],
        })
        .unwrap();
    let r = f.db().read().unwrap();
    let paths = |changes: &[kelp_meta::FileChange]| -> Vec<String> {
        changes.iter().map(|c| c.path.to_string()).collect()
    };
    let record = r.changelist(cl).unwrap().unwrap();
    assert_eq!(paths(&record.changes), ["//game/main/Code/main.cpp"]);
    let shelf = r.shelf(shelf).unwrap().unwrap();
    assert_eq!(paths(&shelf.contents.files), ["//game/main/Code/main.cpp"]);
    assert!(
        r.revisions(&p("//engine/main/Source/a.h"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn review_rules_of_the_source_govern_writable_imports() {
    let f = setup();
    set(&f, vec![import(None, true)]).unwrap();
    f.db()
        .commit(kelp_meta::Governance::SetReviewRules {
            source: "approve 1 * //engine/...\n".into(),
        })
        .unwrap();
    let err = f
        .submit(
            "//game/main",
            vec![f.edit("//game/main/Engine/a.h", b"v1", b"sneaky")],
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::Conflict(ref m) if m[0].contains("needs a review")),
        "{err:?}"
    );
    f.submit(
        "//game/main",
        vec![f.edit("//game/main/Code/main.cpp", b"main", b"free")],
    )
    .unwrap();
}
