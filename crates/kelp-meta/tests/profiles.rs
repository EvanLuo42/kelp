use kelp_meta::{Command, Error, MetaDb, ProfileEdit};
use kelp_testkit::meta::MetaFixture;

fn create(db: &MetaDb, name: &str) {
    db.commit(Command::CreateUser {
        name: name.into(),
        password_hash: Some("hash".into()),
        admin: false,
        replica: false,
        at: 1,
    })
    .unwrap();
}

fn edit(db: &MetaDb, user: &str, edit: ProfileEdit) -> kelp_meta::Result<()> {
    db.commit(Command::EditProfile {
        user: user.into(),
        edit,
    })
    .map(|_| ())
}

fn add(db: &MetaDb, user: &str, email: &str) -> kelp_meta::Result<()> {
    edit(db, user, ProfileEdit::AddEmail(email.into()))
}

#[test]
fn a_user_without_a_profile_has_an_empty_one() {
    let f = MetaFixture::new();
    create(f.db(), "evan");
    let profile = f.db().read().unwrap().profile_of("evan").unwrap();
    assert_eq!(profile.display_name, None);
    assert!(profile.emails.is_empty());
}

#[test]
fn display_name_is_trimmed_and_can_be_cleared() {
    let f = MetaFixture::new();
    let db = f.db();
    create(db, "evan");
    edit(
        db,
        "evan",
        ProfileEdit::DisplayName(Some("  Evan Luo ".into())),
    )
    .unwrap();
    let r = db.read().unwrap();
    assert_eq!(
        r.profile_of("EVAN").unwrap().display_name.as_deref(),
        Some("Evan Luo")
    );

    for bad in ["", "   ", "line\nbreak", &"x".repeat(65)] {
        let result = edit(db, "evan", ProfileEdit::DisplayName(Some(bad.into())));
        assert!(matches!(result, Err(Error::Invalid(_))), "{bad:?}");
    }
    edit(db, "evan", ProfileEdit::DisplayName(None)).unwrap();
    assert_eq!(
        db.read().unwrap().profile_of("evan").unwrap().display_name,
        None
    );
}

#[test]
fn a_user_may_have_several_addresses_and_the_first_is_primary() {
    let f = MetaFixture::new();
    let db = f.db();
    create(db, "evan");
    add(db, "evan", "evan@work.example").unwrap();
    add(db, "evan", "Evan@Home.example").unwrap();
    let r = db.read().unwrap();
    assert_eq!(
        r.profile_of("evan").unwrap().emails,
        ["evan@work.example", "Evan@Home.example"]
    );

    edit(
        db,
        "evan",
        ProfileEdit::PrimaryEmail("evan@home.example".into()),
    )
    .unwrap();
    assert_eq!(
        db.read().unwrap().profile_of("evan").unwrap().emails,
        ["Evan@Home.example", "evan@work.example"]
    );

    let missing = edit(
        db,
        "evan",
        ProfileEdit::PrimaryEmail("nobody@x.example".into()),
    );
    assert!(matches!(missing, Err(Error::NotFound(_))));
}

#[test]
fn an_address_belongs_to_one_user_regardless_of_case() {
    let f = MetaFixture::new();
    let db = f.db();
    create(db, "evan");
    create(db, "alice");
    add(db, "evan", "shared@x.example").unwrap();

    for user in ["alice", "evan"] {
        let again = add(db, user, "SHARED@x.example");
        assert!(matches!(again, Err(Error::AlreadyExists(_))), "{user}");
    }
    let r = db.read().unwrap();
    assert_eq!(
        r.email_owner("Shared@X.example").unwrap().as_deref(),
        Some("evan")
    );
    assert!(r.profile_of("alice").unwrap().emails.is_empty());

    edit(
        db,
        "evan",
        ProfileEdit::RemoveEmail("shared@x.example".into()),
    )
    .unwrap();
    assert_eq!(
        db.read().unwrap().email_owner("shared@x.example").unwrap(),
        None
    );
    add(db, "alice", "shared@x.example").unwrap();
}

#[test]
fn malformed_addresses_and_unknown_users_are_rejected() {
    let f = MetaFixture::new();
    let db = f.db();
    create(db, "evan");
    for bad in ["", "plain", "@x.example", "evan@", "a@b@c", "a b@x.example"] {
        assert!(
            matches!(add(db, "evan", bad), Err(Error::Invalid(_))),
            "{bad:?}"
        );
    }
    assert!(matches!(
        add(db, "ghost", "g@x.example"),
        Err(Error::NotFound(_))
    ));
    let removed = edit(
        db,
        "evan",
        ProfileEdit::RemoveEmail("none@x.example".into()),
    );
    assert!(matches!(removed, Err(Error::NotFound(_))));
}

#[test]
fn a_user_has_a_bounded_number_of_addresses() {
    let f = MetaFixture::new();
    let db = f.db();
    create(db, "evan");
    for i in 0..16 {
        add(db, "evan", &format!("e{i}@x.example")).unwrap();
    }
    assert!(matches!(
        add(db, "evan", "one-more@x.example"),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn merging_keeps_the_target_name_and_joins_the_addresses() {
    let f = MetaFixture::new();
    let db = f.db();
    create(db, "evan");
    create(db, "alice");
    edit(db, "evan", ProfileEdit::DisplayName(Some("Evan".into()))).unwrap();
    add(db, "evan", "evan@x.example").unwrap();
    edit(db, "alice", ProfileEdit::DisplayName(Some("Alice".into()))).unwrap();
    add(db, "alice", "alice@x.example").unwrap();

    db.commit(Command::MergeUser {
        from: "evan".into(),
        into: "alice".into(),
    })
    .unwrap();

    let r = db.read().unwrap();
    let merged = r.profile_of("alice").unwrap();
    assert_eq!(merged.display_name.as_deref(), Some("Alice"));
    assert_eq!(merged.emails, ["alice@x.example", "evan@x.example"]);
    assert_eq!(
        r.email_owner("evan@x.example").unwrap().as_deref(),
        Some("alice")
    );
    // History recorded under the old name finds the profile of its successor.
    assert_eq!(r.profile_of("evan").unwrap(), merged);
}

#[test]
fn renaming_a_user_carries_the_profile_along() {
    let f = MetaFixture::new();
    let db = f.db();
    create(db, "evan");
    edit(
        db,
        "evan",
        ProfileEdit::DisplayName(Some("Evan Luo".into())),
    )
    .unwrap();
    add(db, "evan", "evan@x.example").unwrap();

    db.commit(Command::MergeUser {
        from: "evan".into(),
        into: "evan.luo".into(),
    })
    .unwrap();

    let r = db.read().unwrap();
    let profile = r.profile_of("evan.luo").unwrap();
    assert_eq!(profile.display_name.as_deref(), Some("Evan Luo"));
    assert_eq!(profile.emails, ["evan@x.example"]);
    assert_eq!(
        r.email_owner("evan@x.example").unwrap().as_deref(),
        Some("evan.luo")
    );
    assert_eq!(r.profile_of("evan").unwrap(), profile);
}
