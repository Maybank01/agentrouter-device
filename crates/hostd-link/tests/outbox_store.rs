//! The outbox store on its own (Cloud #2739 step 1): seqs and the producer survive a reopen, acks remove only what
//! they name, refusals are kept, the disk cap pauses producers, batches stay within their limits.
use std::path::PathBuf;

use agentrouter_hostd_link::outbox::{
    BATCH_ITEMS, DEFAULT_CAP_BYTES, NewEvent, Outbox, PAYLOAD_MAX, parse_refused,
};
use serde_json::json;

fn tmp(name: &str) -> PathBuf {
    let mut r = [0u8; 6];
    getrandom::fill(&mut r).unwrap();
    let id: String = r.iter().map(|b| format!("{b:02x}")).collect();
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("ar-outbox-{name}-{id}"))
        .join("o.sqlite")
}

fn ev(n: u64) -> NewEvent {
    NewEvent {
        space: "wrk_feed".into(),
        instance: Some("ins_1".into()),
        event: None,
        table: Some("feed".into()),
        op: None,
        payload: json!({"id": n.to_string(), "title": format!("item {n}")}),
        key: Some(n.to_string()),
        cause: None,
    }
}

#[test]
fn seqs_survive_reopen_and_acks_remove_only_up_to_the_ack() {
    let path = tmp("reopen");
    let link = "lnk_0123456789abcdef0123456789abcdef";
    let producer;
    {
        let o = Outbox::open(&path, link, DEFAULT_CAP_BYTES).unwrap();
        producer = o.producer().to_string();
        assert!(producer.starts_with(&format!("{link}.")));
        for n in 1..=5 {
            assert_eq!(o.append(&ev(n)).unwrap(), n);
        }
        let batch = o.batch(0).unwrap();
        assert_eq!(batch["op"], "events");
        assert_eq!(batch["host"], producer.as_str());
        let items = batch["items"].as_array().unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!(items[0]["seq"], 1);
        assert_eq!(items[0]["id"], format!("{producer}:1"));
        assert_eq!(items[0]["key"], "1");
        assert_eq!(items[0]["instance"], "ins_1");
        assert_eq!(o.ack(2, &[]), 2);
    }
    // A restart (or a power cut after the commit): the rest is still there, the seq continues, the producer stays.
    let o = Outbox::open(&path, link, DEFAULT_CAP_BYTES).unwrap();
    assert_eq!(o.producer(), producer);
    let s = o.status();
    assert_eq!((s.pending, s.acked, s.last_seq), (3, 2, 5));
    assert_eq!(o.batch(0).unwrap()["items"][0]["seq"], 3);
    assert_eq!(o.append(&ev(6)).unwrap(), 6);
    // Everything acknowledged: the seq still never goes back (AUTOINCREMENT).
    o.ack(6, &[]);
    assert!(o.batch(0).is_none());
    assert_eq!(o.append(&ev(7)).unwrap(), 7);
    // Another link's database is refused, not reused.
    assert!(
        Outbox::open(
            &path,
            "lnk_ffffffffffffffffffffffffffffffff",
            DEFAULT_CAP_BYTES
        )
        .is_err()
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_new_database_never_repeats_an_event_id() {
    let link = "lnk_0123456789abcdef0123456789abcdef";
    let a = tmp("ida");
    let b = tmp("idb");
    let oa = Outbox::open(&a, link, DEFAULT_CAP_BYTES).unwrap();
    let ob = Outbox::open(&b, link, DEFAULT_CAP_BYTES).unwrap();
    oa.append(&ev(1)).unwrap();
    ob.append(&ev(1)).unwrap();
    assert_ne!(oa.event_id(1), ob.event_id(1));
    let _ = std::fs::remove_dir_all(a.parent().unwrap());
    let _ = std::fs::remove_dir_all(b.parent().unwrap());
}

#[test]
fn refused_entries_leave_and_are_kept_for_the_status() {
    let path = tmp("refused");
    let o = Outbox::open(
        &path,
        "lnk_0123456789abcdef0123456789abcdef",
        DEFAULT_CAP_BYTES,
    )
    .unwrap();
    for n in 1..=3 {
        o.append(&ev(n)).unwrap();
    }
    let refused = parse_refused(Some(
        &json!([{"seq": 2, "code": "EVENT_INVALID", "message": "wrk_feed declares no table feed"}]),
    ));
    assert_eq!(o.ack(3, &refused), 3);
    let s = o.status();
    assert_eq!((s.pending, s.refused), (0, 1));
    let last = s.last_refused.unwrap();
    assert_eq!(last["code"], "EVENT_INVALID");
    assert_eq!(last["table"], "feed");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn the_cap_pauses_producers_and_bad_events_are_refused_at_once() {
    let path = tmp("cap");
    let o = Outbox::open(&path, "lnk_0123456789abcdef0123456789abcdef", 600).unwrap();
    let mut accepted = 0;
    let full = loop {
        match o.append(&ev(accepted + 1)) {
            Ok(_) => accepted += 1,
            Err(r) => break r,
        }
    };
    assert_eq!(full.code, "OUTBOX_FULL");
    assert!(accepted >= 2 && o.status().bytes <= 600);
    // After the platform took them there is room again.
    o.ack(accepted, &[]);
    assert!(o.append(&ev(99)).is_ok());

    let mut big = ev(1);
    big.payload = json!({"text": "x".repeat(PAYLOAD_MAX)});
    assert_eq!(o.append(&big).unwrap_err().code, "EVENT_TOO_LARGE");
    let mut bad = ev(1);
    bad.table = Some("Feed New".into());
    assert_eq!(o.append(&bad).unwrap_err().code, "EVENT_INVALID");
    bad = ev(1);
    bad.payload = json!([1]);
    assert_eq!(o.append(&bad).unwrap_err().code, "EVENT_INVALID");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn batches_stay_within_their_limits() {
    let path = tmp("batch");
    let o = Outbox::open(
        &path,
        "lnk_0123456789abcdef0123456789abcdef",
        DEFAULT_CAP_BYTES,
    )
    .unwrap();
    for n in 1..=(BATCH_ITEMS as u64 + 10) {
        o.append(&ev(n)).unwrap();
    }
    let first = o.batch(0).unwrap();
    assert_eq!(first["items"].as_array().unwrap().len(), BATCH_ITEMS);
    let next = o.batch(BATCH_ITEMS as u64).unwrap();
    assert_eq!(next["items"][0]["seq"], BATCH_ITEMS as u64 + 1);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_feed_key_is_written_once_and_a_refusal_leaves_it_unseen() {
    let path = tmp("seen");
    const LINK: &str = "lnk_0123456789abcdef0123456789abcdef";
    let outbox = Outbox::open(&path, LINK, 600).unwrap();
    let row = |key: &str, size: usize| NewEvent {
        space: "spc_1".into(),
        instance: Some("ins_1".into()),
        event: None,
        table: Some("feed".into()),
        op: None,
        payload: json!({"id": key, "text": "x".repeat(size)}),
        key: Some(format!("f:{key}")),
        cause: None,
    };
    assert_eq!(
        outbox.append_new("spc_1/f", &row("1", 10)).unwrap(),
        Some(1)
    );
    assert_eq!(outbox.append_new("spc_1/f", &row("1", 10)).unwrap(), None);
    assert_eq!(
        outbox.append_new("spc_1/g", &row("1", 10)).unwrap(),
        Some(2)
    );
    // Past the cap: refused, and the key stays unseen so the next poll writes it.
    let full = outbox.append_new("spc_1/f", &row("2", 700)).unwrap_err();
    assert_eq!(full.code, "OUTBOX_FULL");
    assert_eq!(outbox.seen_count("spc_1/f"), 1);
    assert_eq!(outbox.status().pending, 2);
    // The seen-set survives a reopen.
    drop(outbox);
    let outbox = Outbox::open(&path, LINK, 600).unwrap();
    assert_eq!(outbox.append_new("spc_1/f", &row("1", 10)).unwrap(), None);
    outbox.ack(2, &[]);
    assert_eq!(
        outbox.append_new("spc_1/f", &row("2", 10)).unwrap(),
        Some(3)
    );
    outbox.forget("spc_1/f");
    assert_eq!(outbox.seen_count("spc_1/f"), 0);
}
