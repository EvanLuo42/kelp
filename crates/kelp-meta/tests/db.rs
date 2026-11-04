use kelp_core::{ClNumber, ContentId};
use kelp_meta::{Command, Dirs, EntryKind, Error, LogIndex};
use kelp_testkit::depot_path as p;
use kelp_testkit::meta::MetaFixture;

#[test]
fn submit_history_and_conflicts() {
    let f = MetaFixture::new();
    let hero = "//game/main/Content/Hero.png";

    let mut unknown = f.add("//game/main/a.txt", b"x");
    if let kelp_meta::FileAction::Add { data } = &mut unknown.action {
        data.content = ContentId::of(b"never uploaded");
    }
    assert!(matches!(
        f.submit("//game/main", vec![unknown]),
        Err(Error::MissingContent(_))
    ));

    let cl1 = f.submit("//game/main", vec![f.add(hero, b"one")]).unwrap();
    assert_eq!(cl1, ClNumber(1));
    let cl2 = f
        .submit("//game/main", vec![f.edit(hero, b"one", b"two")])
        .unwrap();
    let stale = f.submit("//game/main", vec![f.edit(hero, b"one", b"three")]);
    assert!(matches!(stale, Err(Error::Conflict(_))));

    let r = f.db().read().unwrap();
    let revs = r.revisions(&p("//GAME/main/content/hero.PNG")).unwrap();
    assert_eq!(
        revs.iter().map(|r| (r.rev, r.cl)).collect::<Vec<_>>(),
        vec![(2, cl2), (1, cl1)]
    );
    assert_eq!(r.changes(Some(&p("//game/main")), 10).unwrap().len(), 2);

    let (at_cl, root1) = r.stream_at(&p("//game/main"), Some(cl1)).unwrap();
    assert_eq!(at_cl, cl1);
    let nodes = r.nodes().unwrap();
    let dirs = Dirs::new(&nodes);
    let entry = dirs.lookup(root1, "Content/Hero.png").unwrap().unwrap();
    assert!(
        matches!(entry.kind, EntryKind::File { content, .. } if content == ContentId::of(b"one"))
    );
    let (_, head) = r.stream_at(&p("//game/main"), None).unwrap();
    assert_eq!(dirs.diff_paths(Some(root1), head).unwrap().len(), 1);
    assert_eq!(r.last_index().unwrap(), LogIndex(4));
}

#[test]
fn users_are_case_insensitive() {
    let f = MetaFixture::new();
    let user = |name: &str| Command::CreateUser {
        name: name.into(),
        password_hash: None,
        admin: false,
        replica: false,
        at: 0,
    };
    f.db().commit(user("Alice")).unwrap();
    assert!(matches!(
        f.db().commit(user("alice")),
        Err(Error::AlreadyExists(_))
    ));
    assert_eq!(
        f.db().read().unwrap().user("ALICE").unwrap().unwrap().name,
        "Alice"
    );
    assert!(f.db().commit(user("bad name")).is_err());
}

#[test]
fn groups_resolve_members() {
    let f = MetaFixture::new();
    for name in ["alice", "bob"] {
        f.db()
            .commit(Command::CreateUser {
                name: name.into(),
                password_hash: None,
                admin: false,
                replica: false,
                at: 0,
            })
            .unwrap();
    }
    let set = |members: &[&str]| Command::SetGroup {
        name: "Artists".into(),
        members: members.iter().map(|m| m.to_string()).collect(),
    };
    assert!(matches!(
        f.db().commit(set(&["alice", "nobody"])),
        Err(Error::NotFound(_))
    ));
    f.db().commit(set(&["ALICE", "bob"])).unwrap();
    assert_eq!(
        f.db().read().unwrap().groups_of("Bob").unwrap(),
        ["Artists"]
    );
    f.db().commit(set(&[])).unwrap();
    assert!(f.db().read().unwrap().groups().unwrap().is_empty());
}

#[test]
fn shelves_share_changelist_numbers() {
    let f = MetaFixture::new();
    let cl1 = f
        .submit("//game/main", vec![f.add("//game/main/a.txt", b"a")])
        .unwrap();
    let contents = kelp_meta::ShelfContents {
        stream: p("//game/main"),
        owner: "alice".into(),
        workspace: "ws".into(),
        description: "wip".into(),
        files: vec![f.edit("//game/main/a.txt", b"a", b"b")],
        at: 3,
    };
    let shelf = f.db().shelve(None, contents.clone()).unwrap();
    assert_eq!(shelf, ClNumber(cl1.0 + 1));
    let cl3 = f
        .submit("//game/main", vec![f.add("//game/main/c.txt", b"c")])
        .unwrap();
    assert_eq!(cl3, ClNumber(shelf.0 + 1));

    let mut updated = contents;
    updated.description = "wip 2".into();
    assert_eq!(f.db().shelve(Some(shelf), updated).unwrap(), shelf);
    assert_eq!(
        f.db()
            .read()
            .unwrap()
            .shelf(shelf)
            .unwrap()
            .unwrap()
            .contents
            .description,
        "wip 2"
    );
    f.db().commit(Command::DeleteShelf { cl: shelf }).unwrap();
    assert!(f.db().read().unwrap().shelf(shelf).unwrap().is_none());

    // Replacing a shelf that does not exist fails, even under the number
    // the next shelf would get.
    let mut contents = updated_contents(&f);
    for missing in [shelf, ClNumber(cl3.0 + 1), ClNumber(cl3.0 + 5)] {
        contents.description = format!("replace {missing}");
        let replaced = f.db().shelve(Some(missing), contents.clone());
        assert!(
            matches!(replaced, Err(Error::NotFound(_))),
            "{missing}: {replaced:?}"
        );
    }
    let next = f.db().shelve(None, contents).unwrap();
    assert_eq!(next, ClNumber(cl3.0 + 1), "no number was used up");
}

fn updated_contents(f: &MetaFixture) -> kelp_meta::ShelfContents {
    kelp_meta::ShelfContents {
        stream: p("//game/main"),
        owner: "alice".into(),
        workspace: "ws".into(),
        description: "wip".into(),
        files: vec![f.edit("//game/main/a.txt", b"a", b"b")],
        at: 4,
    }
}
