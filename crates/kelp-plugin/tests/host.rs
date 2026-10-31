use kelp_plugin::{Change, Error, FileVersion, Host, Limits, MergeResult, PluginWorld, Verdict};
use kelp_testkit::wasm::plugin;

fn host() -> Host {
    Host::new(Limits::default()).unwrap()
}

fn version(path: &str, content: &str) -> FileVersion {
    FileVersion {
        path: path.into(),
        content: content.as_bytes().to_vec(),
    }
}

#[test]
fn a_chunker_splits_files() {
    let host = host();
    let chunker = host.load(&plugin("fixed-chunker")).unwrap();
    assert_eq!(chunker.worlds(), [PluginWorld::Chunker]);
    let spans = chunker.chunk(&host, "a.bin", &[7; 2500]).unwrap();
    let lens: Vec<u32> = spans.iter().map(|s| s.len).collect();
    assert_eq!(lens, [1000, 1000, 500]);
    assert!(matches!(
        chunker.describe(&host, None, None),
        Err(Error::WrongWorld(PluginWorld::Differ))
    ));
}

#[test]
fn a_merger_merges_or_reports_a_conflict() {
    let host = host();
    let merger = host.load(&plugin("line-merger")).unwrap();
    let base = version("tags.txt", "a\nb\n");
    let result = merger
        .merge(
            &host,
            &base,
            &version("tags.txt", "a\nb\nc\n"),
            &version("tags.txt", "b\nd\n"),
        )
        .unwrap();
    assert_eq!(result, MergeResult::Merged(b"b\nc\nd\n".to_vec()));
    let result = merger
        .merge(&host, &base, &version("tags.txt", "!conflict\n"), &base)
        .unwrap();
    assert!(matches!(result, MergeResult::Conflict(_)));
}

#[test]
fn a_differ_describes_changes() {
    let host = host();
    let differ = host.load(&plugin("size-differ")).unwrap();
    let old = version("a.bin", "12345");
    let new = version("a.bin", "123");
    assert_eq!(
        differ.describe(&host, Some(&old), Some(&new)).unwrap(),
        "a.bin: 5 -> 3 bytes"
    );
    assert_eq!(
        differ.describe(&host, None, Some(&new)).unwrap(),
        "a.bin: new, 3 bytes"
    );
}

#[test]
fn a_trigger_reads_files_and_judges_submits() {
    let host = host();
    let trigger = host.load(&plugin("todo-trigger")).unwrap();
    let changes = vec![Change {
        path: "Code/a.rs".into(),
        action: "edit".into(),
        size: 13,
    }];
    let judge = |description: &str, content: &'static [u8]| {
        trigger
            .pre_submit(
                &host,
                "//game/main",
                "alice",
                description,
                &changes,
                move |p| (p == "Code/a.rs").then(|| content.to_vec()),
            )
            .unwrap()
    };
    assert!(matches!(judge("no ticket", b"fine"), Verdict::Reject(_)));
    assert!(
        matches!(judge("fix #12", b"// DO NOT SUBMIT"), Verdict::Reject(ref w) if w.contains("DO NOT SUBMIT"))
    );
    assert_eq!(judge("fix #12", b"fine"), Verdict::Accept);
}

#[test]
fn plugins_are_bounded_and_validated() {
    let tight = Host::new(Limits {
        fuel: 1_000,
        memory: 64 << 20,
    })
    .unwrap();
    let chunker = tight.load(&plugin("fixed-chunker")).unwrap();
    assert!(matches!(
        chunker.chunk(&tight, "a", &[0; 100_000]),
        Err(Error::Trap(_))
    ));
    assert!(matches!(host().load(b"not wasm"), Err(Error::Invalid(_))));
}
