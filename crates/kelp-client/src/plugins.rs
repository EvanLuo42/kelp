//! Running the server's registered plugins on this machine.
//!
//! The typemap names plugins as `plugin:<name>@<version>`. Their components
//! are fetched from the server like any other content, so everyone runs the
//! same bytes, and are compiled once per process.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};

use kelp_cas::{ChunkSpan, Chunker, FastCdcChunker};
use kelp_core::ContentId;
use kelp_plugin::{FileVersion, Host, Limits, Plugin, PluginRef};
use kelp_proto::{Reply, Request};

use crate::{Connection, ContentCache, Error, Result};

/// The largest chunk a chunker plugin may produce.
const MAX_CHUNK: u32 = 4 << 20;

static COMPILED: LazyLock<Mutex<HashMap<ContentId, Plugin>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Returns the process's plugin host.
pub(crate) fn host() -> Result<&'static Host> {
    static HOST: OnceLock<std::result::Result<Host, String>> = OnceLock::new();
    HOST.get_or_init(|| Host::new(Limits::default()).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| Error::Invalid(format!("plugins are unavailable: {e}")))
}

/// Fetches the registered plugin `reference` and compiles it.
///
/// # Errors
///
/// Returns an error if the reference is malformed, the plugin is not
/// registered, or it cannot be downloaded or compiled.
pub async fn load(conn: &Connection, cache: &ContentCache, reference: &str) -> Result<Plugin> {
    let PluginRef::Wasm { name, version } = PluginRef::parse(reference)? else {
        return Err(Error::Invalid(format!("{reference} is not a WASM plugin")));
    };
    let Reply::Plugin(info) = conn.call(&Request::GetPlugin { name, version }).await? else {
        return Err(Error::UnexpectedReply);
    };
    if let Some(p) = COMPILED.lock().expect("plugins lock").get(&info.content) {
        return Ok(p.clone());
    }
    cache.fetch(conn, &[info.content]).await?;
    let plugin = host()?.load(&cache.read(&info.content).await?)?;
    COMPILED
        .lock()
        .expect("plugins lock")
        .insert(info.content, plugin.clone());
    Ok(plugin)
}

/// Uploads the component `wasm` and registers it as plugin `name` at
/// `version`. Requires an administrator.
///
/// # Errors
///
/// Returns an error if the upload fails or the server rejects the plugin,
/// for example because it is not a valid component or the version exists.
pub async fn register(
    conn: &Connection,
    cache: &ContentCache,
    name: &str,
    version: &str,
    wasm: &[u8],
) -> Result<()> {
    let manifest = cache.ingest(wasm).await?;
    cache.upload(conn, std::slice::from_ref(&manifest)).await?;
    let request = Request::RegisterPlugin {
        name: name.to_owned(),
        version: version.to_owned(),
        content: manifest.content,
        size: manifest.size,
    };
    conn.call(&request).await?;
    Ok(())
}

/// Describes the change of the file at the stream-relative `path` from
/// `old` to `new` content with the differ plugin `reference`.
///
/// # Errors
///
/// Returns an error if the plugin or the content cannot be fetched, or the
/// plugin fails.
pub async fn describe(
    conn: &Connection,
    cache: &ContentCache,
    reference: &str,
    path: &str,
    old: Option<ContentId>,
    new: Option<ContentId>,
) -> Result<String> {
    let plugin = load(conn, cache, reference).await?;
    let contents: Vec<ContentId> = old.into_iter().chain(new).collect();
    cache.fetch(conn, &contents).await?;
    let version = |content| async move {
        Ok::<_, Error>(FileVersion {
            path: path.to_owned(),
            content: cache.read(&content).await?,
        })
    };
    let old = match old {
        Some(c) => Some(version(c).await?),
        None => None,
    };
    let new = match new {
        Some(c) => Some(version(c).await?),
        None => None,
    };
    Ok(plugin.describe(host()?, old.as_ref(), new.as_ref())?)
}

/// Like [`describe`], for content given as bytes, such as a file on disk
/// that is not on the server yet.
///
/// # Errors
///
/// Returns an error if the plugin cannot be fetched or fails.
pub async fn describe_bytes(
    conn: &Connection,
    cache: &ContentCache,
    reference: &str,
    path: &str,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
) -> Result<String> {
    let plugin = load(conn, cache, reference).await?;
    let version = |content: &[u8]| FileVersion {
        path: path.to_owned(),
        content: content.to_vec(),
    };
    let (old, new) = (old.map(version), new.map(version));
    Ok(plugin.describe(host()?, old.as_ref(), new.as_ref())?)
}

/// Chunks a file with a chunker plugin, falling back to FastCDC if the
/// plugin fails or produces chunks that are too large.
pub(crate) struct PluginChunker {
    pub plugin: Plugin,
    pub path: String,
}

impl Chunker for PluginChunker {
    fn chunk(&self, data: &[u8]) -> Vec<ChunkSpan> {
        let spans = host().and_then(|h| Ok(self.plugin.chunk(h, &self.path, data)?));
        match spans {
            Ok(spans) if spans.iter().all(|s| s.len <= MAX_CHUNK) => spans
                .into_iter()
                .map(|s| ChunkSpan {
                    offset: s.offset,
                    len: s.len,
                })
                .collect(),
            _ => FastCdcChunker::default().chunk(data),
        }
    }
}
