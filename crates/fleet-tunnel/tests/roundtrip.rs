//! End-to-end round-trip test for the fleet-tunnel daemon (restores the coverage the
//! Python selftest had before the Rust port). Stands up a stub board (WS server) and a stub
//! upstream (HTTP), spawns the real `fleet-tunnel` binary against them, and asserts the full
//! hello / hello_ok / req -> local-forward -> resp path, including the base64 body round-trip.
//!
//! Gated to `--features transport` (the daemon binary it drives is transport-gated).
#![cfg(feature = "transport")]

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

const REQ_BODY: &[u8] = br#"{"recipient":"agent-x","type":"task.assigned","event_seq":42}"#;
const RESP_BODY: &[u8] = br#"{"status":"woken","agent":"agent-x"}"#;

/// Stub notifier: serve every connection, draining the request and replying 200 with RESP_BODY
/// (mirrors `fleet notify`, which accepts any method/path and returns 200). Loops so it answers
/// both the tunnel's forwarded `req` and the health probe's upstream-reachability GET.
async fn stub_upstream(listener: TcpListener) {
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => break,
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await; // drain the request line + headers + body
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                RESP_BODY.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.write_all(RESP_BODY).await;
            let _ = sock.flush().await;
        });
    }
}

/// Minimal HTTP/1.1 GET for probing the daemon's health endpoint: returns (status_code, body).
async fn http_get(addr: SocketAddr) -> Result<(u16, String), String> {
    let mut sock = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| e.to_string())?;
    sock.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw)
        .await
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("no status line in response: {head:?}"))?;
    Ok((status, body.to_string()))
}

/// Stub board: accept the daemon's dial, validate the hello, ack, issue one req, and verify the
/// resp — signals the outcome on `done`. On success it then holds the socket OPEN (draining +
/// answering pings) so the daemon stays connected while the test probes health; the socket closes
/// when the test kills the daemon.
async fn stub_board(listener: TcpListener, done: tokio::sync::oneshot::Sender<Result<(), String>>) {
    let tcp = match listener.accept().await {
        Ok((t, _)) => t,
        Err(e) => {
            let _ = done.send(Err(e.to_string()));
            return;
        }
    };
    let mut ws = match tokio_tungstenite::accept_async(tcp).await {
        Ok(w) => w,
        Err(e) => {
            let _ = done.send(Err(format!("ws upgrade: {e}")));
            return;
        }
    };
    let result = verify_roundtrip(&mut ws).await;
    let ok = result.is_ok();
    let _ = done.send(result);
    if ok {
        // Keep the tunnel up for the health probe; answer pings, drain until the socket closes.
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Ping(p) = msg {
                let _ = ws.send(Message::Pong(p)).await;
            }
        }
    }
}

/// Drive one hello / hello_ok / req -> resp exchange on an established board socket and verify the
/// echoed body round-trips.
async fn verify_roundtrip(ws: &mut WebSocketStream<TcpStream>) -> Result<(), String> {
    // hello
    let hello = match ws.next().await {
        Some(Ok(Message::Text(t))) => t,
        other => return Err(format!("expected hello text frame, got {other:?}")),
    };
    let hello: serde_json::Value =
        serde_json::from_str(hello.as_str()).map_err(|e| e.to_string())?;
    if hello["t"] != "hello" {
        return Err(format!("first frame not hello: {hello}"));
    }
    if hello["agents"][0] != "agent-x" {
        return Err(format!("hello missing declared agent: {hello}"));
    }

    ws.send(Message::Text(r#"{"t":"hello_ok","keepalive":30}"#.into()))
        .await
        .map_err(|e| e.to_string())?;

    let req = serde_json::json!({
        "t": "req", "id": 1, "method": "POST", "path": "/wake",
        "headers": {"content-type": "application/json"},
        "body": BASE64.encode(REQ_BODY),
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .map_err(|e| e.to_string())?;

    // resp (skip any interim ping)
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                let f: serde_json::Value =
                    serde_json::from_str(t.as_str()).map_err(|e| e.to_string())?;
                if f["t"] == "ping" {
                    continue;
                }
                if f["t"] != "resp" {
                    return Err(format!("expected resp, got {f}"));
                }
                if f["id"] != 1 {
                    return Err(format!("resp id mismatch: {f}"));
                }
                if f["status"] != 200 {
                    return Err(format!("resp status not 200: {f}"));
                }
                let body = f["body"].as_str().unwrap_or("");
                let decoded = BASE64.decode(body).map_err(|e| e.to_string())?;
                if decoded != RESP_BODY {
                    return Err(format!("resp body round-trip mismatch: {decoded:?}"));
                }
                return Ok(());
            }
            Some(Ok(Message::Ping(_))) => {}
            other => return Err(format!("expected resp, got {other:?}")),
        }
    }
}

#[tokio::test]
async fn hello_handshake_and_req_forward_roundtrip() {
    let board_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let board_port = board_listener.local_addr().unwrap().port();
    let up_port = up_listener.local_addr().unwrap().port();

    // Grab a free port for the health probe (bind then drop; the daemon rebinds it).
    let health_addr: SocketAddr = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(stub_board(board_listener, done_tx));
    tokio::spawn(stub_upstream(up_listener));

    // Config for the daemon under test (health probe enabled so we can assert liveness too).
    let cfg_path = std::env::temp_dir().join(format!(
        "fleet-tunnel-roundtrip-{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &cfg_path,
        format!(
            "board_ws = \"ws://127.0.0.1:{board_port}/tunnel/ws\"\n\
             upstream = \"http://127.0.0.1:{up_port}\"\n\
             host_id = \"roundtrip-host\"\n\
             agents = [\"agent-x\"]\n\
             health_addr = \"{health_addr}\"\n"
        ),
    )
    .unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_fleet-tunnel"))
        .arg("--config")
        .arg(&cfg_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn fleet-tunnel");

    let result = tokio::time::timeout(Duration::from_secs(20), done_rx).await;

    // The stub board holds the socket open after signaling, so the tunnel is still live here — the
    // probe should report healthy while the daemon is connected.
    let health = match &result {
        Ok(Ok(Ok(()))) => Some(health_probe(health_addr).await),
        _ => None,
    };

    let _ = child.kill().await;
    let _ = std::fs::remove_file(&cfg_path);

    match result {
        Ok(Ok(Ok(()))) => {} // round-trip verified
        Ok(Ok(Err(e))) => panic!("round-trip failed: {e}"),
        Ok(Err(_)) => panic!("board task dropped without signaling"),
        Err(_) => panic!("timed out waiting for the req->resp round-trip"),
    }

    match health {
        Some(Ok((status, body))) => {
            assert_eq!(status, 200, "health should be 200 while live; body={body}");
            assert!(
                body.contains(r#""board_ws":"connected""#),
                "health body={body}"
            );
            assert!(
                body.contains(r#""upstream_reachable":true"#),
                "health body={body}"
            );
            assert!(body.contains(r#""ok":true"#), "health body={body}");
        }
        Some(Err(e)) => panic!("health probe failed: {e}"),
        None => {}
    }
}

/// Poll the daemon's health endpoint until it answers (it binds at startup, so this is quick).
async fn health_probe(addr: SocketAddr) -> Result<(u16, String), String> {
    let mut last = String::new();
    for _ in 0..20 {
        match http_get(addr).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                last = e;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    Err(format!("health endpoint never responded: {last}"))
}
