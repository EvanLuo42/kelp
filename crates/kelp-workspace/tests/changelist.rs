use kelp_core::LocalClId;
use kelp_testkit::depot_path as p;
use kelp_workspace::hunk::{self, HunkAnchor};
use kelp_workspace::{Error, OpenAction, PendingChanges, Selection};

const BASE: &str = "a\n\n\n\nb\n\n\n\nc\n";

#[test]
fn hunks_split_across_changelists() {
    let mut pc = PendingChanges::default();
    let fix = pc.create("fix crash");
    let feature = pc.create("new feature");
    let path = p("//engine/main/src/renderer.rs");
    pc.open(path.clone(), OpenAction::Edit, feature, true)
        .unwrap();

    let working = "a2\n\n\n\nb\n\n\n\nc2\n";
    let hunks = hunk::diff_hunks(BASE, working);
    pc.assign_hunk(&path, hunks[0].anchor, fix).unwrap();

    assert_eq!(
        pc.compose_text(fix, &path, BASE, working).unwrap(),
        "a2\n\n\n\nb\n\n\n\nc\n"
    );
    assert_eq!(
        pc.compose_text(feature, &path, BASE, working).unwrap(),
        "a\n\n\n\nb\n\n\n\nc2\n"
    );

    let anchors = |_: &kelp_core::DepotPath| hunks.iter().map(|h| h.anchor).collect();
    assert!(matches!(&pc.contents(fix, anchors)[..], [(_, Selection::Hunks(s))] if s.len() == 1));
    assert!(pc.contents(LocalClId::DEFAULT, anchors).is_empty());

    // Submitting `fix` keeps the file open in `feature`.
    pc.complete_submit(fix, &[]);
    assert!(pc.changelist(fix).is_none());
    assert_eq!(
        pc.contents(feature, anchors),
        vec![(path, Selection::Whole)]
    );
}

#[test]
fn binary_files_cannot_split() {
    let mut pc = PendingChanges::default();
    let cl = pc.create("art");
    let path = p("//game/main/Hero.png");
    pc.open(path.clone(), OpenAction::Edit, LocalClId::DEFAULT, false)
        .unwrap();
    assert!(matches!(
        pc.assign_hunk(&path, HunkAnchor([0; 16]), cl),
        Err(Error::NotSplittable(_))
    ));
}

#[test]
fn deleting_changelist_moves_files_to_default() {
    let mut pc = PendingChanges::default();
    let cl = pc.create("tmp");
    let path = p("//game/main/a.txt");
    pc.open(path.clone(), OpenAction::Add, cl, true).unwrap();
    pc.delete(cl).unwrap();
    assert_eq!(
        pc.contents(LocalClId::DEFAULT, |_| vec![]),
        vec![(path, Selection::Whole)]
    );
    assert!(pc.delete(LocalClId::DEFAULT).is_err());
}
