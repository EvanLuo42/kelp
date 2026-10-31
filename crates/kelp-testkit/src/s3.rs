//! Access to an S3-compatible test server.
//!
//! Enabled by the `s3` feature. Tests that need S3 call [`config`] and skip
//! themselves when it returns `None`.

use std::sync::atomic::{AtomicU64, Ordering};

use kelp_cas::S3Config;

/// Returns a configuration for the bucket named by `KELP_TEST_S3_BUCKET` on
/// the server at `KELP_TEST_S3_ENDPOINT`, under a prefix no other call
/// returns, or `None` if those variables are not set.
///
/// The server must accept the credentials in the usual AWS environment
/// variables; local test servers such as moto accept any.
pub fn config() -> Option<S3Config> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let endpoint = std::env::var("KELP_TEST_S3_ENDPOINT").ok()?;
    let bucket = std::env::var("KELP_TEST_S3_BUCKET").ok()?;
    let unique = format!(
        "test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    Some(S3Config {
        bucket,
        endpoint: Some(endpoint),
        region: Some("us-east-1".into()),
        allow_http: true,
        prefix: Some(unique),
    })
}
