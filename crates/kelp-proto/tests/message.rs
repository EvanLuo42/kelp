use kelp_proto::message::{Auth, Capabilities, Hello};

#[test]
fn negotiate_versions() {
    let hello = Hello {
        min_version: 1,
        max_version: 3,
        capabilities: Capabilities::ARTIFACTS,
        client_name: "kelp-cli".into(),
        auth: Auth::None,
    };
    assert_eq!(hello.negotiate(1, 2), Some(2));
    assert_eq!(hello.negotiate(4, 5), None);
}

#[test]
fn hello_postcard_round_trip() {
    let hello = Hello {
        min_version: 1,
        max_version: 1,
        capabilities: Capabilities(Capabilities::ARTIFACTS.0 | Capabilities::ZSTD_FRAMES.0),
        client_name: "kelpd".into(),
        auth: Auth::Bearer("t".into()),
    };
    let bytes = postcard::to_allocvec(&hello).unwrap();
    assert_eq!(postcard::from_bytes::<Hello>(&bytes).unwrap(), hello);
}
