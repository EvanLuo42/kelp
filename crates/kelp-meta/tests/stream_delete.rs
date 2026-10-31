use kelp_meta::{Command, Error, StreamKind};
use kelp_testkit::depot_path as p;
use kelp_testkit::meta::MetaFixture;

fn delete(f: &MetaFixture, path: &str) -> kelp_meta::Result<()> {
    f.db()
        .commit(Command::DeleteStream { path: p(path) })
        .map(|_| ())
}

#[test]
fn a_deleted_stream_keeps_its_history_and_its_name() {
    let f = MetaFixture::new();
    f.submit("//game/main", vec![f.add("//game/main/a.txt", b"a")])
        .unwrap();
    f.create_stream("//game/dev", StreamKind::Development, "//game/main");
    f.create_stream("//game/task", StreamKind::Task, "//game/dev");
    let cl = f
        .submit("//game/task", vec![f.add("//game/task/b.txt", b"b")])
        .unwrap();

    assert!(
        matches!(delete(&f, "//game/main"), Err(Error::Invalid(_))),
        "a mainline stays"
    );
    assert!(
        matches!(delete(&f, "//game/dev"), Err(Error::Invalid(_))),
        "a parent stays"
    );
    delete(&f, "//game/task").unwrap();
    let r = f.db().read().unwrap();
    assert!(r.stream(&p("//game/task")).unwrap().is_none());
    assert_eq!(r.changelist(cl).unwrap().unwrap().stream, p("//game/task"));
    drop(r);
    let again = f.db().commit(Command::CreateStream {
        path: p("//game/task"),
        kind: StreamKind::Task,
        parent: Some(p("//game/dev")),
        at: 1,
    });
    assert!(matches!(again, Err(Error::AlreadyExists(_))), "{again:?}");
    delete(&f, "//game/dev").unwrap();
}
