use kelp_core::DepotPath;
use kelp_core::path::fold;
use unicode_normalization::UnicodeNormalization;

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
fn final_sigma_and_other_letter_variants_fold_alike() {
    let key = |s: &str| DepotPath::parse(s).unwrap().key().to_owned();
    assert_eq!(key("//g/m/ΑΣ"), key("//g/m/ασ"));
    assert_eq!(key("//g/m/ας"), key("//g/m/ασ"));
    assert_eq!(key("//g/m/ΟΔΟΣ.txt"), key("//g/m/οδος.txt"));
    assert_eq!(key("//g/m/ϐ"), key("//g/m/β"));
    assert_eq!(key("//g/m/10µm"), key("//g/m/10μm"));
    assert_eq!(key("//g/m/ſ"), key("//g/m/s"));
    // Nothing expands, and the dotless i stays itself.
    assert_eq!(key("//g/m/ß"), "g/m/ß");
    assert_ne!(key("//g/m/ß"), key("//g/m/ss"));
    assert_ne!(key("//g/m/ı"), key("//g/m/i"));
    assert_eq!(key("//g/m/İ"), key("//g/m/i\u{307}"));
}

/// The folding of metadata format 9 and earlier.
fn old_fold(s: &str) -> String {
    s.nfc().collect::<String>().to_lowercase()
}

#[test]
fn folding_is_idempotent_and_refolds_old_keys() {
    for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
        let s = c.to_string();
        let once = fold(&s);
        assert_eq!(fold(&once), once, "folding {c:?} is not idempotent");
        // Stored keys are migrated by folding them again.
        assert_eq!(fold(&old_fold(&s)), once, "the old key of {c:?}");
    }
    for s in ["ΑΣ", "ΣΑΣ", "ΟΔΟΣ ΕΝΑ", "Σ", "aΣ.b", "Ὀδυσσεύς", "cafe\u{301}"] {
        assert_eq!(fold(&old_fold(s)), fold(s), "{s}");
        assert_eq!(fold(&fold(s)), fold(s), "{s}");
    }
}

#[test]
fn deserializing_validates_and_recomputes_the_key() {
    #[derive(serde::Serialize)]
    struct Stored<'a> {
        original: &'a str,
        key: &'a str,
    }
    let bytes = postcard::to_allocvec(&Stored {
        original: "//g/m/ΑΣ",
        key: "g/m/ας",
    })
    .unwrap();
    let path: DepotPath = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(path, DepotPath::parse("//g/m/ασ").unwrap());
    assert_eq!(path.key(), "g/m/ασ");
    assert_eq!(path.as_str(), "//g/m/ΑΣ");

    for bad in ["", "g", "/", "//g/../a", "x//"] {
        let bytes = postcard::to_allocvec(&Stored {
            original: bad,
            key: "anything",
        })
        .unwrap();
        assert!(
            postcard::from_bytes::<DepotPath>(&bytes).is_err(),
            "{bad:?} should be rejected"
        );
    }

    let json = serde_json::to_string(&DepotPath::parse("//g/Main").unwrap()).unwrap();
    let back: DepotPath = serde_json::from_str(&json).unwrap();
    assert_eq!(back.as_str(), "//g/Main");
    assert_eq!(back.key(), "g/main");
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
