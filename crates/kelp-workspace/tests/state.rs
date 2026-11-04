use kelp_core::{ContentId, LocalClId};
use kelp_testkit::{Sandbox, depot_path};
use kelp_workspace::{Config, HaveEntry, Ignore, OpenAction, State};

#[test]
fn config_and_state_round_trip() {
    let sandbox = Sandbox::new();
    let root = sandbox.dir("ws");
    let stream = depot_path("//game/main");

    let config = Config::new("ws-1", "localhost:7474", &stream);
    config.save(&root).unwrap();
    assert_eq!(Config::load(&root).unwrap(), config);
    assert_eq!(config.stream_path().unwrap(), stream);

    assert!(State::load(&root).unwrap().have.is_empty());
    let mut state = State::default();
    let path = depot_path("//game/main/Content/Hero.png");
    state.have.insert(
        path.clone(),
        HaveEntry {
            local: "Content/Hero.png".into(),
            content: ContentId::of(b"png"),
            size: 3,
            executable: false,
            mtime: 42,
        },
    );
    state
        .pending
        .open(path.clone(), OpenAction::Edit, LocalClId::DEFAULT, false)
        .unwrap();
    state.save(&root).unwrap();

    let loaded = State::load(&root).unwrap();
    assert_eq!(loaded.have, state.have);
    assert_eq!(loaded.pending.action(&path), Some(OpenAction::Edit));
}

#[test]
fn ignore_rules() {
    let sandbox = Sandbox::new();
    let root = sandbox.dir("ws");
    assert!(Ignore::load(&root).unwrap().is_ignored(".kelp/state.bin"));

    sandbox.write(
        "ws/.kelpignore",
        "# build output\nIntermediate/\n/Binaries\n*.obj\n",
    );
    let ignore = Ignore::load(&root).unwrap();
    assert!(ignore.is_ignored("Intermediate/Build/a.o"));
    assert!(ignore.is_ignored("Plugins/X/Intermediate/a.o"));
    assert!(ignore.is_ignored("Binaries/editor.exe"));
    assert!(!ignore.is_ignored("Plugins/X/Binaries/x.dll"));
    assert!(ignore.is_ignored("Source/main.obj"));
    assert!(!ignore.is_ignored("Source/main.cpp"));
}

#[test]
fn state_files_carry_a_format_version() {
    let sandbox = Sandbox::new();
    let root = sandbox.dir("ws");
    State::default().save(&root).unwrap();
    let bytes = sandbox.read("ws/.kelp/state.bin");
    assert_eq!(&bytes[..8], b"KELPWS\0\0");
    assert_eq!(bytes[8..12], kelp_workspace::STATE_VERSION.to_le_bytes());
    assert!(
        !sandbox.exists("ws/.kelp/state.tmp"),
        "the temporary file is renamed away"
    );

    let mut newer = bytes.clone();
    newer[8..12].copy_from_slice(&(kelp_workspace::STATE_VERSION + 1).to_le_bytes());
    sandbox.write("ws/.kelp/state.bin", &newer);
    let err = State::load(&root).unwrap_err().to_string();
    assert!(err.contains("upgrade Kelp"), "{err}");

    sandbox.write("ws/.kelp/state.bin", &bytes[12..]);
    let err = State::load(&root).unwrap_err().to_string();
    assert!(err.contains("without format versioning"), "{err}");
}

#[test]
fn artifact_directories_may_not_overlap_or_reach_kelp_files() {
    let mount = |to: &str| kelp_workspace::ArtifactMount {
        name: "x".into(),
        to: to.into(),
        stream: None,
        exact: false,
        tags: Default::default(),
    };
    let mut config = Config::new("ws", "localhost:7474", &depot_path("//game/main"));
    config.artifacts = vec![mount("Bin"), mount("Tools")];
    config.validate().unwrap();
    config.artifacts = vec![mount("Bin"), mount("bin/Win64")];
    assert!(config.validate().is_err());
    config.artifacts = vec![mount(".Kelp/x")];
    assert!(config.validate().is_err());
}

#[test]
fn version_one_state_gains_an_empty_list_of_moves() {
    let sandbox = Sandbox::new();
    let root = sandbox.dir("ws");
    let state = State {
        have_cl: 42,
        ..State::default()
    };
    state.save(&root).unwrap();
    let mut bytes = sandbox.read("ws/.kelp/state.bin");
    assert_eq!(
        bytes.pop(),
        Some(0),
        "an empty map of moves ends the payload"
    );
    bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
    sandbox.write("ws/.kelp/state.bin", &bytes);
    let loaded = State::load(&root).unwrap();
    assert_eq!(loaded.have_cl, 42);
    assert!(loaded.moves.is_empty());
}

#[test]
fn times_not_older_than_the_state_file_are_not_trusted() {
    let sandbox = Sandbox::new();
    let root = sandbox.dir("ws");
    let entry = |local: &str, mtime| HaveEntry {
        local: local.into(),
        content: ContentId::of(b"x"),
        size: 1,
        executable: false,
        mtime,
    };
    let (old, racy) = (
        depot_path("//game/main/old.txt"),
        depot_path("//game/main/racy.txt"),
    );
    let mut state = State::default();
    state.have.insert(old.clone(), entry("old.txt", 1));
    // A time in the future stands for a file changed in the same tick of a
    // coarse clock as the state file, or by a file server ahead of us.
    state.have.insert(racy.clone(), entry("racy.txt", u64::MAX));
    state.save(&root).unwrap();

    let loaded = State::load(&root).unwrap();
    assert_eq!(loaded.have[&old].mtime, 1);
    assert_eq!(loaded.have[&racy].mtime, 0);
}
