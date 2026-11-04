use kelp_meta::Governance;
use kelp_meta::db::Proposal;
use kelp_proto::Reply;
use kelp_proto::rpc::{DeliveryInfo, WebhookInfo};

use super::{Access, Response, meta_error};
use crate::State;

pub(super) async fn set(
    state: &State,
    access: &Access,
    name: String,
    url: String,
    secret: String,
    events: Vec<String>,
    paths: Vec<String>,
) -> Response {
    access.require_role(kelp_meta::acl::Role::Integrations)?;
    let cmd = Governance::SetWebhook {
        name,
        url,
        secret,
        events,
        paths,
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) async fn delete(state: &State, access: &Access, name: String) -> Response {
    access.require_role(kelp_meta::acl::Role::Integrations)?;
    state
        .commit(access.name(), Governance::DeleteWebhook { name })
        .await?;
    Ok(Reply::Ok)
}

pub(super) fn list(state: &State, access: &Access) -> Response {
    access.require_role(kelp_meta::acl::Role::Integrations)?;
    let hooks = state.reader()?.webhooks().map_err(meta_error)?;
    Ok(Reply::Webhooks(
        hooks
            .into_iter()
            .map(|h| WebhookInfo {
                name: h.name,
                url: h.url,
                events: h.events,
                paths: h.paths,
            })
            .collect(),
    ))
}

pub(super) fn deliveries(
    state: &State,
    access: &Access,
    webhook: Option<String>,
    limit: u32,
) -> Response {
    access.require_role(kelp_meta::acl::Role::Integrations)?;
    let deliveries = state
        .reader()?
        .deliveries(webhook.as_deref(), limit.min(1000) as usize)
        .map_err(meta_error)?;
    Ok(Reply::Deliveries(
        deliveries
            .into_iter()
            .map(|d| DeliveryInfo {
                id: d.id,
                webhook: d.webhook,
                kind: d.kind,
                attempts: d.attempts,
                delivered: d.delivered,
                pending: d.next_at.is_some(),
                last_result: d.last_result,
            })
            .collect(),
    ))
}

pub(super) async fn redeliver(state: &State, access: &Access, id: u64) -> Response {
    access.require_role(kelp_meta::acl::Role::Integrations)?;
    state.propose(Proposal::Redeliver { id }).await?;
    Ok(Reply::Ok)
}
