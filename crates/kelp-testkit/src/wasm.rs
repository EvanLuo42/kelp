//! The test plugins in `crates/kelp-plugin/guests`, as components.
//!
//! Enabled by the `wasm` feature. Building them needs the
//! `wasm32-unknown-unknown` Rust target.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// Returns the component of the test plugin `name`, such as
/// `fixed-chunker`, building the plugins on first use.
///
/// # Panics
///
/// Panics if the plugins cannot be built or encoded.
pub fn plugin(name: &str) -> Vec<u8> {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    let dir = BUILT.get_or_init(build);
    let module = std::fs::read(dir.join(format!("{}.wasm", name.replace('-', "_"))))
        .unwrap_or_else(|e| panic!("no test plugin {name}: {e}"));
    wit_component::ComponentEncoder::default()
        .module(&module)
        .and_then(|e| e.validate(true).encode())
        .unwrap_or_else(|e| panic!("cannot encode test plugin {name}: {e:#}"))
}

fn build() -> PathBuf {
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("testkit lives in crates/")
        .to_owned();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(
            || {
                crates
                    .parent()
                    .expect("crates/ has a parent")
                    .join("target")
            },
            PathBuf::from,
        )
        .join("wasm-guests");
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "build",
            "--quiet",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .arg("--manifest-path")
        .arg(crates.join("kelp-plugin/guests/Cargo.toml"))
        .arg("--target-dir")
        .arg(&target)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .status()
        .expect("run cargo");
    assert!(status.success(), "building the test plugins failed");
    target.join("wasm32-unknown-unknown/release")
}
