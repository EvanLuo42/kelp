use kelp_core::Hash;
use kelp_meta::{Command, Error, LockHolder, MetaDb, ShelfContents};
use kelp_testkit::depot_path;
use kelp_testkit::meta::MetaFixture;

fn create(db: &MetaDb, name: &str, admin: bool) {
    db.commit(Command::CreateUser {
        name: name.into(),
        password_hash: Some("hash".into()),
        admin,
        replica: false,
        at: 1,
    })
    .unwrap();
}

fn token(db: &MetaDb, byte: u8, user: &str) -> Hash {
    let token_hash = Hash([byte; 32]);
    db.commit(Command::IssueToken {
        token_hash,
        user: user.into(),
        expires_at: u64::MAX,
    })
    .unwrap();
    token_hash
}

fn link(db: &MetaDb, user: &str, subject: &str) -> kelp_meta::Result<()> {
    db.commit(Command::LinkSso {
        user: user.into(),
        issuer: "https://idp".into(),
        subject: subject.into(),
    })
    .map(|_| ())
}

#[test]
fn linking_makes_single_sign_on_the_only_way_in() {
    let f = MetaFixture::new();
    let db = f.db();
    create(db, "evan", false);
    create(db, "mallory", false);
    db.commit(Command::SetTotp {
        name: "evan".into(),
        secret: Some("SECRET".into()),
    })
    .unwrap();
    let old = token(db, 1, "evan");
    db.commit(Command::ExpectSso {
        user: "evan".into(),
        sso_name: Some("alice.smith".into()),
    })
    .unwrap();
    assert_eq!(
        db.read()
            .unwrap()
            .expected_sso("Alice.Smith")
            .unwrap()
            .as_deref(),
        Some("evan")
    );

    link(db, "evan", "sub-1").unwrap();
    let r = db.read().unwrap();
    let evan = r.user("evan").unwrap().unwrap();
    assert_eq!((evan.password_hash, evan.totp), (None, None));
    assert!(r.token(&old).unwrap().is_none(), "old tokens are revoked");
    assert_eq!(
        r.sso_user("https://idp", "sub-1").unwrap().as_deref(),
        Some("evan")
    );
    assert_eq!(r.expected_sso("alice.smith").unwrap(), None);
    drop(r);

    let taken = link(db, "mallory", "sub-1").unwrap_err();
    assert!(matches!(taken, Error::AlreadyExists(_)), "{taken:?}");
    let twice = link(db, "evan", "sub-2").unwrap_err();
    assert!(matches!(twice, Error::Invalid(_)), "{twice:?}");
    link(db, "evan", "sub-1").unwrap();

    db.commit(Command::UnlinkSso {
        user: "evan".into(),
    })
    .unwrap();
    assert_eq!(
        db.read().unwrap().sso_user("https://idp", "sub-1").unwrap(),
        None
    );
    link(db, "mallory", "sub-1").unwrap();
}

#[test]
fn merging_moves_what_a_user_holds_and_reserves_the_old_name() {
    let f = MetaFixture::new();
    let db = f.db();
    create(db, "evan", false);
    create(db, "alice", false);
    create(db, "root", true);
    let session = token(db, 1, "evan");
    link(db, "alice", "sub-1").unwrap();
    db.commit(Command::SetGroup {
        name: "artists".into(),
        members: vec!["evan".into(), "alice".into()],
    })
    .unwrap();
    db.commit(Command::SetProtections {
        source: "write * //...\nnone   user:Evan //game/main/Secret/...\n".into(),
    })
    .unwrap();
    let stream = db
        .read()
        .unwrap()
        .stream(&depot_path("//game/main"))
        .unwrap()
        .unwrap();
    let holder = LockHolder {
        user: "evan".into(),
        workspace: "ws".into(),
    };
    db.commit(Command::AcquireLocks {
        holder,
        domain: stream.lock_domain.clone(),
        paths: vec!["a.png".into()],
    })
    .unwrap();
    let shelf = db
        .shelve(
            None,
            ShelfContents {
                stream: depot_path("//game/main"),
                owner: "evan".into(),
                workspace: "ws".into(),
                description: "wip".into(),
                files: vec![f.add("//game/main/a.txt", b"a")],
                at: 1,
            },
        )
        .unwrap();

    let admins = db.commit(Command::MergeUser {
        from: "evan".into(),
        into: "root".into(),
    });
    assert!(
        matches!(admins, Err(Error::Invalid(_))),
        "rights must match"
    );
    db.commit(Command::MergeUser {
        from: "evan".into(),
        into: "alice".into(),
    })
    .unwrap();

    let r = db.read().unwrap();
    assert!(r.user("evan").unwrap().is_none());
    assert_eq!(r.alias("EVAN").unwrap().as_deref(), Some("alice"));
    assert_eq!(
        r.token(&session).unwrap().unwrap().user,
        "alice",
        "sessions carry on"
    );
    assert_eq!(r.groups().unwrap()[0].members, ["alice"]);
    assert_eq!(
        r.locks(&stream.lock_domain).unwrap()[0].holder.user,
        "alice"
    );
    assert_eq!(r.shelf(shelf).unwrap().unwrap().contents.owner, "alice");
    assert_eq!(
        r.protections_source().unwrap().as_deref(),
        Some("write * //...\nnone   user:alice //game/main/Secret/...\n")
    );
    assert_eq!(
        r.sso_user("https://idp", "sub-1").unwrap().as_deref(),
        Some("alice")
    );
    drop(r);

    let reuse = db.commit(Command::CreateUser {
        name: "evan".into(),
        password_hash: None,
        admin: false,
        replica: false,
        at: 2,
    });
    assert!(matches!(reuse, Err(Error::AlreadyExists(_))), "{reuse:?}");
}

#[test]
fn merging_into_a_new_name_renames() {
    let f = MetaFixture::new();
    let db = f.db();
    create(db, "evan", true);
    link(db, "evan", "sub-1").unwrap();
    db.commit(Command::MergeUser {
        from: "evan".into(),
        into: "alice.smith".into(),
    })
    .unwrap();
    let r = db.read().unwrap();
    let user = r.user("alice.smith").unwrap().unwrap();
    assert!(user.admin);
    assert_eq!(r.sso_link("alice.smith").unwrap().unwrap().subject, "sub-1");
    assert_eq!(r.aliases_of("alice.smith").unwrap(), ["evan"]);
    drop(r);

    db.commit(Command::MergeUser {
        from: "alice.smith".into(),
        into: "evan".into(),
    })
    .unwrap();
    let r = db.read().unwrap();
    assert!(
        r.user("evan").unwrap().is_some(),
        "a name can be taken back"
    );
    assert_eq!(r.aliases_of("evan").unwrap(), ["alice.smith"]);
}

#[test]
fn membership_changes_keep_groups_and_batches_are_atomic() {
    use kelp_meta::db::Proposal;
    let f = MetaFixture::new();
    let db = f.db();
    create(db, "ann", false);
    create(db, "ben", false);
    db.commit(Command::SetGroup {
        name: "artists".into(),
        members: vec!["ann".into()],
    })
    .unwrap();
    db.commit(Command::SetMembership {
        group: "artists".into(),
        user: "BEN".into(),
        member: true,
    })
    .unwrap();
    for user in ["ann", "ben"] {
        db.commit(Command::SetMembership {
            group: "artists".into(),
            user: user.into(),
            member: false,
        })
        .unwrap();
    }
    let groups = db.read().unwrap().groups().unwrap();
    assert_eq!(groups.len(), 1, "an emptied group stays");
    assert!(groups[0].members.is_empty());

    let batch = Proposal::Batch(vec![
        (
            "root".into(),
            Command::CreateDepot {
                name: "art".into(),
                at: 1,
            },
        ),
        (
            "root".into(),
            Command::CreateDepot {
                name: "art".into(),
                at: 1,
            },
        ),
    ]);
    assert!(db.propose(&batch).is_err());
    assert!(
        db.read()
            .unwrap()
            .depots()
            .unwrap()
            .iter()
            .all(|d| d.name != "art"),
        "nothing of a failed batch is applied"
    );
}
