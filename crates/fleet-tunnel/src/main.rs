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
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use fleet_tunnel::config::Config;
use fleet_tunnel::frame::{Frame, PROTOCOL_VERSION, decode_body, encode_body};
use fleet_tunnel::health::HealthState;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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

// Bound the health probe's upstream reachability check so a hung notifier can't wedge the probe.
const HEALTH_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);

// Reconnect if no board frame arrives for this many keepalive intervals. A healthy board sends a
// keepalive frame every `keepalive`s, so prolonged silence means a half-open/dead socket (an
// ungracefully-restarted or killed board that sent no close). Without this the read blocks for the
// OS TCP timeout (minutes) while we still look "connected" — silently dropped from the board's
// tunnel registry and missing wakes. Kept below the health probe's staleness threshold (3x) so the
// daemon self-heals before the probe reports unhealthy.
const IDLE_KEEPALIVE_MULT: u64 = 2;
// Floor so a small negotiated keepalive can't make the idle timeout hair-trigger.
const MIN_IDLE_TIMEOUT_SECS: u64 = 20;

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

    // Shared liveness state, updated as frames flow. The health probe (if configured) reads it
    // across reconnects, so a watchdog polling during a reconnect gap sees `disconnected`, not a
    // refused connection.
    let health = Arc::new(HealthState::new());
    if let Some(bind) = cfg.health_bind() {
        match bind.parse::<SocketAddr>() {
            Ok(addr) => {
                tokio::spawn(serve_health(
                    addr,
                    health.clone(),
                    cfg.upstream_trimmed().to_string(),
                ));
            }
            Err(e) => tracing::warn!("health probe disabled: invalid health_addr {bind:?}: {e}"),
        }
    }

    // Graceful shutdown: a supervisor (tmux keep-alive wrapper, or systemd) stops us with
    // SIGTERM/SIGINT. Break out of the reconnect loop so the socket closes cleanly and we exit 0.
    tokio::select! {
        _ = run_forever(cfg, health) => {}
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

async fn run_forever(cfg: Arc<Config>, health: Arc<HealthState>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let outcome = run_once(&cfg, &health).await;
        health.set_connected(false); // socket is down until the next handshake completes
        match outcome {
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

/// Resolve the served-agent set for this connection. When `agents_cmd` is configured, run it and use
/// its stdout (one id per line) — so a moved/new agent is picked up on the NEXT reconnect with no
/// static-list edit (the staleness that silently starves an agent of event-wakes). Fail-safe: a command
/// that errors, exits non-zero, or prints no ids falls back to the static `agents` list rather than
/// declaring an empty set (which would tear down every tunnel on this host).
async fn resolve_served_agents(cfg: &Config) -> Vec<String> {
    let Some(argv) = cfg.agents_cmd_argv() else {
        return cfg.agents.clone();
    };
    let (bin, rest) = argv
        .split_first()
        .expect("agents_cmd_argv is non-empty when Some");
    match tokio::process::Command::new(bin).args(rest).output().await {
        Ok(out) if out.status.success() => {
            let derived =
                fleet_tunnel::config::parse_agent_lines(&String::from_utf8_lossy(&out.stdout));
            if derived.is_empty() {
                tracing::warn!(
                    "agents_cmd `{}` produced no ids; falling back to the static agents list",
                    argv.join(" ")
                );
                cfg.agents.clone()
            } else {
                tracing::info!(
                    "derived {} served agents from `{}`",
                    derived.len(),
                    argv.join(" ")
                );
                derived
            }
        }
        Ok(out) => {
            tracing::warn!(
                "agents_cmd `{}` exited {}; falling back to the static agents list ({})",
                argv.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            cfg.agents.clone()
        }
        Err(e) => {
            tracing::warn!(
                "agents_cmd `{}` failed to run ({e}); falling back to the static agents list",
                argv.join(" ")
            );
            cfg.agents.clone()
        }
    }
}

/// One connection lifetime: dial, handshake, then serve frames until the socket closes.
async fn run_once(cfg: &Config, health: &Arc<HealthState>) -> Result<(), BoxError> {
    let request = build_request(cfg)?;
    let agents = resolve_served_agents(cfg).await;
    tracing::info!(
        "dialing board ws {} (host={} agents={:?})",
        cfg.board_ws,
        cfg.host_id_or_hostname(),
        agents
    );
    let (ws, _resp) = tokio_tungstenite::connect_async(request).await?;
    let (mut write, mut read) = ws.split();

    // hello
    let hello = Frame::Hello {
        v: PROTOCOL_VERSION,
        host: cfg.host_id_or_hostname(),
        agents,
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
    // Tunnel is live: mark connected, record the keepalive the freshness threshold scales off, and
    // stamp the hello_ok as the first board frame.
    health.set_keepalive(keepalive);
    health.set_connected(true);
    health.mark_board_frame();

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
    let result = serve(&mut read, &tx, &upstream, &agent, health, keepalive).await;

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
    health: &Arc<HealthState>,
    keepalive: u64,
) -> Result<(), BoxError>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    // Silence longer than this means a dead/half-open socket (see IDLE_KEEPALIVE_MULT) — reconnect
    // rather than block forever on a board that vanished without a close.
    let idle = Duration::from_secs(
        keepalive
            .saturating_mul(IDLE_KEEPALIVE_MULT)
            .max(MIN_IDLE_TIMEOUT_SECS),
    );
    loop {
        let msg = match tokio::time::timeout(idle, read.next()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break, // stream ended: clean close
            Err(_) => {
                // No frame (not even a keepalive) within the idle window: treat the socket as dead.
                // Returning Ok triggers a prompt re-dial + hello in run_forever, re-registering us
                // in the board's tunnel registry so wakes resume.
                tracing::warn!(
                    "no board frame for {}s ({}x keepalive); socket presumed dead, reconnecting",
                    idle.as_secs(),
                    IDLE_KEEPALIVE_MULT
                );
                return Ok(());
            }
        };
        // Any inbound frame proves the socket is alive; stamp it for the health probe's staleness
        // check before dispatching.
        health.mark_board_frame();
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

/// The liveness probe: a tiny loopback HTTP/1.1 server. Any request gets the current
/// [`HealthState`] snapshot as JSON — HTTP 200 when the wake path is live (`ok`), 503 otherwise —
/// so a watchdog can distinguish a working tunnel from a silently-wedged socket. Loopback-only;
/// it exposes no control surface. Runs for the process lifetime, independent of connection state.
async fn serve_health(addr: SocketAddr, health: Arc<HealthState>, upstream: String) {
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => {
            tracing::info!("health probe listening on http://{addr}/ (any path)");
            l
        }
        Err(e) => {
            tracing::warn!("health probe disabled: cannot bind {addr}: {e}");
            return;
        }
    };
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("health probe accept failed: {e}");
                continue;
            }
        };
        let health = health.clone();
        let upstream = upstream.clone();
        tokio::spawn(async move {
            // Drain the request (we don't route on path/method — any request returns health).
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let reachable = probe_upstream(&upstream).await;
            let snap = health.snapshot(reachable, &upstream);
            let body = snap.to_json();
            let status = if snap.ok() {
                "200 OK"
            } else {
                "503 Service Unavailable"
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}

/// Is the local upstream (the notifier) reachable right now? A bounded GET to its base URL: any
/// HTTP response (including a non-2xx status) proves reachability; only a transport/connect error
/// means unreachable — the same "reachable vs not" distinction [`forward`] draws.
async fn probe_upstream(upstream: &str) -> bool {
    let base = upstream.to_string();
    tokio::task::spawn_blocking(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout(HEALTH_UPSTREAM_TIMEOUT)
            .build();
        matches!(
            agent.get(&base).call(),
            Ok(_) | Err(ureq::Error::Status(_, _))
        )
    })
    .await
    .unwrap_or(false)
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
