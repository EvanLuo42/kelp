use kelp_workspace::hunk::{apply_hunks, diff_hunks};

const BASE: &str = "fn a() {}\n\nfn b() {}\n\nfn c() {}\n\nfn d() {}\n\nfn e() {}\n";

#[test]
fn apply_subset() {
    let working = BASE
        .replace("fn a() {}", "fn a() { 1 }")
        .replace("fn e() {}", "fn e() { 5 }");
    let hunks = diff_hunks(BASE, &working);
    assert_eq!(hunks.len(), 2);
    assert_eq!(apply_hunks(BASE, &hunks), working);
    assert_eq!(
        apply_hunks(BASE, &hunks[..1]),
        BASE.replace("fn a() {}", "fn a() { 1 }")
    );
    assert_eq!(
        apply_hunks(BASE, &hunks[1..]),
        BASE.replace("fn e() {}", "fn e() { 5 }")
    );
}

#[test]
fn anchor_stable_while_editing_inside_hunk() {
    let v1 = BASE.replace("fn c() {}", "fn c() { todo!() }");
    let v2 = BASE.replace("fn c() {}", "fn c() {\n    compute()\n}");
    assert_eq!(
        diff_hunks(BASE, &v1)[0].anchor,
        diff_hunks(BASE, &v2)[0].anchor
    );
}
