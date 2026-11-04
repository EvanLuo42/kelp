use kelp_core::{ClNumber, ContentId};
use kelp_meta::{ArtifactContents, Command, Dirs, Error};
use kelp_testkit::depot_path as p;
use kelp_testkit::meta::MetaFixture;

fn contents(f: &MetaFixture, cl: u64, platform: &str, bytes: &[u8]) -> ArtifactContents {
    ArtifactContents {
        stream: p("//game/main"),
        cl: ClNumber(cl),
        name: "editor".into(),
        tags: vec![("platform".into(), platform.into())],
        files: vec![
            ("bin/editor.exe".into(), f.data(bytes)),
            ("bin/editor.pdb".into(), f.data(b"pdb")),
        ],
        by: "ci".into(),
        at: 10,
    }
}

/// Submits `n` changelists to `//game/main`, numbered from 1.
fn submits(f: &MetaFixture, n: u64) {
    for i in 1..=n {
        let path = format!("//game/main/f{i}.txt");
        f.submit("//game/main", vec![f.add(&path, i.to_string().as_bytes())])
            .unwrap();
    }
}

#[test]
fn artifacts_of_changelists_the_stream_does_not_have_are_refused() {
    let f = MetaFixture::new();
    submits(&f, 2);
    let published = f.db().publish_artifact(contents(&f, 3, "win64", b"x"));
    assert!(
        matches!(published, Err(Error::NotFound(_))),
        "{published:?}"
    );
    f.db()
        .publish_artifact(contents(&f, 2, "win64", b"x"))
        .unwrap();
}

#[test]
fn artifacts_resolve_to_the_nearest_changelist_at_or_below() {
    let f = MetaFixture::new();
    submits(&f, 8);
    for cl in [3, 5, 8] {
        f.db()
            .publish_artifact(contents(&f, cl, "win64", format!("build {cl}").as_bytes()))
            .unwrap();
    }
    f.db()
        .publish_artifact(contents(&f, 6, "linux", b"linux build"))
        .unwrap();

    let r = f.db().read().unwrap();
    let win = [("platform".to_owned(), "win64".to_owned())];
    let found = r
        .artifacts(&p("//game/main"), "Editor", Some(ClNumber(7)), &win)
        .unwrap();
    assert_eq!(found.iter().map(|a| a.cl.0).collect::<Vec<_>>(), [5, 3]);
    let linux = [("platform".to_owned(), "linux".to_owned())];
    assert_eq!(
        r.artifacts(&p("//game/main"), "editor", None, &linux)
            .unwrap()[0]
            .cl,
        ClNumber(6)
    );
    assert!(
        r.artifacts(&p("//game/main"), "editor", Some(ClNumber(2)), &[])
            .unwrap()
            .is_empty()
    );

    let artifact = &found[0];
    assert_eq!((artifact.files, artifact.by.as_str()), (2, "ci"));
    let nodes = r.nodes().unwrap();
    let entry = Dirs::new(&nodes)
        .lookup(artifact.root, "bin/editor.exe")
        .unwrap()
        .unwrap();
    assert!(
        matches!(entry.kind, kelp_meta::EntryKind::File { content, .. } if content == ContentId::of(b"build 5"))
    );
    assert!(
        r.live_contents()
            .unwrap()
            .contains(&ContentId::of(b"build 5")),
        "artifact content is live"
    );
}

#[test]
fn retention_expires_old_artifacts_unless_pinned() {
    let f = MetaFixture::new();
    submits(&f, 5);
    let (_, first) = f
        .db()
        .publish_artifact(contents(&f, 1, "win64", b"1"))
        .unwrap();
    f.db()
        .commit(Command::PinArtifact {
            id: first,
            pinned: true,
        })
        .unwrap();
    let retain = Command::SetArtifactRetention {
        stream: p("//game/main"),
        name: "editor".into(),
        keep_last: Some(2),
    };
    f.db().commit(retain).unwrap();
    for cl in 2..=5 {
        f.db()
            .publish_artifact(contents(&f, cl, "win64", cl.to_string().as_bytes()))
            .unwrap();
    }
    let r = f.db().read().unwrap();
    let kept: Vec<u64> = r
        .artifacts(&p("//game/main"), "editor", None, &[])
        .unwrap()
        .iter()
        .map(|a| a.cl.0)
        .collect();
    assert_eq!(kept, [5, 4, 1], "the newest two plus the pinned one");
    assert!(!r.live_contents().unwrap().contains(&ContentId::of(b"2")));
}

#[test]
fn artifacts_need_their_content() {
    let f = MetaFixture::new();
    submits(&f, 1);
    let mut c = contents(&f, 1, "win64", b"x");
    c.files[0].1.content = ContentId::of(b"never uploaded");
    assert!(matches!(
        f.db().publish_artifact(c),
        Err(Error::MissingContent(_))
    ));
    let mut c = contents(&f, 1, "win64", b"x");
    c.files.clear();
    assert!(f.db().publish_artifact(c).is_err());
}
