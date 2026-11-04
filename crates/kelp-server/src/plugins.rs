//! Running registered plugins on the server: validating them when they are
//! registered, and running submit triggers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kelp_cas::BlobStore;
use kelp_core::ContentId;
use kelp_core::typemap::Typemap;
use kelp_meta::{FileAction, FileChange};
use kelp_plugin::{Change, Host, Limits, Plugin, PluginRef, Verdict};
use kelp_proto::{ErrorKind, ErrorReply};

use crate::State;

/// The server's plugin host and the plugins it has compiled.
pub(crate) struct Plugins {
    host: Option<Host>,
    compiled: Mutex<HashMap<ContentId, Plugin>>,
}

impl Plugins {
    pub(crate) fn new() -> Self {
        let host = Host::new(Limits::default())
            .inspect_err(|e| tracing::warn!("plugins disabled: {e}"))
            .ok();
        Self {
            host,
            compiled: Mutex::new(HashMap::new()),
        }
    }

    fn host(&self) -> Result<&Host, ErrorReply> {
        self.host
            .as_ref()
            .ok_or_else(|| ErrorReply::new(ErrorKind::Internal, "plugins are disabled"))
    }

    /// Compiles `wasm`, the component of `content`, or returns it compiled.
    pub(crate) fn load(&self, content: ContentId, wasm: &[u8]) -> Result<Plugin, ErrorReply> {
        if let Some(p) = self.compiled.lock().expect("plugins lock").get(&content) {
            return Ok(p.clone());
        }
        let plugin = self
            .host()?
            .load(wasm)
            .map_err(|e| ErrorReply::new(ErrorKind::Invalid, e.to_string()))?;
        self.compiled
            .lock()
            .expect("plugins lock")
            .insert(content, plugin.clone());
        Ok(plugin)
    }
}

/// Reads the whole content of a file from the chunk store for a plugin.
///
/// Content larger than a plugin's memory could never be handed to one, so
/// it is refused before anything is read.
pub(crate) async fn read_content(
    state: &State,
    content: &ContentId,
) -> Result<Vec<u8>, ErrorReply> {
    let manifest = state
        .reader()?
        .manifest(content)
        .map_err(internal)?
        .ok_or_else(|| {
            ErrorReply::new(ErrorKind::NotFound, format!("content {content} not found"))
        })?;
    let limit = Limits::default().memory as u64;
    if manifest.size > limit {
        return Err(ErrorReply::new(
            ErrorKind::Invalid,
            format!(
                "content {content} has {} bytes, more than the {limit} a plugin can take",
                manifest.size
            ),
        ));
    }
    let mut data = Vec::with_capacity(manifest.size as usize);
    for chunk in &manifest.chunks {
        let bytes = state.blobs.get(chunk.id).await.map_err(internal)?;
        if (data.len() + bytes.len()) as u64 > manifest.size {
            return Err(internal(format!(
                "the chunks of content {content} exceed its size"
            )));
        }
        data.extend_from_slice(&bytes);
    }
    Ok(data)
}

/// Runs the triggers `typemap` assigns to `changes` of `stream` and returns
/// an error naming the first that rejects the submit. A trigger that is not
/// registered, or fails, rejects it too.
pub(crate) async fn check_triggers(
    state: &Arc<State>,
    typemap: &Typemap,
    stream: &kelp_core::DepotPath,
    author: &str,
    description: &str,
    changes: &[FileChange],
) -> Result<(), ErrorReply> {
    let relative: Vec<(String, &FileChange)> = changes
        .iter()
        .filter_map(|c| kelp_meta::db::stream_relative(stream, &c.path).map(|r| (r, c)))
        .collect();
    let triggers: Vec<String> = typemap
        .triggers(relative.iter().map(|(r, _)| r.as_str()))
        .into_iter()
        .map(str::to_owned)
        .collect();
    if triggers.is_empty() {
        return Ok(());
    }
    let summary: Vec<Change> = relative
        .iter()
        .map(|(path, c)| {
            let (action, size) = match &c.action {
                FileAction::Add { data } => ("add", data.size),
                FileAction::Edit { data, .. } => ("edit", data.size),
                FileAction::Delete { .. } => ("delete", 0),
            };
            Change {
                path: path.clone(),
                action: action.to_owned(),
                size,
            }
        })
        .collect();
    let contents: Arc<HashMap<String, ContentId>> = Arc::new(
        relative
            .iter()
            .filter_map(|(path, c)| c.action.data().map(|d| (path.clone(), d.content)))
            .collect(),
    );
    for reference in triggers {
        let rejected = |why: String| {
            ErrorReply::new(
                ErrorKind::Conflict,
                format!("rejected by {reference}: {why}"),
            )
        };
        let plugin = registered(state, &reference)
            .await
            .map_err(|e| rejected(e.message))?;
        let (state, summary, contents) = (state.clone(), summary.clone(), contents.clone());
        let (stream, author, description) = (
            stream.to_string(),
            author.to_owned(),
            description.to_owned(),
        );
        let handle = tokio::runtime::Handle::current();
        let verdict = tokio::task::spawn_blocking(move || {
            let reader = state.clone();
            let read_file = move |path: &str| {
                let content = contents.get(path)?;
                handle.block_on(read_content(&reader, content)).ok()
            };
            let host = state.plugins.host()?;
            plugin
                .pre_submit(host, &stream, &author, &description, &summary, read_file)
                .map_err(|e| ErrorReply::new(ErrorKind::Internal, e.to_string()))
        })
        .await
        .map_err(internal)?;
        match verdict {
            Ok(Verdict::Accept) => {}
            Ok(Verdict::Reject(why)) => return Err(rejected(why)),
            Err(e) => return Err(rejected(e.message)),
        }
    }
    Ok(())
}

/// Loads the registered plugin `reference`, as `plugin:<name>@<version>`.
pub(crate) async fn registered(state: &State, reference: &str) -> Result<Plugin, ErrorReply> {
    let PluginRef::Wasm { name, version } = PluginRef::parse(reference)
        .map_err(|e| ErrorReply::new(ErrorKind::Invalid, e.to_string()))?
    else {
        return Err(ErrorReply::new(
            ErrorKind::Invalid,
            format!("{reference} is not a WASM plugin"),
        ));
    };
    let record = state
        .reader()?
        .plugin(&name, &version)
        .map_err(internal)?
        .ok_or_else(|| {
            ErrorReply::new(
                ErrorKind::NotFound,
                format!("{reference} is not registered"),
            )
        })?;
    let wasm = read_content(state, &record.content).await?;
    state.plugins.load(record.content, &wasm)
}

fn internal(e: impl std::fmt::Display) -> ErrorReply {
    ErrorReply::new(ErrorKind::Internal, e.to_string())
}
