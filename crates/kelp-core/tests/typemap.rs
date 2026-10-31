use kelp_core::typemap::{EolPolicy, FileKind, FileType, LockPolicy, Typemap};

const SAMPLE: &str = r#"
    [[rule]]
    pattern = "**/*.{png,psd,fbx}"
    kind = "binary"
    lock = "exclusive"

    [[rule]]
    pattern = "**/*.{rs,cpp,h}"
    kind = "text"
    eol = "normalize"

    [[rule]]
    pattern = "Build/Baked/**"
    retain = 3
    compress = false
"#;

#[test]
fn later_rules_override_fields() {
    let map = Typemap::parse(SAMPLE).unwrap();

    let png = map.resolve("Content/Hero.PNG");
    assert_eq!(png.kind, FileKind::Binary);
    assert_eq!(png.lock, LockPolicy::Exclusive);
    assert!(!png.allows_hunk_split());

    let baked = map.resolve("Build/Baked/Atlas.png");
    assert_eq!(baked.lock, LockPolicy::Exclusive);
    assert_eq!(baked.retain, Some(3));
    assert!(!baked.compress);

    let src = map.resolve("Source/Renderer/main.rs");
    assert_eq!(src.eol, EolPolicy::Normalize);
    assert!(src.allows_hunk_split());

    assert_eq!(map.resolve("README"), FileType::default());
}

#[test]
fn rejects_unknown_fields() {
    assert!(Typemap::parse("[[rule]]\npattern = \"*\"\nlocked = true").is_err());
}
