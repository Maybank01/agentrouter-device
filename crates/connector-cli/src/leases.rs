//! Lease epochs on this host (Cloud PROJECT-SERVICE §4.8 "Lease epochs", Cloud #2246): a job the platform started for an
//! instance (`job_start` with `lease {instance, epoch}`) keeps running while the link is down, and the platform may end
//! that instance meanwhile (its lease ran out, someone stopped it). After every welcome this host lists those jobs
//! (`{"op":"instances"}`); the platform fences them against the instance record and answers the stale ones
//! (`lease_lost`), which are stopped here — never a job that now runs under another instance's lease.

use agentrouter_aggregator::Aggregator;
use agentrouter_hostd_link::Handle;
use serde_json::Value;

/// After a welcome: send the running leased jobs (nothing when there are none).
pub(crate) fn after_welcome(agg: &Aggregator, handle: &Handle) {
    let (agg, handle) = (agg.clone(), handle.clone());
    tokio::spawn(async move {
        let list = agg.host_leases().await;
        if !list.is_empty() {
            handle.instances(Value::Array(list));
        }
    });
}

/// The platform's `lease_lost`: stop those jobs, one line each.
pub(crate) fn lost(agg: &Aggregator, jobs: Vec<Value>, line: fn(&str)) {
    let agg = agg.clone();
    tokio::spawn(async move {
        for o in agg.lease_lost(&jobs).await {
            let text = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            if o.get("stopped").and_then(Value::as_bool) == Some(true) {
                line(&format!(
                    "stopped {} of {}: its instance {} (lease_lost)",
                    text("job"),
                    text("instance"),
                    text("why")
                ));
            } else {
                line(&format!(
                    "kept {} ({}): {}",
                    text("job"),
                    text("instance"),
                    text("error")
                ));
            }
        }
    });
}
