//! `kelp plugin`: the server's registry of WASM plugins.

use crate::util::unexpected;
use std::path::PathBuf;

use clap::Subcommand;
use kelp_client::{Connection, ContentCache, plugins};
use kelp_proto::{Reply, Request};

#[derive(Debug, Subcommand)]
pub enum PluginCmd {
    /// Uploads and registers a component (administrators only).
    ///
    /// Typemaps refer to it as `plugin:<name>@<version>`.
    Add {
        name: String,
        version: String,
        file: PathBuf,
    },
    /// Lists registered plugins.
    List,
    /// Deletes a plugin version (administrators only).
    Remove { name: String, version: String },
}

pub async fn run(
    conn: &Connection,
    cache: &ContentCache,
    command: PluginCmd,
) -> anyhow::Result<()> {
    match command {
        PluginCmd::Add {
            name,
            version,
            file,
        } => {
            let wasm = std::fs::read(&file)?;
            plugins::register(conn, cache, &name, &version, &wasm).await?;
            println!("Registered plugin:{name}@{version}.");
        }
        PluginCmd::List => {
            let Reply::Plugins(list) = conn.call(&Request::ListPlugins).await? else {
                return Err(unexpected());
            };
            for p in list {
                println!(
                    "plugin:{}@{:<12} {:>9} bytes  {}",
                    p.name,
                    p.version,
                    p.size,
                    p.worlds.join(", ")
                );
            }
        }
        PluginCmd::Remove { name, version } => {
            conn.call(&Request::DeletePlugin { name, version }).await?;
        }
    }
    Ok(())
}
