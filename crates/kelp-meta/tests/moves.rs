use kelp_meta::FileChange;
use kelp_meta::db::Proposal;
use kelp_testkit::depot_path as p;
use kelp_testkit::meta::MetaFixture;

fn move_file(f: &MetaFixture, changes: Vec<FileChange>, from: &str, to: &str) {
    let submission = kelp_meta::Submission {
        stream: p("//game/main"),
        author: "alice".into(),
        workspace: "ws".into(),
        description: "move".into(),
        changes,
        integration: None,
        issues: Vec::new(),
        fixes: Vec::new(),
        review: None,
        at: 3,
    };
    let moves = vec![(p(from), p(to))];
    f.db()
        .propose(&Proposal::SubmitMoves { submission, moves })
        .unwrap();
}

fn history(f: &MetaFixture, path: &str) -> Vec<(String, u64)> {
    f.db()
        .read()
        .unwrap()
        .history(&p(path))
        .unwrap()
        .iter()
        .map(|r| (r.path.to_string(), r.cl.0))
        .collect()
}

fn own(seen: &[(&str, u64)]) -> Vec<(String, u64)> {
    seen.iter().map(|(p, cl)| (p.to_string(), *cl)).collect()
}

#[test]
fn a_file_moved_onto_a_deleted_path_keeps_its_history() {
    let f = MetaFixture::new();
    let (a, b) = ("//game/main/a.txt", "//game/main/b.txt");
    f.submit("//game/main", vec![f.add(a, b"old")]).unwrap(); // 1
    f.submit("//game/main", vec![f.delete(a, b"old")]).unwrap(); // 2
    f.submit("//game/main", vec![f.add(b, b"b1")]).unwrap(); // 3
    f.submit("//game/main", vec![f.edit(b, b"b1", b"b2")])
        .unwrap(); // 4
    move_file(&f, vec![f.delete(b, b"b2"), f.add(a, b"b2")], b, a); // 5
    assert_eq!(history(&f, a), own(&[(a, 5), (b, 4), (b, 3)]));
}

#[test]
fn a_file_moved_away_and_back_keeps_its_whole_history() {
    let f = MetaFixture::new();
    let (a, b) = ("//game/main/a.txt", "//game/main/b.txt");
    f.submit("//game/main", vec![f.add(a, b"v1")]).unwrap(); // 1
    f.submit("//game/main", vec![f.edit(a, b"v1", b"v2")])
        .unwrap(); // 2
    move_file(&f, vec![f.delete(a, b"v2"), f.add(b, b"v2")], a, b); // 3
    f.submit("//game/main", vec![f.edit(b, b"v2", b"v3")])
        .unwrap(); // 4
    move_file(&f, vec![f.delete(b, b"v3"), f.add(a, b"v3")], b, a); // 5
    assert_eq!(
        history(&f, a),
        own(&[(a, 5), (b, 4), (b, 3), (a, 2), (a, 1)])
    );
}

#[test]
fn a_path_re_added_without_a_move_keeps_all_its_revisions() {
    let f = MetaFixture::new();
    let a = "//game/main/a.txt";
    f.submit("//game/main", vec![f.add(a, b"1")]).unwrap();
    f.submit("//game/main", vec![f.delete(a, b"1")]).unwrap();
    f.submit("//game/main", vec![f.add(a, b"2")]).unwrap();
    assert_eq!(history(&f, a), own(&[(a, 3), (a, 2), (a, 1)]));
}
