mod common;

use std::sync::Arc;

use kelp_testkit::Sandbox;
use kelp_vfs::{LowerTree, Node, Overlay, Projection};

fn names(overlay: &Overlay, dir: &str) -> Vec<String> {
    overlay.list(dir).unwrap().into_keys().collect()
}

#[test]
fn lower_tree_tracks_directories() {
    let (mut lower, _) = common::sample();
    assert!(lower.is_dir("Content/Maps"));
    assert_eq!(lower.children("").collect::<Vec<_>>(), ["Content", "a.txt"]);
    lower.remove("Content/Maps/level.map");
    assert!(!lower.is_dir("Content/Maps"));
    assert_eq!(lower.children("Content").collect::<Vec<_>>(), ["Hero.png"]);
    lower.remove("Content/Hero.png");
    assert!(!lower.is_dir("Content"));
    assert_eq!(lower.children("").collect::<Vec<_>>(), ["a.txt"]);
}

#[test]
fn files_hydrate_only_when_needed() {
    let sandbox = Sandbox::new();
    let (lower, hydrator) = common::sample();
    let hydrator = Arc::new(hydrator);
    let overlay = Overlay::new(sandbox.dir("upper"), lower, hydrator.clone()).unwrap();

    assert_eq!(names(&overlay, ""), ["Content", "a.txt"]);
    assert!(matches!(overlay.node("Content/Hero.png"), Some(Node::LowerFile(f)) if f.size == 3));
    assert!(
        hydrator.hydrated.lock().unwrap().is_empty(),
        "listing and stat do not download"
    );

    let path = overlay.hydrate("Content/Hero.png").unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"png");
    assert_eq!(*hydrator.hydrated.lock().unwrap(), ["Content/Hero.png"]);
    assert!(matches!(
        overlay.node("Content/Hero.png"),
        Some(Node::Upper(_))
    ));
}

#[test]
fn deletes_creates_and_renames() {
    let sandbox = Sandbox::new();
    let (lower, hydrator) = common::sample();
    let overlay = Overlay::new(sandbox.dir("upper"), lower.clone(), Arc::new(hydrator)).unwrap();

    overlay.unlink("a.txt").unwrap();
    assert!(overlay.node("a.txt").is_none());
    overlay.rename("Content", "Assets").unwrap();
    assert_eq!(names(&overlay, ""), ["Assets"]);
    assert_eq!(names(&overlay, "Assets/Maps"), ["level.map"]);
    assert!(overlay.node("Content/Hero.png").is_none());

    overlay.create("Content/new.txt").unwrap();
    assert_eq!(
        names(&overlay, "Content"),
        ["new.txt"],
        "a recreated directory keeps old lower files hidden"
    );
    assert!(overlay.rmdir("Assets").is_err());

    // Whiteouts survive a remount.
    let (_, hydrator) = common::sample();
    let again = Overlay::new(sandbox.path("upper"), lower, Arc::new(hydrator)).unwrap();
    assert!(again.node("a.txt").is_none());
    assert_eq!(names(&again, "Content"), ["new.txt"]);
}

#[test]
fn projecting_replaces_clean_copies_and_lifts_whiteouts() {
    let sandbox = Sandbox::new();
    let (lower, mut hydrator) = common::sample();
    let v2 = common::lower_file(&mut hydrator, b"alpha v2");
    let overlay = Overlay::new(sandbox.dir("upper"), lower, Arc::new(hydrator)).unwrap();

    overlay.hydrate("a.txt").unwrap();
    overlay.project("a.txt", Some(v2)).unwrap();
    assert!(matches!(overlay.node("a.txt"), Some(Node::LowerFile(f)) if f.size == 8));

    overlay.unlink("Content/Hero.png").unwrap();
    overlay.project("Content/Hero.png", Some(v2)).unwrap();
    assert!(overlay.node("Content/Hero.png").is_some());

    overlay.project("Content/Maps/level.map", None).unwrap();
    assert_eq!(names(&overlay, "Content"), ["Hero.png"]);
    assert!(LowerTree::new().is_empty());
}

#[test]
fn projecting_never_discards_local_changes() {
    let sandbox = Sandbox::new();
    let (lower, mut hydrator) = common::sample();
    let v2 = common::lower_file(&mut hydrator, b"alpha v2");
    let overlay = Overlay::new(sandbox.dir("upper"), lower, Arc::new(hydrator)).unwrap();

    let path = overlay.hydrate("a.txt").unwrap();
    std::fs::write(&path, "my edit").unwrap();
    overlay.touched("a.txt");
    assert!(!overlay.project("a.txt", Some(v2)).unwrap(), "left alone");
    assert!(!overlay.project("a.txt", None).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), b"my edit");

    let mut created = overlay.create("new.txt").unwrap();
    std::io::Write::write_all(&mut created, b"new").unwrap();
    assert!(!overlay.project("new.txt", Some(v2)).unwrap());
    assert_eq!(
        std::fs::read(overlay.upper_path("new.txt")).unwrap(),
        b"new"
    );

    overlay.hydrate("Content/Hero.png").unwrap();
    assert!(
        overlay.project("Content/Hero.png", Some(v2)).unwrap(),
        "a clean copy is replaced"
    );
}

#[test]
fn names_kelp_keeps_state_under_are_reserved() {
    let sandbox = Sandbox::new();
    let (lower, hydrator) = common::sample();
    let overlay = Overlay::new(sandbox.dir("upper"), lower, Arc::new(hydrator)).unwrap();
    for name in [".kelp-whiteouts", "Content/.kelp-hydrate-x"] {
        let err = overlay.create(name).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{name}");
    }
    assert!(overlay.rename("a.txt", ".kelp-whiteouts").is_err());
    assert!(overlay.node("a.txt").is_some());
}

#[test]
fn a_directory_created_where_one_was_deleted_starts_empty() {
    let sandbox = Sandbox::new();
    let (lower, hydrator) = common::sample();
    let overlay = Overlay::new(sandbox.dir("upper"), lower, Arc::new(hydrator)).unwrap();
    overlay.unlink("Content/Maps/level.map").unwrap();
    overlay.rmdir("Content/Maps").unwrap();
    overlay.mkdir("Content/Maps").unwrap();
    assert!(
        names(&overlay, "Content/Maps").is_empty(),
        "nothing comes back"
    );
    assert!(overlay.node("Content/Maps/level.map").is_none());

    let reopened = Overlay::new(
        sandbox.path("upper"),
        common::sample().0,
        Arc::new(common::sample().1),
    )
    .unwrap();
    assert!(
        names(&reopened, "Content/Maps").is_empty(),
        "and stays so after a restart"
    );
}
