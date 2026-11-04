//! A stream created again at a deleted stream's path starts afresh, and
//! every stream remembers who created it.

use kelp_core::{ClNumber, ContentId, FileManifest};
use kelp_meta::record::Import;
use kelp_meta::{
    Command, Dirs, FileAction, FileChange, FileData, MetaDb, StreamKind, Submission,
};
use kelp_testkit::{Sandbox, depot_path as p};
use redb::TableDefinition;

type Bytes = &'static [u8];
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const VIEWS: TableDefinition<(&str, u64), Bytes> = TableDefinition::new("views");
const VIEW_IMPORTS: TableDefinition<(&str, u64), Bytes> = TableDefinition::new("view_imports");
const STREAM_OWNERS: TableDefinition<&str, &str> = TableDefinition::new("stream_owners");

fn add(db: &MetaDb, stream: &str, path: &str, bytes: &[u8]) -> ClNumber {
    let content = ContentId::of(bytes);
    let size = bytes.len() as u64;
    db.put_manifest(&FileManifest {
        content,
        size,
        chunks: Vec::new(),
    })
    .unwrap();
    let submission = Submission {
        stream: p(stream),
        author: "alice".into(),
        workspace: "ws".into(),
        description: "test".into(),
        changes: vec![FileChange {
            path: p(path),
            action: FileAction::Add {
                data: FileData {
                    content,
                    size,
                    executable: false,
                },
            },
        }],
        integration: None,
        issues: Vec::new(),
        fixes: Vec::new(),
        review: None,
        at: 2,
    };
    db.submit(submission).unwrap().1
}

fn create(db: &MetaDb, actor: &str, path: &str, parent: Option<&str>) {
    let kind = match parent {
        Some(_) => StreamKind::Development,
        None => StreamKind::Mainline,
    };
    db.commit_by(
        actor,
        Command::CreateStream {
            path: p(path),
            kind,
            parent: parent.map(p),
            at: 1,
        },
    )
    .unwrap();
}

fn set_imports(db: &MetaDb, imports: Vec<Import>) {
    db.commit(Command::SetImports {
        stream: p("//game/t"),
        imports,
    })
    .unwrap();
}

fn owner(db: &MetaDb, path: &str) -> Option<String> {
    db.read().unwrap().stream_owner(&p(path)).unwrap()
}

/// Asserts that `//game/t` shows its own tree, which has `x.txt` from
/// `//game/main` and nothing of `//game/lib`.
fn shows_its_own_tree(db: &MetaDb) {
    let r = db.read().unwrap();
    let own = r.stream_at(&p("//game/t"), None).unwrap();
    let view = r.view_at(&p("//game/t"), None).unwrap();
    assert_eq!(view, own, "no view of the deleted stream");
    let nodes = r.nodes().unwrap();
    let dirs = Dirs::new(&nodes);
    assert!(dirs.lookup(view.1, "x.txt").unwrap().is_some());
    assert!(dirs.lookup(view.1, "Lib").unwrap().is_none());
}

/// Builds the history: `//game/lib` by bob, who becomes robert; `//game/t`
/// by carol, importing lib, then deleted; then `//game/t` again by dave.
fn history(db: &MetaDb) {
    db.commit(Command::CreateDepot {
        name: "game".into(),
        at: 1,
    })
    .unwrap();
    create(db, "admin", "//game/main", None);
    db.commit(Command::CreateUser {
        name: "bob".into(),
        password_hash: Some("x".into()),
        admin: false,
        replica: false,
        at: 1,
    })
    .unwrap();
    create(db, "bob", "//game/lib", None);
    add(db, "//game/lib", "//game/lib/l.txt", b"l");
    create(db, "carol", "//game/t", Some("//game/main"));
    set_imports(
        db,
        vec![Import {
            source: p("//game/lib"),
            from: String::new(),
            to: "Lib".into(),
            at: None,
            writable: false,
        }],
    );
    set_imports(db, Vec::new());
    db.commit(Command::DeleteStream { path: p("//game/t") })
        .unwrap();
    assert_eq!(owner(db, "//game/t"), None);
    add(db, "//game/main", "//game/main/x.txt", b"x");
    create(db, "dave", "//game/t", Some("//game/main"));
    db.commit(Command::MergeUser {
        from: "bob".into(),
        into: "robert".into(),
    })
    .unwrap();
}

#[test]
fn a_recreated_stream_does_not_show_the_deleted_one_and_owners_are_kept() {
    let sandbox = Sandbox::new();
    let db = MetaDb::open(&sandbox.path("meta.redb")).unwrap();
    history(&db);
    shows_its_own_tree(&db);
    assert_eq!(owner(&db, "//game/t").as_deref(), Some("dave"));
    assert_eq!(owner(&db, "//game/lib").as_deref(), Some("robert"));
    assert_eq!(owner(&db, "//game/main").as_deref(), Some("admin"));
    assert_eq!(owner(&db, "//game/none"), None);
}

#[test]
fn opening_a_format_9_database_drops_leftover_views_and_finds_owners() {
    let sandbox = Sandbox::new();
    let path = sandbox.path("meta.redb");
    let db = MetaDb::open(&path).unwrap();
    history(&db);
    let lib_root = db
        .read()
        .unwrap()
        .stream(&p("//game/lib"))
        .unwrap()
        .unwrap()
        .root;
    drop(db);

    // Format 9 kept the views of deleted streams and knew no owners. The
    // imports were set as changelists 2 and 3.
    {
        let raw = redb::Database::create(&path).unwrap();
        let txn = raw.begin_write().unwrap();
        {
            let mut views = txn.open_table(VIEWS).unwrap();
            let mut imports = txn.open_table(VIEW_IMPORTS).unwrap();
            let none = postcard::to_allocvec(&Vec::<Import>::new()).unwrap();
            for (stream, cl) in [("game/t", 2), ("game/t", 3), ("game/gone", 3)] {
                views
                    .insert((stream, cl), lib_root.0.0.as_slice())
                    .unwrap();
                imports.insert((stream, cl), none.as_slice()).unwrap();
            }
            txn.open_table(META)
                .unwrap()
                .insert("format_version", 9)
                .unwrap();
        }
        txn.delete_table(STREAM_OWNERS).unwrap();
        txn.commit().unwrap();
    }

    let db = MetaDb::open(&path).unwrap();
    shows_its_own_tree(&db);
    assert_eq!(owner(&db, "//game/t").as_deref(), Some("dave"));
    assert_eq!(owner(&db, "//game/lib").as_deref(), Some("robert"));
    assert_eq!(owner(&db, "//game/main").as_deref(), Some("admin"));
    drop(db);
    let raw = redb::Database::create(&path).unwrap();
    let txn = raw.begin_read().unwrap();
    use redb::ReadableTableMetadata;
    assert_eq!(txn.open_table(VIEWS).unwrap().len().unwrap(), 0);
    assert_eq!(txn.open_table(VIEW_IMPORTS).unwrap().len().unwrap(), 0);
}
