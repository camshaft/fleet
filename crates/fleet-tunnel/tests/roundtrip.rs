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
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const REQ_BODY: &[u8] = br#"{"recipient":"agent-x","type":"task.assigned","event_seq":42}"#;
const RESP_BODY: &[u8] = br#"{"status":"woken","agent":"agent-x"}"#;

/// Stub notifier: accept one connection, drain the request, always reply 200 with RESP_BODY
/// (mirrors `fleet notify`, which accepts any method/path and returns 200).
async fn stub_upstream(listener: TcpListener) {
    if let Ok((mut sock, _)) = listener.accept().await {
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf).await; // drain the request line + headers + body
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            RESP_BODY.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.write_all(RESP_BODY).await;
        let _ = sock.flush().await;
    }
}

/// Stub board: accept the daemon's dial, validate the hello, ack, issue one req, and verify the
/// resp. Returns Ok(()) on a clean round-trip.
async fn stub_board(listener: TcpListener) -> Result<(), String> {
    let (tcp, _) = listener.accept().await.map_err(|e| e.to_string())?;
    let mut ws = tokio_tungstenite::accept_async(tcp)
        .await
        .map_err(|e| format!("ws upgrade: {e}"))?;

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

    let board = tokio::spawn(stub_board(board_listener));
    tokio::spawn(stub_upstream(up_listener));

    // Config for the daemon under test.
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
             agents = [\"agent-x\"]\n"
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

    let result = tokio::time::timeout(Duration::from_secs(20), board).await;

    let _ = child.kill().await;
    let _ = std::fs::remove_file(&cfg_path);

    match result {
        Ok(Ok(Ok(()))) => {} // board task finished, round-trip verified
        Ok(Ok(Err(e))) => panic!("round-trip failed: {e}"),
        Ok(Err(join)) => panic!("board task panicked: {join}"),
        Err(_) => panic!("timed out waiting for the req->resp round-trip"),
    }
}
