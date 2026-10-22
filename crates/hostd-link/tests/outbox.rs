//! The host's outbox over the link (Cloud PROJECT-SERVICE §9.8.2, Cloud #2739 step 1) against a stub gateway on this
//! computer: entries written while offline go out in seq order after the welcome; a partial ack sends the rest again
//! later; a batch the gateway never acknowledged is sent again on the next connection (the platform dedupes by event
//! id); acknowledged entries leave and stay gone across a restart of hostd; an event written while connected goes out
//! at once.

use std::sync::Arc;
use std::time::Duration;

use agentrouter_hostd_link::outbox::{DEFAULT_CAP_BYTES, NewEvent, Outbox};
use agentrouter_hostd_link::{Event, LinkInfo, Options, start};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

const LINK: &str = "lnk_0123456789abcdef0123456789abcdef";

fn info() -> LinkInfo {
    LinkInfo {
        id: LINK.into(),
        folder: "feeds".into(),
        os: "linux".into(),
        arch: "x64".into(),
        app: "cli".into(),
        version: "0.1.0".into(),
        sandbox: String::new(),
        read_only: false,
        folders: vec!["/work/feeds".into()],
        full: true,
        placement: "pc".into(),
        capacity: Value::Null,
        labels: Value::Null,
        pool: None,
        resources: serde_json::Value::Null,
        software: serde_json::Value::Null,
        local_data: serde_json::Value::Null,
    }
}

fn row(n: u64) -> NewEvent {
    NewEvent {
        space: "wrk_feed".into(),
        instance: Some("ins_feed".into()),
        event: None,
        table: Some("feed".into()),
        payload: json!({"id": n.to_string()}),
        key: Some(n.to_string()),
        cause: None,
    }
}

/// The next `events` frame (heartbeats and other frames skipped).
async fn next_events<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match timeout(Duration::from_secs(20), ws.next())
            .await
            .expect("a frame in time")
        {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["op"] == "events" {
                    return v;
                }
            }
            Some(Ok(_)) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

fn seqs(frame: &Value) -> Vec<u64> {
    frame["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["seq"].as_u64().unwrap())
        .collect()
}

async fn welcome<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let hello = match timeout(Duration::from_secs(20), ws.next()).await.unwrap() {
        Some(Ok(Message::Text(t))) => serde_json::from_str::<Value>(t.as_str()).unwrap(),
        other => panic!("hello expected, got {other:?}"),
    };
    assert_eq!(hello["op"], "hello");
    ws.send(Message::text(
        json!({"op": "welcome", "link": LINK}).to_string(),
    ))
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_entries_flush_in_order_and_leave_only_after_the_ack() {
    let mut r = [0u8; 6];
    getrandom::fill(&mut r).unwrap();
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "ar-outbox-link-{}",
        r.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let path = Outbox::path_for(&dir, LINK);

    // Written while offline (the link not even started).
    let outbox = Arc::new(Outbox::open(&path, LINK, DEFAULT_CAP_BYTES).unwrap());
    for n in 1..=3 {
        assert_eq!(outbox.append(&row(n)).unwrap(), n);
    }
    let producer = outbox.producer().to_string();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (handle, mut events) = start(Options {
        gateway: format!("http://127.0.0.1:{port}"),
        info: info(),
        tokens: Arc::new(|| Ok("ldt_x".into())),
        audit_head: Arc::new(|| json!({"seq": 0, "hash": ""})),
        load: None,
        bridge: None,
        outbox: Some(outbox.clone()),
    });

    // Connection 1: everything waiting goes out in seq order, with its event id.
    let (tcp, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    welcome(&mut ws).await;
    let first = next_events(&mut ws).await;
    assert_eq!(first["host"], producer.as_str());
    assert_eq!(seqs(&first), vec![1, 2, 3]);
    assert_eq!(first["items"][0]["id"], format!("{producer}:1"));
    assert_eq!(first["items"][0]["space"], "wrk_feed");
    assert_eq!(first["items"][0]["table"], "feed");
    assert_eq!(first["items"][0]["instance"], "ins_feed");

    // A partial ack (the platform took 1 and 2, refused nothing, could not take 3 yet): 1 and 2 leave; 3 again soon.
    ws.send(Message::text(
        json!({"op": "events_ack", "host": producer, "seq": 2, "refused": [], "retry_ms": 100})
            .to_string(),
    ))
    .await
    .unwrap();
    let again = next_events(&mut ws).await;
    assert_eq!(seqs(&again), vec![3]);
    assert_eq!(outbox.status().pending, 1);

    // The link drops before the ack: nothing is lost, 3 goes out again on the next connection.
    drop(ws);
    let (tcp, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    welcome(&mut ws).await;
    let replay = next_events(&mut ws).await;
    assert_eq!(seqs(&replay), vec![3]);
    assert_eq!(replay["items"][0]["id"], format!("{producer}:3"));
    // An ack for another producer (an older database of this link) changes nothing.
    ws.send(Message::text(
        json!({"op": "events_ack", "host": format!("{LINK}.0000000000000000"), "seq": 3})
            .to_string(),
    ))
    .await
    .unwrap();
    ws.send(Message::text(
        json!({"op": "events_ack", "host": producer, "seq": 3}).to_string(),
    ))
    .await
    .unwrap();

    // Written while connected: out at once, after the earlier ones.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(outbox.status().pending, 0);
    assert_eq!(outbox.append(&row(4)).unwrap(), 4);
    let live = next_events(&mut ws).await;
    assert_eq!(seqs(&live), vec![4]);
    ws.send(Message::text(
        json!({"op": "events_ack", "host": producer, "seq": 4}).to_string(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(outbox.status().acked, 4);

    handle.stop();
    loop {
        if let Event::Ended(_) = timeout(Duration::from_secs(20), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            break;
        }
    }
    drop(outbox);
    // hostd restarted: the acknowledged entries stay gone, the seq continues.
    let reopened = Outbox::open(&path, LINK, DEFAULT_CAP_BYTES).unwrap();
    assert_eq!(reopened.producer(), producer);
    assert_eq!(reopened.status().pending, 0);
    assert_eq!(reopened.append(&row(5)).unwrap(), 5);
    drop(reopened);
    let _ = std::fs::remove_dir_all(&dir);
}
