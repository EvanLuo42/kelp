//! `kelp webhook`: HTTP endpoints that receive events (administrators only).

use clap::Subcommand;
use kelp_client::Connection;
use kelp_proto::{Reply, Request};

use crate::util::read_password;
use crate::util::unexpected;

#[derive(Debug, Subcommand)]
pub enum WebhookCmd {
    /// Creates or replaces a webhook; prompts for the signing secret.
    Set {
        name: String,
        url: String,
        /// An event kind or prefix, such as `submit` or `review`; may be
        /// repeated. All events if omitted.
        #[arg(long = "event")]
        events: Vec<String>,
        /// A path pattern such as `//game/...`; may be repeated. All paths
        /// if omitted.
        #[arg(long = "path")]
        paths: Vec<String>,
        /// Reads the secret from standard input instead of prompting.
        #[arg(long)]
        secret_stdin: bool,
    },
    /// Lists webhooks.
    List,
    /// Deletes a webhook.
    Delete { name: String },
    /// Lists recent deliveries, newest first.
    Deliveries {
        name: Option<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
    },
    /// Sends a delivery again.
    Redeliver { id: u64 },
}

pub async fn run(conn: &Connection, command: WebhookCmd) -> anyhow::Result<()> {
    match command {
        WebhookCmd::Set {
            name,
            url,
            events,
            paths,
            secret_stdin,
        } => {
            let secret = read_password(secret_stdin, "Signing secret: ", true)?;
            let request = Request::SetWebhook {
                name: name.clone(),
                url,
                secret,
                events,
                paths,
            };
            conn.call(&request).await?;
            println!("Set webhook {name}.");
        }
        WebhookCmd::List => {
            let Reply::Webhooks(hooks) = conn.call(&Request::ListWebhooks).await? else {
                return Err(unexpected());
            };
            for h in hooks {
                let events = if h.events.is_empty() {
                    "*".to_owned()
                } else {
                    h.events.join(",")
                };
                let paths = if h.paths.is_empty() {
                    "//...".to_owned()
                } else {
                    h.paths.join(",")
                };
                println!("{:<16} {:<40} {events:<20} {paths}", h.name, h.url);
            }
        }
        WebhookCmd::Delete { name } => {
            conn.call(&Request::DeleteWebhook { name }).await?;
        }
        WebhookCmd::Deliveries { name, limit } => {
            let request = Request::Deliveries {
                webhook: name,
                limit,
            };
            let Reply::Deliveries(deliveries) = conn.call(&request).await? else {
                return Err(unexpected());
            };
            for d in deliveries {
                let state = match (d.delivered, d.pending) {
                    (true, _) => "delivered",
                    (false, true) => "retrying",
                    (false, false) => "failed",
                };
                println!(
                    "{:>8}  {:<16} {:<16} {state:<10} {} attempt(s)  {}",
                    d.id,
                    d.webhook,
                    d.kind,
                    d.attempts,
                    d.last_result.unwrap_or_default()
                );
            }
        }
        WebhookCmd::Redeliver { id } => {
            conn.call(&Request::Redeliver { id }).await?;
        }
    }
    Ok(())
}
