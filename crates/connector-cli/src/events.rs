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

    fn append_event(
        &self,
        e: &agentrouter_aggregator::feeds::RuntimeEvent,
    ) -> Result<u64, agentrouter_aggregator::Failure> {
        let event = agentrouter_hostd_link::outbox::NewEvent {
            space: e.space.clone(),
            instance: Some(e.instance.clone()),
            event: Some(e.event.clone()),
            table: None,
            payload: e.payload.clone(),
            key: e.key.clone(),
            cause: Some(e.cause.clone()),
        };
        self.0
            .append(&event)
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

#[cfg(test)]
mod tests {
    use super::*;
    use agentrouter_aggregator::feeds::{EventSink, RuntimeEvent};
    use serde_json::json;

    /// Cloud #3607: a runtime's event goes into the outbox as an event of its space, through its instance, with its
    /// key and cause, and leaves in the next `events` batch.
    #[test]
    fn a_runtime_event_is_written_as_an_event_of_its_space() {
        // Under target/ (never the temp folder, docs/AV-HYGIENE.md).
        let exe = std::env::current_exe().unwrap();
        let target = exe.ancestors().nth(3).unwrap();
        let dir = target
            .join("tmp")
            .join(format!("ar-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let link = "lnk_0123456789abcdef0123456789abcdef";
        let outbox = open_outbox(&dir, link, |_| {}).unwrap();
        let sink = OutboxSink(outbox.clone());
        let e = RuntimeEvent {
            space: "wrk_709329401e7d8c5a5a44067197b06854".into(),
            instance: "ins_0123456789abcdef01234567".into(),
            event: "round.result".into(),
            payload: json!({"round": 1}),
            key: Some("r1".into()),
            cause: "runtime:ins_0123456789abcdef01234567 job-1".into(),
        };
        let seq = sink.append_event(&e).unwrap();
        let batch = outbox.batch(0).unwrap();
        let item = &batch["items"][0];
        assert_eq!(item["seq"], seq);
        assert_eq!(item["event"], "round.result");
        assert_eq!(item["instance"], e.instance.as_str());
        assert_eq!(item["key"], "r1");
        assert!(item.get("table").is_none_or(|t| t.is_null()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
