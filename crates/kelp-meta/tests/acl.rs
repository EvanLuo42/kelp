use kelp_meta::acl::{Level, Protections, Subject};
use kelp_testkit::depot_path as p;

const TABLE: &str = "
    write  *               //...
    read   group:artists   //engine/...
    none   user:bob        //game/main/Secret/...
    write  user:bob        //game/main/Secret/bob.txt
";

#[test]
fn last_matching_line_wins() {
    let table = Protections::parse(TABLE).unwrap();
    let artists = ["Artists".to_owned()];
    let bob = Subject {
        user: "Bob",
        groups: &[],
    };
    let ann = Subject {
        user: "ann",
        groups: &artists,
    };

    assert_eq!(table.level(bob, &p("//game/main/a.txt")), Level::Write);
    assert_eq!(
        table.level(bob, &p("//game/main/secret/plan.txt")),
        Level::None
    );
    assert_eq!(
        table.level(bob, &p("//game/main/Secret/bob.txt")),
        Level::Write
    );
    assert_eq!(
        table.level(ann, &p("//engine/main/src/lib.rs")),
        Level::Read
    );
    assert_eq!(
        table.level(bob, &p("//engine/main/src/lib.rs")),
        Level::Write
    );
}

#[test]
fn visibility_of_directories() {
    let table = Protections::parse("read user:ann //game/main/Docs/...").unwrap();
    let ann = Subject {
        user: "ann",
        groups: &[],
    };
    assert!(table.may_read_below(ann, &p("//game/main")));
    assert!(!table.may_read_below(ann, &p("//engine/main")));
    assert_eq!(table.level(ann, &p("//game/main/a.txt")), Level::None);
}

#[test]
fn rejects_malformed_lines() {
    for bad in [
        "write * //game/main/a...",
        "admin * //...",
        "write someone //...",
        "write *",
    ] {
        assert!(Protections::parse(bad).is_err(), "{bad}");
    }
}
