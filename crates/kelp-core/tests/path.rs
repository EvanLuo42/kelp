use kelp_core::DepotPath;

#[test]
fn case_insensitive_but_preserving() {
    let a = DepotPath::parse("//Game/Main/Content/Hero.PNG").unwrap();
    let b = DepotPath::parse("//game/main/content/hero.png").unwrap();
    assert_eq!(a, b);
    assert_eq!(a.as_str(), "//Game/Main/Content/Hero.PNG");
    assert_eq!(a.depot(), "Game");
    assert_eq!(a.file_name(), "Hero.PNG");
}

#[test]
fn unicode_normalisation() {
    let composed = DepotPath::parse("//g/caf\u{e9}.txt").unwrap();
    let decomposed = DepotPath::parse("//g/cafe\u{301}.txt").unwrap();
    assert_eq!(composed, decomposed);
}

#[test]
fn rejects_bad_paths() {
    for bad in [
        "game/a",
        "//",
        "//g//a",
        "//g/../a",
        "//g/CON.txt",
        "//g/a.",
        "//g/a ",
        "//g/a:b",
        "//g/a\u{7}",
    ] {
        assert!(DepotPath::parse(bad).is_err(), "{bad} should be rejected");
    }
}

#[test]
fn prefix_respects_segments() {
    let dir = DepotPath::parse("//g/Content").unwrap();
    assert!(
        DepotPath::parse("//g/content/a.png")
            .unwrap()
            .starts_with(&dir)
    );
    assert!(
        !DepotPath::parse("//g/ContentExtra/a.png")
            .unwrap()
            .starts_with(&dir)
    );
}
