//! Format 10 folds names one character at a time; databases of format 9
//! keyed a word-final sigma differently and are re-keyed when opened.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use kelp_core::{ClNumber, ContentId, DepotPath, FileManifest, TreeId};
use kelp_meta::db::FORMAT_VERSION;
use kelp_meta::record::{Changelist, Stream};
use kelp_meta::{
    Command, Dirs, EntryKind, FileAction, FileChange, FileData, LockDomain, LockHolder, MetaDb,
    StreamKind, Submission, TreeEntry,
};
use kelp_prolly::{ChunkConfig, Node, NodeId, NodeStore, Prolly};
use kelp_testkit::{Sandbox, depot_path};
use redb::{ReadableTable, TableDefinition};

type Bytes = &'static [u8];
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const NODES: TableDefinition<Bytes, Bytes> = TableDefinition::new("nodes");
const STREAMS: TableDefinition<&str, Bytes> = TableDefinition::new("streams");
const CHANGES: TableDefinition<u64, Bytes> = TableDefinition::new("changes");
const REVISIONS: TableDefinition<(&str, u32), Bytes> = TableDefinition::new("revisions");
const LOCKS: TableDefinition<(&str, &str), Bytes> = TableDefinition::new("locks");

#[derive(Default)]
struct Nodes(RefCell<HashMap<NodeId, Arc<Node>>>);

impl NodeStore for Nodes {
    fn get(&self, id: &NodeId) -> kelp_prolly::Result<Arc<Node>> {
        self.0
            .borrow()
            .get(id)
            .cloned()
            .ok_or(kelp_prolly::Error::Missing(*id))
    }

    fn put(&self, node: Node) -> kelp_prolly::Result<NodeId> {
        let id = node.id();
        self.0.borrow_mut().insert(id, Arc::new(node));
        Ok(id)
    }
}

/// Builds a directory keyed by `key` as given, the way format 9 did.
fn old_dir(nodes: &Nodes, entries: Vec<(&str, TreeEntry)>) -> TreeId {
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = entries
        .into_iter()
        .map(|(k, e)| (k.as_bytes().to_vec(), postcard::to_allocvec(&e).unwrap()))
        .collect();
    entries.sort();
    TreeId(
        Prolly::new(nodes, ChunkConfig::DEFAULT)
            .build(entries)
            .unwrap(),
    )
}

fn data(db: &MetaDb, bytes: &[u8]) -> FileData {
    let content = ContentId::of(bytes);
    db.put_manifest(&FileManifest {
        content,
        size: bytes.len() as u64,
        chunks: Vec::new(),
    })
    .unwrap();
    FileData {
        content,
        size: bytes.len() as u64,
        executable: false,
    }
}

fn file(d: FileData) -> EntryKind {
    EntryKind::File {
        content: d.content,
        size: d.size,
        executable: d.executable,
    }
}

fn add(db: &MetaDb, path: &str, bytes: &[u8]) -> FileChange {
    FileChange {
        path: depot_path(path),
        action: FileAction::Add {
            data: data(db, bytes),
        },
    }
}

fn submit(db: &MetaDb, changes: Vec<FileChange>) -> ClNumber {
    let submission = Submission {
        stream: depot_path("//game/main"),
        author: "alice".into(),
        workspace: "ws".into(),
        description: "test".into(),
        changes,
        integration: None,
        issues: Vec::new(),
        fixes: Vec::new(),
        review: None,
        at: 2,
    };
    db.submit(submission).unwrap().1
}

fn create(path: &std::path::Path) -> MetaDb {
    let db = MetaDb::open(path).unwrap();
    db.commit(Command::CreateDepot {
        name: "game".into(),
        at: 1,
    })
    .unwrap();
    db.commit(Command::CreateStream {
        path: depot_path("//game/main"),
        kind: StreamKind::Mainline,
        parent: None,
        at: 1,
    })
    .unwrap();
    db
}

/// Rewrites, in the raw database, the stream `//game/main` and its only
/// changelist `cl` to `root`, moves the rows of `revisions` and `locks` to
/// the keys format 9 used, and stamps the database with format 9.
fn make_old(
    path: &std::path::Path,
    nodes: Nodes,
    root: TreeId,
    cl: ClNumber,
    revisions: &[(&str, &str)],
    locks: &[(&str, &str)],
) {
    let raw = redb::Database::create(path).unwrap();
    let txn = raw.begin_write().unwrap();
    {
        let mut table = txn.open_table(NODES).unwrap();
        for (id, node) in nodes.0.borrow().iter() {
            table
                .insert(id.0.as_slice(), node.encode().as_slice())
                .unwrap();
        }
        let mut streams = txn.open_table(STREAMS).unwrap();
        let mut stream: Stream =
            postcard::from_bytes(streams.get("game/main").unwrap().unwrap().value()).unwrap();
        stream.root = root;
        streams
            .insert(
                "game/main",
                postcard::to_allocvec(&stream).unwrap().as_slice(),
            )
            .unwrap();
        let mut changes = txn.open_table(CHANGES).unwrap();
        let mut change: Changelist =
            postcard::from_bytes(changes.get(cl.0).unwrap().unwrap().value()).unwrap();
        change.root = root;
        changes
            .insert(cl.0, postcard::to_allocvec(&change).unwrap().as_slice())
            .unwrap();
        let mut table = txn.open_table(REVISIONS).unwrap();
        for (new, old) in revisions {
            let value = table.remove((*new, 1)).unwrap().unwrap().value().to_vec();
            table.insert((*old, 1), value.as_slice()).unwrap();
        }
        let mut table = txn.open_table(LOCKS).unwrap();
        for (new, old) in locks {
            let value = table
                .remove(("game/main", *new))
                .unwrap()
                .unwrap()
                .value()
                .to_vec();
            table.insert(("game/main", *old), value.as_slice()).unwrap();
        }
        txn.open_table(META)
            .unwrap()
            .insert("format_version", 9)
            .unwrap();
    }
    txn.commit().unwrap();
}

#[test]
fn a_final_sigma_is_rekeyed_when_a_format_9_database_opens() {
    let sandbox = Sandbox::new();
    let path = sandbox.path("meta.redb");
    let db = create(&path);
    let (sigma, plain) = (data(&db, b"sigma"), data(&db, b"plain"));
    let cl = submit(
        &db,
        vec![
            add(&db, "//game/main/Docs/ΟΔΟΣ.txt", b"sigma"),
            add(&db, "//game/main/plain.txt", b"plain"),
        ],
    );
    db.commit(Command::AcquireLocks {
        holder: LockHolder {
            user: "alice".into(),
            workspace: "ws".into(),
        },
        domain: LockDomain("game/main".into()),
        paths: vec!["Docs/ΟΔΟΣ.txt".into()],
    })
    .unwrap();
    drop(db);

    // Format 9 lowercased "ΟΔΟΣ" to "οδος" with a final sigma.
    let nodes = Nodes::default();
    let docs = old_dir(
        &nodes,
        vec![(
            "οδος.txt",
            TreeEntry {
                name: "ΟΔΟΣ.txt".into(),
                kind: file(sigma),
            },
        )],
    );
    let root = old_dir(
        &nodes,
        vec![
            (
                "docs",
                TreeEntry {
                    name: "Docs".into(),
                    kind: EntryKind::Dir { tree: docs },
                },
            ),
            (
                "plain.txt",
                TreeEntry {
                    name: "plain.txt".into(),
                    kind: file(plain),
                },
            ),
        ],
    );
    make_old(
        &path,
        nodes,
        root,
        cl,
        &[("game/main/docs/οδοσ.txt", "game/main/docs/οδος.txt")],
        &[("docs/οδοσ.txt", "docs/οδος.txt")],
    );

    let db = MetaDb::open(&path).unwrap();
    let r = db.read().unwrap();
    assert_eq!(r.format_version().unwrap(), FORMAT_VERSION);
    let file = DepotPath::parse("//game/main/docs/οδοσ.txt").unwrap();
    assert_eq!(r.revisions(&file).unwrap().len(), 1);
    assert_eq!(
        r.revisions(&file).unwrap()[0].path.as_str(),
        "//game/main/Docs/ΟΔΟΣ.txt"
    );
    let stream = r.stream(&depot_path("//game/main")).unwrap().unwrap();
    assert_ne!(stream.root, root, "the tree is re-keyed");
    let nodes = r.nodes().unwrap();
    let dirs = Dirs::new(&nodes);
    for spelling in ["Docs/ΟΔΟΣ.txt", "docs/οδος.txt", "DOCS/οδοσ.txt"] {
        let entry = dirs.lookup(stream.root, spelling).unwrap().unwrap();
        assert_eq!(entry.name, "ΟΔΟΣ.txt");
    }
    assert!(dirs.lookup(stream.root, "plain.txt").unwrap().is_some());
    assert_eq!(
        r.changelist(cl).unwrap().unwrap().root,
        stream.root,
        "the changelist's tree is re-keyed too"
    );
    let lock = r
        .lock(&stream.lock_domain, "docs/οδος.txt")
        .unwrap()
        .expect("the lock is found under its new key");
    assert_eq!(lock.path, "Docs/ΟΔΟΣ.txt");
    drop(r);

    // The file can be edited and the lock released by its new spelling.
    let edit = FileChange {
        path: depot_path("//game/main/docs/οδοσ.txt"),
        action: FileAction::Edit {
            base: sigma.content,
            data: data(&db, b"edited"),
        },
    };
    let next = submit(&db, vec![edit]);
    let r = db.read().unwrap();
    assert_eq!(r.revisions(&file).unwrap().len(), 2);
    assert_eq!(
        r.stream_at(&depot_path("//game/main"), None).unwrap().0,
        next
    );
    r.verify_audit().unwrap();
}

#[test]
fn names_that_only_now_fold_alike_are_both_kept() {
    let sandbox = Sandbox::new();
    let path = sandbox.path("meta.redb");
    let db = create(&path);
    let (a, b) = (data(&db, b"a"), data(&db, b"b"));
    // With the new folding these two would collide, so add the second
    // under a placeholder name and rename it in the old-format tree.
    let cl = submit(
        &db,
        vec![
            add(&db, "//game/main/ασ.txt", b"a"),
            add(&db, "//game/main/other.txt", b"b"),
        ],
    );
    drop(db);
    let nodes = Nodes::default();
    let root = old_dir(
        &nodes,
        vec![
            (
                "ασ.txt",
                TreeEntry {
                    name: "ασ.txt".into(),
                    kind: file(a),
                },
            ),
            (
                "ας.txt",
                TreeEntry {
                    name: "ας.txt".into(),
                    kind: file(b),
                },
            ),
        ],
    );
    {
        // Give the second file's history the old key of its new name.
        let raw = redb::Database::create(&path).unwrap();
        let txn = raw.begin_write().unwrap();
        {
            let mut table = txn.open_table(REVISIONS).unwrap();
            let rev = table
                .remove(("game/main/other.txt", 1))
                .unwrap()
                .unwrap()
                .value()
                .to_vec();
            table
                .insert(("game/main/ας.txt", 1), rev.as_slice())
                .unwrap();
        }
        txn.commit().unwrap();
    }
    make_old(&path, nodes, root, cl, &[], &[]);

    let db = MetaDb::open(&path).unwrap();
    let r = db.read().unwrap();
    let stream = r.stream(&depot_path("//game/main")).unwrap().unwrap();
    let nodes = r.nodes().unwrap();
    let dirs = Dirs::new(&nodes);
    let names: Vec<String> = dirs
        .list(stream.root)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names.len(), 2, "neither entry is lost: {names:?}");
    assert!(names.contains(&"ας.txt".to_owned()));
    let found = dirs.lookup(stream.root, "ας.txt").unwrap().unwrap();
    assert_eq!(found.name, "ασ.txt", "the one spelled like its key wins");
    let mut revisions = 0;
    let raw = DepotPath::parse("//game/main/ασ.txt").unwrap();
    revisions += r.revisions(&raw).unwrap().len();
    assert_eq!(revisions, 1);
    drop((nodes, r, db));
    let raw = redb::Database::create(&path).unwrap();
    let txn = raw.begin_read().unwrap();
    let table = txn.open_table(REVISIONS).unwrap();
    assert!(
        table.get(("game/main/ας.txt", 1)).unwrap().is_some(),
        "the losing history keeps its old key"
    );
    assert!(table.get(("game/main/ασ.txt", 1)).unwrap().is_some());
    assert_eq!(table.iter().unwrap().count(), 2);
}
