use kelp_meta::db::FORMAT_VERSION;
use kelp_meta::{Error, MetaDb};
use kelp_testkit::Sandbox;

#[test]
fn a_new_database_gets_the_current_format() {
    let sandbox = Sandbox::new();
    let path = sandbox.path("meta.redb");
    let db = MetaDb::open(&path).unwrap();
    assert_eq!(db.read().unwrap().format_version().unwrap(), FORMAT_VERSION);
    drop(db);
    let db = MetaDb::open(&path).unwrap();
    assert_eq!(db.read().unwrap().format_version().unwrap(), FORMAT_VERSION);
}

#[test]
fn databases_of_unknown_formats_are_refused_untouched() {
    let sandbox = Sandbox::new();
    let write = |name: &str, entries: &[(&str, u64)]| {
        let path = sandbox.path(name);
        let db = redb::Database::create(&path).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut meta = txn
                .open_table(redb::TableDefinition::<&str, u64>::new("meta"))
                .unwrap();
            for (k, v) in entries {
                meta.insert(*k, *v).unwrap();
            }
        }
        txn.commit().unwrap();
        path
    };
    let newer = write("newer.redb", &[("format_version", FORMAT_VERSION + 1)]);
    assert!(
        matches!(MetaDb::open(&newer), Err(Error::Format(ref m)) if m.contains("upgrade Kelp"))
    );
    let unversioned = write("old.redb", &[("last_index", 7)]);
    assert!(matches!(MetaDb::open(&unversioned), Err(Error::Format(_))));
    assert!(
        matches!(MetaDb::open(&unversioned), Err(Error::Format(_))),
        "a refused database is not stamped"
    );
}

#[test]
fn users_that_single_sign_on_created_link_on_their_next_sign_on() {
    let sandbox = Sandbox::new();
    let path = sandbox.path("meta.redb");
    let db = MetaDb::open(&path).unwrap();
    for (actor, name) in [("oidc", "carol"), ("admin", "ci")] {
        let create = kelp_meta::Command::CreateUser {
            name: name.into(),
            password_hash: None,
            admin: false,
            replica: false,
            at: 1,
        };
        db.commit_by(actor, create).unwrap();
    }
    drop(db);
    {
        let raw = redb::Database::create(&path).unwrap();
        let txn = raw.begin_write().unwrap();
        txn.open_table(redb::TableDefinition::<&str, u64>::new("meta"))
            .unwrap()
            .insert("format_version", 3)
            .unwrap();
        txn.delete_table(redb::TableDefinition::<&str, &str>::new("sso_expected"))
            .unwrap();
        txn.commit().unwrap();
    }
    let db = MetaDb::open(&path).unwrap();
    let r = db.read().unwrap();
    assert_eq!(r.format_version().unwrap(), FORMAT_VERSION);
    assert_eq!(r.expected_sso("carol").unwrap().as_deref(), Some("carol"));
    assert_eq!(
        r.expected_sso("ci").unwrap(),
        None,
        "service accounts stay unlinked"
    );
}
