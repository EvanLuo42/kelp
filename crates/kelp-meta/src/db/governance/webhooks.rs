use kelp_core::ClNumber;
use redb::{ReadableTable, WriteTransaction};

use crate::acl::PathPattern;
use crate::db::{
    DELIVERIES, META, MetaDb, PENDING_DELIVERIES, REVIEWS, Reader, WEBHOOKS, dec, enc, name_key,
};
use crate::record::{Delivery, Review, Webhook};
use crate::{Command, Error, Governance, Result, Timestamp, event};

const NEXT_DELIVERY: &str = "next_delivery";

/// How many attempts a delivery gets before it is given up.
pub const MAX_ATTEMPTS: u32 = 8;

pub(in crate::db) fn apply(txn: &WriteTransaction, cmd: &Governance) -> Result<()> {
    match cmd {
        Governance::SetWebhook {
            name,
            url,
            secret,
            events,
            paths,
        } => {
            crate::db::apply::validate_name("webhook", name)?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(Error::Invalid(format!(
                    "webhook url {url:?} is not http(s)"
                )));
            }
            for path in paths {
                PathPattern::parse(path)
                    .map_err(|e| Error::Invalid(format!("webhook path {path:?}: {e}")))?;
            }
            let hook = Webhook {
                name: name.clone(),
                url: url.clone(),
                secret: secret.clone(),
                events: events.clone(),
                paths: paths.clone(),
            };
            txn.open_table(WEBHOOKS)?
                .insert(name_key(name).as_str(), enc(&hook).as_slice())?;
        }
        Governance::DeleteWebhook { name } => {
            if txn
                .open_table(WEBHOOKS)?
                .remove(name_key(name).as_str())?
                .is_none()
            {
                return Err(Error::NotFound(format!("webhook {name}")));
            }
        }
        _ => unreachable!("not a webhook command"),
    }
    Ok(())
}

/// Queues the event of `cmd`, logged at `index` on behalf of `actor`, for
/// every webhook that wants it.
pub(in crate::db) fn enqueue(
    txn: &WriteTransaction,
    index: u64,
    actor: &str,
    cmd: &Command,
) -> Result<()> {
    let Some(mut event) = event::of(cmd) else {
        return Ok(());
    };
    let hooks: Vec<Webhook> = txn
        .open_table(WEBHOOKS)?
        .iter()?
        .map(|r| dec("webhooks", r?.1.value()))
        .collect::<Result<_>>()?;
    if hooks.is_empty() {
        return Ok(());
    }
    if let Some(shelf) = review_of(cmd) {
        // Review events are about the files under review.
        if let Some(v) = txn.open_table(REVIEWS)?.get(shelf.0)? {
            let review: Review = dec("reviews", v.value())?;
            event.paths = review.files().iter().map(|f| f.path.clone()).collect();
        }
    }
    let mut meta = txn.open_table(META)?;
    let mut next = meta.get(NEXT_DELIVERY)?.map_or(1, |v| v.value());
    let mut deliveries = txn.open_table(DELIVERIES)?;
    let mut pending = txn.open_table(PENDING_DELIVERIES)?;
    for hook in hooks.iter().filter(|h| wants(h, &event)) {
        let body = serde_json::json!({
            "event": event.kind,
            "delivery": next,
            "log_index": index,
            "actor": actor,
            "data": event.data,
        });
        let delivery = Delivery {
            id: next,
            webhook: hook.name.clone(),
            kind: event.kind.to_owned(),
            body: serde_json::to_vec(&body).expect("events serialize"),
            attempts: 0,
            next_at: Some(0),
            delivered: false,
            last_result: None,
        };
        deliveries.insert(next, enc(&delivery).as_slice())?;
        pending.insert(next, ())?;
        next += 1;
    }
    meta.insert(NEXT_DELIVERY, next)?;
    Ok(())
}

fn wants(hook: &Webhook, event: &event::Event) -> bool {
    let kind = hook.events.is_empty()
        || hook
            .events
            .iter()
            .any(|e| event.kind.starts_with(e.as_str()));
    let path = hook.paths.is_empty()
        || hook
            .paths
            .iter()
            .filter_map(|p| PathPattern::parse(p).ok())
            .any(|pattern| event.paths.iter().any(|p| pattern.matches(p)));
    kind && path
}

fn review_of(cmd: &Command) -> Option<ClNumber> {
    match cmd {
        Command::Governance(
            Governance::OpenReview { shelf, .. }
            | Governance::Vote { shelf, .. }
            | Governance::VoteOn { shelf, .. }
            | Governance::ReportStatus { shelf, .. }
            | Governance::CloseReview { shelf },
        ) => Some(*shelf),
        _ => None,
    }
}

impl MetaDb {
    /// Records the outcome of an attempt to send delivery `id` at `now`.
    ///
    /// A failed delivery is retried with exponential backoff, starting at
    /// `retry` milliseconds, until it has had [`MAX_ATTEMPTS`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if the delivery does not exist, or
    /// [`Error::Db`] if the transaction fails.
    pub fn record_delivery(
        &self,
        id: u64,
        outcome: std::result::Result<String, String>,
        now: Timestamp,
        retry: u64,
    ) -> Result<()> {
        let proposal = crate::db::Proposal::RecordDelivery {
            id,
            outcome,
            at: now,
            retry,
        };
        self.propose(&proposal).map(|_| ())
    }

    /// Queues delivery `id` to be sent again now, with a fresh set of
    /// attempts.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if the delivery does not exist, or
    /// [`Error::Db`] if the transaction fails.
    pub fn redeliver(&self, id: u64) -> Result<()> {
        self.propose(&crate::db::Proposal::Redeliver { id })
            .map(|_| ())
    }
}

pub(crate) fn record_delivery(
    txn: &WriteTransaction,
    id: u64,
    outcome: std::result::Result<String, String>,
    now: Timestamp,
    retry: u64,
) -> Result<()> {
    let mut deliveries = txn.open_table(DELIVERIES)?;
    let mut delivery: Delivery = match deliveries.get(id)? {
        Some(v) => dec("deliveries", v.value())?,
        None => return Err(Error::NotFound(format!("delivery {id}"))),
    };
    delivery.attempts += 1;
    let (delivered, result) = match outcome {
        Ok(status) => (true, status),
        Err(error) => (false, error),
    };
    delivery.delivered = delivered;
    delivery.last_result = Some(result);
    delivery.next_at = match delivered || delivery.attempts >= MAX_ATTEMPTS {
        true => None,
        false => Some(now + retry.saturating_mul(1 << (delivery.attempts - 1))),
    };
    if delivery.next_at.is_none() {
        txn.open_table(PENDING_DELIVERIES)?.remove(id)?;
    }
    deliveries.insert(id, enc(&delivery).as_slice())?;
    Ok(())
}

pub(crate) fn redeliver(txn: &WriteTransaction, id: u64) -> Result<()> {
    let mut deliveries = txn.open_table(DELIVERIES)?;
    let mut delivery: Delivery = match deliveries.get(id)? {
        Some(v) => dec("deliveries", v.value())?,
        None => return Err(Error::NotFound(format!("delivery {id}"))),
    };
    delivery.attempts = 0;
    delivery.next_at = Some(0);
    delivery.delivered = false;
    deliveries.insert(id, enc(&delivery).as_slice())?;
    txn.open_table(PENDING_DELIVERIES)?.insert(id, ())?;
    Ok(())
}

impl Reader {
    /// Returns all webhooks, ordered by folded name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn webhooks(&self) -> Result<Vec<Webhook>> {
        self.txn()
            .open_table(WEBHOOKS)?
            .iter()?
            .map(|r| dec("webhooks", r?.1.value()))
            .collect()
    }

    /// Returns up to `limit` pending deliveries due at `now`, oldest first,
    /// with the URL and secret of their webhook.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn due_deliveries(&self, now: Timestamp, limit: usize) -> Result<Vec<(Delivery, Webhook)>> {
        let deliveries = self.txn().open_table(DELIVERIES)?;
        let hooks = self.txn().open_table(WEBHOOKS)?;
        let mut due = Vec::new();
        for row in self.txn().open_table(PENDING_DELIVERIES)?.iter()? {
            let id = row?.0.value();
            let Some(v) = deliveries.get(id)? else {
                continue;
            };
            let delivery: Delivery = dec("deliveries", v.value())?;
            if delivery.next_at.is_none_or(|t| t > now) {
                continue;
            }
            // Deliveries of deleted webhooks stay pending until it is recreated.
            if let Some(h) = hooks.get(name_key(&delivery.webhook).as_str())? {
                due.push((delivery, dec("webhooks", h.value())?));
                if due.len() == limit {
                    break;
                }
            }
        }
        Ok(due)
    }

    /// Returns the newest `limit` deliveries to `webhook`, or to all
    /// webhooks, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn deliveries(&self, webhook: Option<&str>, limit: usize) -> Result<Vec<Delivery>> {
        let mut out = Vec::new();
        for row in self.txn().open_table(DELIVERIES)?.iter()?.rev() {
            let delivery: Delivery = dec("deliveries", row?.1.value())?;
            if webhook.is_none_or(|w| name_key(w) == name_key(&delivery.webhook)) {
                out.push(delivery);
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }
}
