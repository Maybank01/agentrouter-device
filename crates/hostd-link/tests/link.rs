//! hostd's link layer against a stub gateway on this computer: hello v2 with the link, welcome, frames both ways,
//! session close, the audit head on the heartbeat, reconnect after a drop (answers produced while down are not
//! replayed), replacement (4409) and revocation end it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentrouter_hostd_link::{End, Event, LinkInfo, Options, TokenError, start};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

fn info() -> LinkInfo {
    LinkInfo {
        id: "lnk_0123456789abcdef0123456789abcdef".into(),
        folder: "my-mod".into(),
        os: "linux".into(),
        arch: "x64".into(),
        app: "cli".into(),
        version: "0.1.0".into(),
        sandbox: "srt@0.0.81".into(),
        read_only: false,
        folders: vec!["/work/my-mod".into()],
        full: false,
        placement: "pc".into(),
        capacity: serde_json::Value::Null,
        labels: serde_json::Value::Null,
        pool: Some("渲染机".into()),
        resources: serde_json::Value::Null,
        software: serde_json::Value::Null,
    }
}

async fn next_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Event {
    timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("an event in time")
        .expect("the link layer is running")
}

async fn read_json<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match timeout(Duration::from_secs(20), ws.next()).await.unwrap() {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(t.as_str()).unwrap(),
            Some(Ok(_)) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_frames_heartbeat_reconnect_and_ends() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tokens_issued = Arc::new(AtomicUsize::new(0));
    let issued = tokens_issued.clone();
    let (handle, mut events) = start(Options {
        gateway: format!("http://127.0.0.1:{port}"),
        info: info(),
        tokens: Arc::new(move || {
            let n = issued.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // The first try fails like a network error: the link layer retries.
                Err(TokenError::Retry("offline".into()))
            } else {
                Ok(format!("ldt_{n}"))
            }
        }),
        audit_head: Arc::new(|| json!({"seq": 7, "hash": "ab".repeat(32)})),
        load: None,
        bridge: None,
        outbox: None,
    });

    // The first token try failed: Down, then a connection.
    assert!(matches!(next_event(&mut events).await, Event::Down { .. }));

    // Connection 1: hello v2 with the link, welcome, the audit head right away.
    let (tcp, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    let hello = read_json(&mut ws).await;
    assert_eq!(hello["op"], "hello");
    assert_eq!(hello["v"], 2);
    assert_eq!(hello["token"], "ldt_1");
    assert_eq!(hello["link"]["id"], info().id);
    assert_eq!(hello["link"]["folder"], "my-mod");
    assert_eq!(hello["link"]["app"], "cli");
    assert_eq!(hello["link"]["sandbox"], "srt@0.0.81");
    assert_eq!(hello["link"]["placement"], "pc");
    // The owner's pool (Cloud VISION D2, #2576).
    assert_eq!(hello["link"]["pool"], "渲染机");
    assert!(
        hello["link"].get("path").is_none(),
        "no absolute path leaves the device"
    );
    ws.send(Message::text(
        json!({"op": "welcome", "link": info().id}).to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(next_event(&mut events).await, Event::Up { link: info().id });
    let head = read_json(&mut ws).await;
    assert_eq!(
        head,
        json!({"op": "audit_head", "seq": 7, "hash": "ab".repeat(32)})
    );

    // Gateway → device: an MCP message of a session, then the session's close.
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    ws.send(Message::text(
        json!({"s": "mcs_a", "m": request}).to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        Event::Data {
            s: "mcs_a".into(),
            m: request
        }
    );
    // Device → gateway: the answer, unchanged inside the envelope.
    let answer = json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}});
    handle.send("mcs_a", answer.clone());
    assert_eq!(read_json(&mut ws).await, json!({"s": "mcs_a", "m": answer}));
    handle.close("mcs_a");
    assert_eq!(
        read_json(&mut ws).await,
        json!({"op": "close", "s": "mcs_a"})
    );
    ws.send(Message::text(
        json!({"op": "close", "s": "mcs_b"}).to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        Event::Close { s: "mcs_b".into() }
    );

    // The link drops: Down; an answer produced meanwhile is not replayed on the next connection.
    drop(ws);
    assert!(matches!(next_event(&mut events).await, Event::Down { .. }));
    handle.send("mcs_a", json!({"jsonrpc": "2.0", "id": 2, "result": {}}));

    // Connection 2: a fresh token; the late answer never arrives.
    let (tcp, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    let hello = read_json(&mut ws).await;
    assert_eq!(hello["token"], "ldt_2");
    ws.send(Message::text(
        json!({"op": "welcome", "link": info().id}).to_string(),
    ))
    .await
    .unwrap();
    assert!(matches!(next_event(&mut events).await, Event::Up { .. }));
    assert_eq!(read_json(&mut ws).await["op"], "audit_head");
    handle.send("mcs_a", json!({"jsonrpc": "2.0", "id": 3, "result": {}}));
    assert_eq!(read_json(&mut ws).await["m"]["id"], 3, "id 2 was dropped");

    // Another link for the same folder took over: the link layer ends, it does not fight back.
    ws.send(Message::Close(Some(CloseFrame {
        code: CloseCode::from(4409),
        reason: "replaced".into(),
    })))
    .await
    .unwrap();
    assert_eq!(next_event(&mut events).await, Event::Ended(End::Replaced));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_and_outdated_end_the_link_and_stop_is_clean() {
    for (frame, want) in [
        (
            json!({"op": "revoked", "reason": "unlinked on the web"}),
            End::Revoked("unlinked on the web".into()),
        ),
        (
            json!({"op": "error", "code": "DEVICE_OUTDATED", "message": "update"}),
            End::Refused {
                code: "DEVICE_OUTDATED".into(),
                message: "update".into(),
            },
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_handle, mut events) = start(Options {
            gateway: format!("http://localhost:{port}"),
            info: info(),
            tokens: Arc::new(|| Ok("ldt_x".into())),
            audit_head: Arc::new(|| json!({"seq": 0, "hash": ""})),
            load: None,
            bridge: None,
            outbox: None,
        });
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let _hello = read_json(&mut ws).await;
        ws.send(Message::text(frame.to_string())).await.unwrap();
        assert_eq!(next_event(&mut events).await, Event::Ended(want));
    }

    // A revoked identity at token time ends it too; stop() ends a running link.
    let (_h, mut events) = start(Options {
        gateway: "http://127.0.0.1:9".into(),
        info: info(),
        tokens: Arc::new(|| Err(TokenError::Revoked("gone".into()))),
        audit_head: Arc::new(|| json!({})),
        load: None,
        bridge: None,
        outbox: None,
    });
    assert_eq!(
        next_event(&mut events).await,
        Event::Ended(End::Revoked("gone".into()))
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (handle, mut events) = start(Options {
        gateway: format!("http://127.0.0.1:{port}"),
        info: info(),
        tokens: Arc::new(|| Ok("ldt_x".into())),
        audit_head: Arc::new(|| json!({"seq": 0, "hash": ""})),
        load: None,
        bridge: None,
        outbox: None,
    });
    let (tcp, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    let _hello = read_json(&mut ws).await;
    ws.send(Message::text(
        json!({"op": "welcome", "link": "lnk_x"}).to_string(),
    ))
    .await
    .unwrap();
    assert!(matches!(next_event(&mut events).await, Event::Up { .. }));
    handle.stop();
    assert_eq!(next_event(&mut events).await, Event::Ended(End::Stopped));
}
