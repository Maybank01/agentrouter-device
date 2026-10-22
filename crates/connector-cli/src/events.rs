//! The host's outbox (Cloud PROJECT-SERVICE §9.8.2, Cloud #2739): one per link, in hostd's own data directory, flushed
//! by the link layer (`agentrouter_hostd_link::outbox`). Every event this host produces for a space goes through it.

use std::path::Path;
use std::sync::Arc;

use agentrouter_hostd_link::outbox::{DEFAULT_CAP_BYTES, Outbox};

/// Opens `<data dir>/outbox/<link>.sqlite`. None (said in one line) when it cannot be opened: the link still works,
/// only nothing on this host can produce events until it can.
pub fn open_outbox(data: &Path, link: &str, line: impl Fn(&str)) -> Option<Arc<Outbox>> {
    let path = Outbox::path_for(data, link);
    match Outbox::open(&path, link, DEFAULT_CAP_BYTES) {
        Ok(outbox) => {
            let s = outbox.status();
            if s.pending > 0 {
                line(&format!(
                    "  事件 {} 条在本机等待上传（离线期间产生的），连上后按顺序补发",
                    s.pending
                ));
            }
            Some(Arc::new(outbox))
        }
        Err(e) => {
            line(&format!("  事件发件箱打不开（OUTBOX_UNAVAILABLE）：{e}"));
            None
        }
    }
}

/// The outbox as the feed runner's sink (`agentrouter_aggregator::feeds`): a feed's dedupe scope is its seen-set here,
/// marked in the same transaction as the write.
pub struct OutboxSink(pub Arc<Outbox>);

impl agentrouter_aggregator::feeds::EventSink for OutboxSink {
    fn append_new(
        &self,
        scope: &str,
        row: &agentrouter_aggregator::feeds::FeedRow,
    ) -> Result<bool, agentrouter_aggregator::Failure> {
        let event = agentrouter_hostd_link::outbox::NewEvent {
            space: row.space.clone(),
            instance: Some(row.instance.clone()),
            event: None,
            table: Some(row.table.clone()),
            payload: row.payload.clone(),
            key: Some(row.key.clone()),
            cause: Some(row.cause.clone()),
        };
        self.0
            .append_new(scope, &event)
            .map(|seq| seq.is_some())
            .map_err(|r| agentrouter_aggregator::Failure::new(r.code, r.message))
    }

    fn nearly_full(&self) -> bool {
        self.0.nearly_full()
    }
}

/// The sink of an opened outbox.
pub fn sink(
    outbox: &Option<Arc<Outbox>>,
) -> Option<Arc<dyn agentrouter_aggregator::feeds::EventSink>> {
    outbox
        .clone()
        .map(|o| Arc::new(OutboxSink(o)) as Arc<dyn agentrouter_aggregator::feeds::EventSink>)
}
