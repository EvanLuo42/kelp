use std::sync::Arc;

use kelp_core::ContentId;
use kelp_meta::Governance;
use kelp_meta::record::PluginRecord;
use kelp_proto::rpc::PluginInfo;
use kelp_proto::{ErrorKind, ErrorReply, Reply};

use super::{Access, Response, meta_error};
use crate::State;
use crate::plugins::read_content;

pub(super) async fn register(
    state: &Arc<State>,
    access: &Access,
    name: String,
    version: String,
    content: ContentId,
    size: u64,
) -> Response {
    access.require_admin()?;
    let wasm = read_content(state, &content).await?;
    let plugin = state.plugins.load(content, &wasm)?;
    let cmd = Governance::RegisterPlugin {
        name,
        version,
        content,
        size,
        worlds: plugin.worlds().iter().map(ToString::to_string).collect(),
        by: access.name().to_owned(),
        at: state.clock.now(),
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) async fn delete(
    state: &State,
    access: &Access,
    name: String,
    version: String,
) -> Response {
    access.require_admin()?;
    state
        .commit(access.name(), Governance::DeletePlugin { name, version })
        .await?;
    Ok(Reply::Ok)
}

pub(super) fn get(state: &State, name: String, version: String) -> Response {
    let record = state
        .reader()?
        .plugin(&name, &version)
        .map_err(meta_error)?
        .ok_or_else(|| {
            ErrorReply::new(
                ErrorKind::NotFound,
                format!("plugin {name}@{version} not found"),
            )
        })?;
    Ok(Reply::Plugin(info(record)))
}

pub(super) fn list(state: &State) -> Response {
    let records = state.reader()?.plugins().map_err(meta_error)?;
    Ok(Reply::Plugins(records.into_iter().map(info).collect()))
}

fn info(r: PluginRecord) -> PluginInfo {
    PluginInfo {
        name: r.name,
        version: r.version,
        content: r.content,
        size: r.size,
        worlds: r.worlds,
    }
}
