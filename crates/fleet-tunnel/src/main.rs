//! fleet-tunnel daemon — the async transport shell (built only with `--features transport`).
//!
//! Dials OUT to the board websocket, performs the hello/hello_ok handshake (declaring host id +
//! served agent set), runs an app-level heartbeat, and forwards each board `req` frame to a single
//! configured local upstream (the notifier), returning `resp`/`err` over the socket. Reconnects with
//! exponential backoff + jitter; shuts down cleanly on SIGTERM/SIGINT. Not an open proxy — forwards
//! to exactly the one configured upstream. Ported 1:1 from the original Python daemon.

use std::collections::BTreeMap;
use std::error::Error;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use fleet_tunnel::config::Config;
use fleet_tunnel::frame::{Frame, PROTOCOL_VERSION, decode_body, encode_body};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};

type BoxError = Box<dyn Error + Send + Sync>;

// Reconnect backoff: dialing a flapping board must never hot-loop.
const BACKOFF_MIN: f64 = 0.5;
const BACKOFF_MAX: f64 = 30.0;
const BACKOFF_FACTOR: f64 = 2.0;

// Bound each forwarded upstream call so a hung notifier can't wedge the socket.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

// Fallback heartbeat if the board doesn't advertise one in hello_ok.
const DEFAULT_KEEPALIVE: u64 = 30;

// Cap a forwarded response body so a misbehaving upstream can't exhaust memory.
const MAX_RESP_BODY: u64 = 16 * 1024 * 1024;

#[derive(Parser)]
#[command(about = "fleet-tunnel reverse HTTP-over-websocket bridge (fleet-host daemon)")]
struct Args {
    /// Path to the TOML config file (the ONLY thing chosen outside the file; mandate #159).
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let cfg = match Config::from_toml_path(&args.config) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!("config: {e}");
            return ExitCode::from(2);
        }
    };

    // Graceful shutdown: a supervisor (tmux keep-alive wrapper, or systemd) stops us with
    // SIGTERM/SIGINT. Break out of the reconnect loop so the socket closes cleanly and we exit 0.
    tokio::select! {
        _ = run_forever(cfg) => {}
        _ = shutdown_signal() => tracing::info!("signal received; shutting down"),
    }
    tracing::info!("stopped");
    ExitCode::SUCCESS
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut intr = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = intr.recv() => {}
    }
}

async fn run_forever(cfg: Arc<Config>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        match run_once(&cfg).await {
            Ok(()) => {
                backoff = BACKOFF_MIN;
                tracing::info!("board closed the tunnel; reconnecting");
            }
            Err(e) => tracing::warn!("tunnel connection failed: {e}"),
        }
        let sleep = backoff.min(BACKOFF_MAX) * (0.5 + jitter());
        tracing::info!("reconnecting in {sleep:.1}s");
        tokio::time::sleep(Duration::from_secs_f64(sleep)).await;
        backoff = (backoff * BACKOFF_FACTOR).min(BACKOFF_MAX);
    }
}

/// Cheap [0,1) jitter from the clock's sub-second nanos — avoids an rng dependency.
fn jitter() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos as f64 / (u32::MAX as f64 + 1.0)
}

/// One connection lifetime: dial, handshake, then serve frames until the socket closes.
async fn run_once(cfg: &Config) -> Result<(), BoxError> {
    let request = build_request(cfg)?;
    tracing::info!(
        "dialing board ws {} (host={} agents={:?})",
        cfg.board_ws,
        cfg.host_id_or_hostname(),
        cfg.agents
    );
    let (ws, _resp) = tokio_tungstenite::connect_async(request).await?;
    let (mut write, mut read) = ws.split();

    // hello
    let hello = Frame::Hello {
        v: PROTOCOL_VERSION,
        host: cfg.host_id_or_hostname(),
        agents: cfg.agents.clone(),
        token: cfg.token.clone(),
    };
    write.send(Message::Text(hello.to_json().into())).await?;

    // await hello_ok (answering any interim WS pings)
    let keepalive = loop {
        match read.next().await {
            Some(Ok(Message::Text(t))) => match Frame::from_json(t.as_str()) {
                Ok(Frame::HelloOk { keepalive }) => break keepalive.unwrap_or(DEFAULT_KEEPALIVE),
                Ok(other) => return Err(format!("expected hello_ok, got {other:?}").into()),
                Err(e) => return Err(format!("bad hello_ok frame: {e}").into()),
            },
            Some(Ok(Message::Ping(p))) => write.send(Message::Pong(p)).await?,
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
            None => return Err("board closed before hello_ok".into()),
        }
    };
    tracing::info!("tunnel up (keepalive={keepalive}s); serving board requests");

    // A single writer task owns the sink; heartbeat, req responses, and pongs push Messages to it
    // over an mpsc, so nothing has to lock the sink.
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write.send(msg).await.is_err() {
                break;
            }
        }
    });

    let hb_tx = tx.clone();
    let heartbeat = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(keepalive));
        ticker.tick().await; // the first tick fires immediately; skip it
        loop {
            ticker.tick().await;
            if hb_tx
                .send(Message::Text(Frame::Ping.to_json().into()))
                .is_err()
            {
                break;
            }
        }
    });

    let agent = ureq::AgentBuilder::new().timeout(UPSTREAM_TIMEOUT).build();
    let upstream = cfg.upstream_trimmed().to_string();
    let result = serve(&mut read, &tx, &upstream, &agent).await;

    heartbeat.abort();
    drop(tx); // let the writer drain + finish
    let _ = writer.await;
    result
}

async fn serve<S>(
    read: &mut S,
    tx: &mpsc::UnboundedSender<Message>,
    upstream: &str,
    agent: &ureq::Agent,
) -> Result<(), BoxError>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = read.next().await {
        match msg? {
            Message::Text(t) => match Frame::from_json(t.as_str()) {
                Ok(Frame::Req {
                    id,
                    method,
                    path,
                    headers,
                    body,
                }) => {
                    // Forward concurrently; responses may complete out of id order.
                    let tx = tx.clone();
                    let agent = agent.clone();
                    let upstream = upstream.to_string();
                    tokio::spawn(async move {
                        let resp =
                            forward(&agent, &upstream, id, method, path, headers, body).await;
                        let _ = tx.send(Message::Text(resp.to_json().into()));
                    });
                }
                Ok(Frame::Ping) => {
                    let _ = tx.send(Message::Text(Frame::Pong.to_json().into()));
                }
                Ok(Frame::Pong) => {}
                Ok(other) => tracing::warn!("ignoring unexpected frame: {other:?}"),
                Err(e) => tracing::warn!("dropping non-frame text: {e}"),
            },
            Message::Ping(p) => {
                let _ = tx.send(Message::Pong(p));
            }
            Message::Close(_) => {
                tracing::info!("board sent close");
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Forward one `req` to the local upstream and build the `resp`/`err` frame. Tunnels the HTTP
/// faithfully (no payload interpretation — the notifier demuxes) and forwards to exactly the one
/// configured upstream: not an open proxy.
// `ureq::Error` is a large enum, but it's ureq's type and we match it directly (incl. the
// `Status(_, resp)` arm), so boxing would just add churn without value.
#[allow(clippy::result_large_err)]
async fn forward(
    agent: &ureq::Agent,
    upstream: &str,
    id: i64,
    method: Option<String>,
    path: Option<String>,
    headers: BTreeMap<String, String>,
    body: Option<String>,
) -> Frame {
    let method = method.unwrap_or_else(|| "POST".into()).to_uppercase();
    let path = path.unwrap_or_else(|| "/".into());
    let url = if path.starts_with('/') {
        format!("{upstream}{path}")
    } else {
        format!("{upstream}/{path}")
    };
    let body = match decode_body(&body) {
        Ok(b) => b,
        Err(e) => {
            return Frame::Err {
                id,
                code: "bad_body".into(),
                msg: e.to_string(),
            };
        }
    };

    let agent = agent.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut req = agent.request(&method, &url);
        for (k, v) in &headers {
            // Drop hop-by-hop / length headers the client recomputes itself.
            let kl = k.to_ascii_lowercase();
            if matches!(
                kl.as_str(),
                "host" | "content-length" | "connection" | "transfer-encoding"
            ) {
                continue;
            }
            req = req.set(k, v);
        }
        req.send_bytes(&body)
    })
    .await;

    match outcome {
        Err(join) => Frame::Err {
            id,
            code: "internal".into(),
            msg: join.to_string(),
        },
        // A non-2xx status is still a response the board asked us to relay, not a tunnel error.
        Ok(Ok(resp)) | Ok(Err(ureq::Error::Status(_, resp))) => resp_to_frame(id, resp),
        Ok(Err(e)) => Frame::Err {
            id,
            code: "upstream_unreachable".into(),
            msg: e.to_string(),
        },
    }
}

fn resp_to_frame(id: i64, resp: ureq::Response) -> Frame {
    let status = resp.status();
    let mut headers = BTreeMap::new();
    for name in resp.headers_names() {
        if let Some(v) = resp.header(&name) {
            headers.insert(name, v.to_string());
        }
    }
    let mut buf = Vec::new();
    let _ = resp.into_reader().take(MAX_RESP_BODY).read_to_end(&mut buf);
    Frame::Resp {
        id,
        status,
        headers,
        body: encode_body(&buf),
    }
}

/// Build the WS handshake request, adding the Cloudflare Access service-token headers for the
/// off-LAN public-gateway dial when configured.
fn build_request(
    cfg: &Config,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, BoxError> {
    let mut request = cfg.board_ws.as_str().into_client_request()?;
    if let Some((id, secret)) = cfg.cf_credentials() {
        let headers = request.headers_mut();
        headers.insert(
            HeaderName::from_static("cf-access-client-id"),
            HeaderValue::from_str(&id)?,
        );
        headers.insert(
            HeaderName::from_static("cf-access-client-secret"),
            HeaderValue::from_str(&secret)?,
        );
    }
    Ok(request)
}
