use kelp_testkit::depot_path;
use kelp_workspace::View;

#[test]
fn later_lines_override() {
    let view = View::parse(
        "//game/main/...  ...\n\
         -//game/main/Content/Cinematics/...\n\
         //game/main/Content/Cinematics/Ep1/...  Cine/Ep1/...\n",
    )
    .unwrap();
    let map = |s| view.map(&depot_path(s));
    assert_eq!(
        map("//game/main/Source/Main.cpp").as_deref(),
        Some("Source/Main.cpp")
    );
    assert_eq!(map("//Game/Main/content/Cinematics/Ep2/a.mov"), None);
    assert_eq!(
        map("//game/main/Content/Cinematics/Ep1/Intro.mov").as_deref(),
        Some("Cine/Ep1/Intro.mov")
    );
    assert_eq!(map("//engine/main/a.rs"), None);
}

#[test]
fn rejects_malformed_lines() {
    assert!(View::parse("//game/main/... Content").is_err());
    assert!(View::parse("-//game/main/... x/...").is_err());
}

#[test]
fn maps_back_to_depot_paths() {
    let view = View::parse(
        "//game/main/...  ...\n\
         -//game/main/Content/Cinematics/...\n\
         //game/main/Content/Cinematics/Ep1/...  Cine/Ep1/...\n",
    )
    .unwrap();
    assert_eq!(
        view.to_depot("Source/Main.cpp"),
        Some(depot_path("//game/main/Source/Main.cpp"))
    );
    assert_eq!(
        view.to_depot("Cine/Ep1/Intro.mov"),
        Some(depot_path("//game/main/Content/Cinematics/Ep1/Intro.mov"))
    );
    assert_eq!(view.to_depot("Content/Cinematics/Ep2/a.mov"), None);

    let whole = View::whole_stream(&depot_path("//game/main"));
    assert_eq!(
        whole.map(&depot_path("//game/main/a/b.txt")).as_deref(),
        Some("a/b.txt")
    );
}

#[test]
fn the_workspace_side_stays_inside_the_workspace() {
    for line in [
        "//game/main/... /etc/...",
        "//game/main/... ../outside/...",
        "//game/main/... a/../../b/...",
        "//game/main/... .KELP/...",
        "//game/main/... C:/x/...",
    ] {
        assert!(View::parse(line).is_err(), "{line}");
    }
    assert!(View::parse("//game/main/... Game/...").is_ok());
}
