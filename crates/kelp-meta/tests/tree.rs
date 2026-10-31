use kelp_core::ContentId;
use kelp_meta::{Dirs, EntryKind, Error, TreeEntry};
use kelp_prolly::MemStore;

fn file(name: &str, data: &[u8]) -> TreeEntry {
    TreeEntry {
        name: name.into(),
        kind: kind(data),
    }
}

fn kind(data: &[u8]) -> EntryKind {
    EntryKind::File {
        content: ContentId::of(data),
        size: data.len() as u64,
        executable: false,
    }
}

#[test]
fn path_updates_and_diff() {
    let store = MemStore::default();
    let dirs = Dirs::new(&store);
    let empty = dirs.empty().unwrap();
    let v1 = dirs
        .update_paths(
            empty,
            vec![
                ("Source/main.rs".into(), Some(kind(b"fn main() {}"))),
                ("Source/render/mod.rs".into(), Some(kind(b"mod a;"))),
                ("Content/Hero.png".into(), Some(kind(b"png"))),
            ],
        )
        .unwrap();
    assert_eq!(
        dirs.lookup(v1, "source/RENDER/mod.rs")
            .unwrap()
            .unwrap()
            .name,
        "mod.rs"
    );
    assert!(dirs.lookup(v1, "Source/missing.rs").unwrap().is_none());

    // Removing the last file of a directory removes the directory.
    let v2 = dirs
        .update_paths(
            v1,
            vec![
                ("Source/render/mod.rs".into(), None),
                ("Content/Hero.png".into(), Some(kind(b"png v2"))),
            ],
        )
        .unwrap();
    assert!(
        dirs.get(
            dirs.lookup(v2, "Source")
                .unwrap()
                .map(|e| match e.kind {
                    EntryKind::Dir { tree } => tree,
                    _ => panic!(),
                })
                .unwrap(),
            "render"
        )
        .unwrap()
        .is_none()
    );

    let diff = dirs.diff_paths(Some(v1), v2).unwrap();
    let paths: Vec<_> = diff
        .iter()
        .map(|d| (d.path.as_str(), d.old.is_some(), d.new.is_some()))
        .collect();
    assert_eq!(
        paths,
        vec![
            ("Content/Hero.png", true, true),
            ("Source/render/mod.rs", true, false)
        ]
    );
    assert_eq!(dirs.diff_paths(None, v1).unwrap().len(), 3);

    // A path cannot be both a file and a directory.
    assert!(matches!(
        dirs.update_paths(v2, vec![("Source".into(), Some(kind(b"x")))]),
        Err(Error::PathConflict(_))
    ));
    assert!(matches!(
        dirs.update_paths(v2, vec![("Content/Hero.png/x".into(), Some(kind(b"x")))]),
        Err(Error::PathConflict(_))
    ));
}

#[test]
fn id_is_order_independent_and_case_insensitive_lookup() {
    let store = MemStore::default();
    let dirs = Dirs::new(&store);
    let a = dirs.build(vec![file("b", b"1"), file("A", b"2")]).unwrap();
    let b = dirs.build(vec![file("A", b"2"), file("b", b"1")]).unwrap();
    assert_eq!(a, b);
    assert_eq!(dirs.get(a, "a").unwrap().unwrap().name, "A");
}

#[test]
fn rejects_case_collisions() {
    let store = MemStore::default();
    let dirs = Dirs::new(&store);
    assert!(
        dirs.build(vec![file("Hero.png", b"1"), file("hero.PNG", b"2")])
            .is_err()
    );
}

#[test]
fn huge_directory_updates_match_rebuild() {
    let store = MemStore::default();
    let dirs = Dirs::new(&store);
    let entries: Vec<_> = (0..50_000u32)
        .map(|i| file(&format!("Actor_{i:05}.uasset"), &i.to_le_bytes()))
        .collect();
    let dir = dirs.build(entries.clone()).unwrap();

    let updated = dirs
        .update(
            dir,
            vec![
                (
                    "actor_00042.UASSET".into(),
                    Some(file("Actor_00042.uasset", b"changed")),
                ),
                ("Actor_00043.uasset".into(), None),
            ],
        )
        .unwrap();

    let mut expected = entries;
    expected[42] = file("Actor_00042.uasset", b"changed");
    expected.remove(43);
    assert_eq!(updated, dirs.build(expected).unwrap());
    assert_eq!(dirs.list(updated).unwrap().len(), 49_999);
}
