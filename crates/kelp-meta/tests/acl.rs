use kelp_meta::acl::{Level, Protections, Role, Subject};
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
        "super * //...",
        "role accounts",
        "write someone //...",
        "write *",
    ] {
        assert!(Protections::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn admin_lines_administer_whole_directories_and_roles_add_up() {
    let table = Protections::parse(
        "
        write  *             //...
        admin  group:leads   //game/...
        read   group:leads   //game/main/Secret/...
        none   user:eve      //game/main/Secret/...
        role   accounts      group:it
        role   audit         user:ann
        ",
    )
    .unwrap();
    let leads = ["leads".to_owned()];
    let it = ["IT".to_owned()];
    let lead = Subject {
        user: "lou",
        groups: &leads,
    };
    let ann = Subject {
        user: "Ann",
        groups: &it,
    };
    let eve = Subject {
        user: "eve",
        groups: &[],
    };

    assert_eq!(table.level(lead, &p("//game/main/a.txt")), Level::Admin);
    assert!(table.level(lead, &p("//game/main/a.txt")) >= Level::Write);
    assert_eq!(table.dir_level(lead, &p("//game")), Level::Admin);
    assert_eq!(table.dir_level(lead, &p("//game/dev")), Level::Admin);
    // A line below a directory does not take its administration away.
    assert_eq!(table.dir_level(lead, &p("//game/main")), Level::Admin);
    assert_eq!(table.dir_level(lead, &p("//engine")), Level::Write);
    assert_eq!(table.dir_level(ann, &p("//game")), Level::Write);

    assert!(table.has_role(ann, Role::Accounts));
    assert!(table.has_role(ann, Role::Audit));
    assert!(!table.has_role(ann, Role::Operations));
    assert_eq!(table.roles(lead), Vec::<Role>::new());

    assert!(table.reads_everything(ann));
    assert!(table.reads_everything(lead));
    assert!(!table.reads_everything(eve));
    assert!(
        !Protections::parse("read * //game/...")
            .unwrap()
            .reads_everything(ann)
    );

    assert!(Protections::parse("role wizard *").is_err());
    assert!(Protections::parse("admin * //game/...").is_ok());
}
