use kelp_meta::{Command, Error, LockDomain, LockHolder};
use kelp_testkit::meta::MetaFixture;

fn holder(user: &str, ws: &str) -> LockHolder {
    LockHolder {
        user: user.into(),
        workspace: ws.into(),
    }
}

fn acquire(f: &MetaFixture, h: LockHolder, paths: &[&str]) -> kelp_meta::Result<()> {
    let cmd = Command::AcquireLocks {
        holder: h,
        domain: LockDomain("game/main".into()),
        paths: paths.iter().map(|p| p.to_string()).collect(),
    };
    f.db().commit(cmd).map(drop)
}

fn set_typemap(f: &MetaFixture) {
    let source = "[[rule]]\npattern = \"**/*.png\"\nkind = \"binary\"\nlock = \"exclusive\"\n";
    let cmd = Command::SetTypemap {
        depot: "game".into(),
        source: source.into(),
        by: "admin".into(),
        at: 1,
    };
    f.db().commit(cmd).unwrap();
}

#[test]
fn locks_are_all_or_nothing() {
    let f = MetaFixture::new();
    acquire(&f, holder("alice", "a1"), &["Hero.png"]).unwrap();
    let err = acquire(&f, holder("bob", "b1"), &["hero.PNG", "Other.png"]).unwrap_err();
    assert!(matches!(err, Error::Locked(ref v) if v.len() == 1), "{err}");
    let r = f.db().read().unwrap();
    assert!(
        r.lock(&LockDomain("game/main".into()), "Other.png")
            .unwrap()
            .is_none()
    );
    assert_eq!(r.locks(&LockDomain("game/main".into())).unwrap().len(), 1);

    let release = |h: Option<LockHolder>| Command::ReleaseLocks {
        holder: h,
        domain: LockDomain("game/main".into()),
        paths: vec!["Hero.png".into()],
    };
    f.db().commit(release(Some(holder("bob", "b1")))).unwrap();
    acquire(&f, holder("bob", "b1"), &["Hero.png"]).unwrap_err();
    f.db().commit(release(None)).unwrap();
    acquire(&f, holder("bob", "b1"), &["Hero.png"]).unwrap();
}

#[test]
fn exclusive_files_need_the_submitters_lock() {
    let f = MetaFixture::new();
    set_typemap(&f);
    let add = || vec![f.add("//game/main/Content/Hero.png", b"png")];
    assert!(matches!(
        f.submit("//game/main", add()),
        Err(Error::Conflict(_))
    ));

    acquire(&f, holder("alice", "other-ws"), &["Content/Hero.png"]).unwrap();
    assert!(
        matches!(f.submit("//game/main", add()), Err(Error::Conflict(_))),
        "lock from another workspace"
    );

    let release = Command::ReleaseLocks {
        holder: None,
        domain: LockDomain("game/main".into()),
        paths: vec!["Content/Hero.png".into()],
    };
    f.db().commit(release).unwrap();
    acquire(&f, holder("alice", "ws"), &["Content/Hero.png"]).unwrap();
    f.submit("//game/main", add()).unwrap();
    let r = f.db().read().unwrap();
    assert!(
        r.lock(&LockDomain("game/main".into()), "Content/Hero.png")
            .unwrap()
            .is_none(),
        "submit releases"
    );

    f.submit(
        "//game/main",
        vec![f.add("//game/main/readme.txt", b"text")],
    )
    .unwrap();
}

#[test]
fn development_streams_share_the_parent_lock_domain() {
    let f = MetaFixture::new();
    f.create_stream(
        "//game/dev",
        kelp_meta::StreamKind::Development,
        "//game/main",
    );
    f.create_stream("//game/rel", kelp_meta::StreamKind::Release, "//game/main");
    let r = f.db().read().unwrap();
    let domain = |s: &str| {
        r.stream(&kelp_testkit::depot_path(s))
            .unwrap()
            .unwrap()
            .lock_domain
    };
    assert_eq!(domain("//game/dev"), domain("//game/main"));
    assert_ne!(domain("//game/rel"), domain("//game/main"));
}
