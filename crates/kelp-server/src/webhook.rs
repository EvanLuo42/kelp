//! Sending queued webhook deliveries.
//!
//! Each delivery is a `POST` of the event as JSON with these headers:
//!
//! - `X-Kelp-Event`: the event kind, such as `submit`;
//! - `X-Kelp-Delivery`: the delivery id, the same across retries;
//! - `X-Kelp-Signature`: `sha256=` and the hex HMAC-SHA256 of the body,
//!   keyed with the webhook's secret.
//!
//! A `2xx` response completes the delivery; anything else is retried.

use std::sync::Arc;
use std::time::Duration;

use kelp_meta::db::Proposal;
use kelp_meta::record::{Delivery, Webhook};

use crate::State;

const BATCH: usize = 32;
const TIMEOUT: Duration = Duration::from_secs(10);

/// Sends due deliveries until the server stops.
pub(crate) async fn deliver(state: Arc<State>, poll: Duration, retry: Duration) {
    let client = match reqwest::Client::builder().timeout(TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("webhooks disabled: {e}");
            return;
        }
    };
    loop {
        if state.cluster().is_some_and(|c| !c.is_leader()) {
            tokio::time::sleep(poll).await;
            continue;
        }
        let now = state.clock.now();
        let due = state.db.read().and_then(|r| r.due_deliveries(now, BATCH));
        let due = match due {
            Ok(due) => due,
            Err(e) => {
                tracing::warn!("webhooks: {e}");
                Vec::new()
            }
        };
        let idle = due.is_empty();
        for (delivery, hook) in due {
            let outcome = send(&client, &delivery, &hook).await;
            match &outcome {
                Ok(status) => tracing::debug!(
                    webhook = hook.name,
                    delivery = delivery.id,
                    event = delivery.kind,
                    "delivered ({status})"
                ),
                Err(why) => tracing::warn!(
                    webhook = hook.name,
                    delivery = delivery.id,
                    event = delivery.kind,
                    attempt = delivery.attempts + 1,
                    "delivery failed: {why}"
                ),
            }
            let now = state.clock.now();
            let proposal = Proposal::RecordDelivery {
                id: delivery.id,
                outcome,
                at: now,
                retry: retry.as_millis() as u64,
            };
            if let Err(e) = state.propose(proposal).await {
                tracing::warn!("webhooks: {}", e.message);
            }
        }
        if idle {
            tokio::time::sleep(poll).await;
        }
    }
}

async fn send(
    client: &reqwest::Client,
    delivery: &Delivery,
    hook: &Webhook,
) -> Result<String, String> {
    let response = client
        .post(&hook.url)
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            concat!("kelp-server/", env!("CARGO_PKG_VERSION")),
        )
        .header("X-Kelp-Event", &delivery.kind)
        .header("X-Kelp-Delivery", delivery.id.to_string())
        .header("X-Kelp-Signature", signature(&hook.secret, &delivery.body))
        .body(delivery.body.clone())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    match status.is_success() {
        true => Ok(status.as_u16().to_string()),
        false => Err(status.as_u16().to_string()),
    }
}

/// Returns the `X-Kelp-Signature` header value of `body`.
pub fn signature(secret: &str, body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let tag = ring::hmac::sign(&key, body);
    let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}
