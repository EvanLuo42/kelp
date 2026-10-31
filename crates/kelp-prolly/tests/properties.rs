use std::collections::BTreeMap;

use kelp_prolly::{ChunkConfig, Diff, MemStore, NodeId, NodeStore, Prolly};
use proptest::prelude::*;

/// Tiny nodes so that small inputs still produce several levels.
const SMALL: ChunkConfig = ChunkConfig { min: 64, max: 256 };

type Map = BTreeMap<Vec<u8>, Vec<u8>>;
type Batch = Vec<(Vec<u8>, Option<Vec<u8>>)>;

fn key() -> impl Strategy<Value = Vec<u8>> {
    // Small key space so edits frequently hit existing keys.
    (0u16..600).prop_map(|k| format!("dir/file-{k:04}").into_bytes())
}

fn value() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..24)
}

fn map() -> impl Strategy<Value = Map> {
    prop::collection::btree_map(key(), value(), 0..400)
}

fn batches() -> impl Strategy<Value = Vec<Batch>> {
    prop::collection::vec(
        prop::collection::vec((key(), prop::option::of(value())), 0..40),
        1..6,
    )
}

fn height(store: &MemStore, root: NodeId) -> u8 {
    store.get(&root).unwrap().level
}

fn check_nodes(store: &MemStore, id: NodeId, is_root: bool) {
    let node = store.get(&id).unwrap();
    let payload: usize = node
        .entries
        .iter()
        .map(|(k, v)| k.len() + v.len() + 4)
        .sum();
    let largest = node
        .entries
        .iter()
        .map(|(k, v)| k.len() + v.len() + 4)
        .max()
        .unwrap_or(0);
    assert!(
        payload < SMALL.max as usize + largest,
        "node of {payload} bytes exceeds bound"
    );
    if is_root && node.level > 0 {
        assert!(
            node.entries.len() > 1,
            "root with a single child is not canonical"
        );
    }
    if !is_root {
        assert!(!node.entries.is_empty(), "only the root may be empty");
    }
    if node.level > 0 {
        for (i, (first, _)) in node.entries.iter().enumerate() {
            let child_id = kelp_core::Hash(node.entries[i].1.as_slice().try_into().unwrap());
            let child = store.get(&child_id).unwrap();
            assert_eq!(child.level + 1, node.level);
            assert_eq!(
                &child.entries[0].0, first,
                "parent key must be the child's first key"
            );
            check_nodes(store, child_id, false);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Incremental edits in any batching give the same root as a bulk build.
    #[test]
    fn history_independent(initial in map(), batches in batches()) {
        let store = MemStore::default();
        let tree = Prolly::new(&store, SMALL);

        let mut expected = initial.clone();
        let mut root = tree.build(initial)?;
        for batch in batches {
            for (k, v) in &batch {
                match v {
                    Some(v) => expected.insert(k.clone(), v.clone()),
                    None => expected.remove(k),
                };
            }
            root = tree.apply(root, batch)?;
            check_nodes(&store, root, true);
        }

        let rebuilt = tree.build(expected.clone())?;
        prop_assert_eq!(root, rebuilt);
        prop_assert_eq!(tree.entries(root)?, expected.clone().into_iter().collect::<Vec<_>>());
        for (k, v) in expected.iter().take(50) {
            prop_assert_eq!(tree.get(root, k)?, Some(v.clone()));
        }
        prop_assert_eq!(tree.get(root, b"absent")?, None);
    }

    /// Diff reports exactly the changed keys, in order.
    #[test]
    fn diff_matches_model(a in map(), b in map()) {
        let store = MemStore::default();
        let tree = Prolly::new(&store, SMALL);
        let (ra, rb) = (tree.build(a.clone())?, tree.build(b.clone())?);

        let mut expected = Vec::new();
        let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
        for k in keys {
            match (a.get(k), b.get(k)) {
                (Some(x), None) => expected.push(Diff::Removed(k.clone(), x.clone())),
                (None, Some(y)) => expected.push(Diff::Added(k.clone(), y.clone())),
                (Some(x), Some(y)) if x != y => expected.push(Diff::Modified(k.clone(), x.clone(), y.clone())),
                _ => {}
            }
        }
        prop_assert_eq!(tree.diff(ra, rb)?, expected);
        prop_assert!(tree.diff(ra, ra)?.is_empty());
    }
}

#[test]
fn small_edit_rewrites_few_nodes() {
    let store = MemStore::default();
    let tree = Prolly::new(&store, SMALL);
    let entries: Vec<_> = (0..20_000u32)
        .map(|i| (format!("k{i:06}").into_bytes(), vec![1; 8]))
        .collect();
    let root = tree.build(entries).unwrap();
    assert!(height(&store, root) >= 3);

    let before = store.len();
    let edited = tree
        .apply(root, [(b"k010000".to_vec(), Some(vec![2; 8]))])
        .unwrap();
    let written = store.len() - before;
    assert!(
        written <= 3 * (height(&store, edited) as usize + 1),
        "rewrote {written} nodes"
    );

    let diff = tree.diff(root, edited).unwrap();
    assert_eq!(
        diff,
        vec![Diff::Modified(b"k010000".to_vec(), vec![1; 8], vec![2; 8])]
    );
}

#[test]
fn delete_everything_gives_empty_tree() {
    let store = MemStore::default();
    let tree = Prolly::new(&store, SMALL);
    let entries: Vec<_> = (0..500u32)
        .map(|i| (i.to_be_bytes().to_vec(), vec![0; 4]))
        .collect();
    let root = tree.build(entries.clone()).unwrap();
    let emptied = tree
        .apply(root, entries.into_iter().map(|(k, _)| (k, None)))
        .unwrap();
    assert_eq!(emptied, tree.empty().unwrap());
}
