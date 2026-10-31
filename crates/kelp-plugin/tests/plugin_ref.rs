use kelp_plugin::PluginRef;

#[test]
fn parse_refs() {
    assert_eq!(
        PluginRef::parse("builtin:3way").unwrap(),
        PluginRef::Builtin("3way".into())
    );
    let r = PluginRef::parse("plugin:texture-mips@1.2.0").unwrap();
    assert_eq!(r.to_string(), "plugin:texture-mips@1.2.0");
    assert!(PluginRef::parse("plugin:nover").is_err());
    assert!(PluginRef::parse("texture").is_err());
}
