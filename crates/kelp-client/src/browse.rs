//! Lists what is in depots and streams without a workspace.

use std::collections::BTreeMap;
use std::fmt::Write;

use kelp_core::DepotPath;
use kelp_core::path::fold;
use kelp_proto::rpc::{DirEntry, DirEntryKind, ListOptions};
use kelp_proto::{Reply, Request};

use crate::workspace::stream_relative;
use crate::{Connection, Error, Result};

/// Returns what is below `path`, or below the server's root if it is `None`.
/// The entries' paths are relative to `path`, and a file lists only itself,
/// under its name.
///
/// Inside a stream, these are its files. Above the streams, these are the
/// depots and streams; they have no changelists, and streams are not entered.
///
/// # Errors
///
/// Returns [`Error::Invalid`] if a changelist is given for a path above the
/// streams or nothing the user may read is at `path`, or an error if the server
/// cannot be reached.
pub async fn list_depot(
    conn: &Connection,
    path: Option<&DepotPath>,
    options: ListOptions,
) -> Result<Vec<DirEntry>> {
    let Reply::Streams(streams) = conn.call(&Request::ListStreams).await? else {
        return Err(Error::UnexpectedReply);
    };
    let streams: Vec<DepotPath> = streams
        .into_iter()
        .filter_map(|s| DepotPath::parse(&s.path).ok())
        .collect();
    let stream = path.and_then(|path| {
        streams
            .iter()
            .filter(|s| path.starts_with(s))
            .max_by_key(|s| s.as_str().len())
            .map(|stream| (stream, path))
    });
    if let Some((stream, path)) = stream {
        let rel = stream_relative(stream, path);
        let mut entries = conn.list_dir(stream.as_str(), &rel, options).await?;
        let skip = rel.split('/').filter(|s| !s.is_empty()).count();
        for entry in &mut entries {
            let segments: Vec<&str> = entry.path.split('/').collect();
            let below = segments[skip.min(segments.len())..].join("/");
            if !below.is_empty() {
                entry.path = below;
            } else if let Some(name) = segments.last() {
                entry.path = (*name).to_owned();
            }
        }
        return Ok(entries);
    }
    if options.at.is_some() {
        return Err(Error::Invalid(
            "a changelist needs a path inside a stream".into(),
        ));
    }
    let entries = above_streams(&streams, path, options.depth);
    match (path, entries.is_empty()) {
        (Some(path), true) => Err(Error::Invalid(format!("{path} does not exist"))),
        _ => Ok(entries),
    }
}

/// Returns the depots and streams below `path`, which no stream contains.
fn above_streams(
    streams: &[DepotPath],
    path: Option<&DepotPath>,
    depth: Option<u32>,
) -> Vec<DirEntry> {
    let skip = path.map_or(0, |p| p.segments().count());
    let depth = depth.map_or(usize::MAX, |d| d as usize);
    // Ordered by segment, so that a directory's entries directly follow it.
    let mut found = BTreeMap::new();
    for stream in streams
        .iter()
        .filter(|s| path.is_none_or(|p| s.starts_with(p)))
    {
        let segments: Vec<&str> = stream.segments().skip(skip).collect();
        for n in 1..=segments.len().min(depth) {
            let key: Vec<String> = segments[..n].iter().map(|s| fold(s)).collect();
            found.entry(key).or_insert_with(|| segments[..n].join("/"));
        }
    }
    found
        .into_values()
        .map(|path| DirEntry {
            path,
            kind: DirEntryKind::Dir,
            last: None,
        })
        .collect()
}

/// Returns an entry's last path segment as a listing shows it: directories
/// end in `/`, and symlinks show their target.
pub fn entry_label(entry: &DirEntry) -> String {
    let name = entry.path.rsplit('/').next().unwrap_or_default();
    match &entry.kind {
        DirEntryKind::File(_) => name.to_owned(),
        DirEntryKind::Dir => format!("{name}/"),
        DirEntryKind::Symlink { target } => format!("{name} -> {target}"),
    }
}

/// Draws `entries`, as [`list_depot`] returns them, as a tree.
pub fn render_tree(entries: &[DirEntry]) -> String {
    let depth = |e: &DirEntry| e.path.split('/').count() - 1;
    // Going backwards, an entry is the last of its siblings if none was seen
    // at its level since leaving its parent.
    let mut last = vec![false; entries.len()];
    let mut seen: Vec<bool> = Vec::new();
    for (i, entry) in entries.iter().enumerate().rev() {
        let level = depth(entry);
        seen.resize(level + 1, false);
        last[i] = !seen[level];
        seen[level] = true;
    }

    let mut out = String::new();
    // Whether each ancestor of the entry is the last of its siblings.
    let mut ancestors: Vec<bool> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        ancestors.truncate(depth(entry));
        for ancestor_last in &ancestors {
            out.push_str(if *ancestor_last { "    " } else { "│   " });
        }
        let branch = if last[i] { "└── " } else { "├── " };
        let _ = writeln!(out, "{branch}{}", entry_label(entry));
        ancestors.push(last[i]);
    }
    out
}
