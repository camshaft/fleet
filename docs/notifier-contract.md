# The fleet notifier (`fleet notify`) — event-wake contract

`fleet notify` is the fleet-host **event-wake injector**: a small HTTP listener that turns the
coordination board's inbox-event webhooks into a `tmux send-keys` nudge in the recipient agent's
window, so a board-native agent reacts to a new task/message immediately instead of waiting for its
next `/loop` poll.

It is the local upstream that the reverse tunnel (`camshaft/dotfiles` `fleet-tunnel`) forwards board
requests to: the board POSTs each agent's webhook, the tunnel carries it over an outbound-dialed
websocket to the fleet host, and forwards it to this listener. The fleet host never needs an inbound
port.

## Listener

- **Address**: `$FLEET_NOTIFY_ADDR`, else `127.0.0.1:8899` (loopback-only — the tunnel forwards to
  loopback, so the listener never needs a public bind).
- **Run**: `fleet notify [--addr 127.0.0.1:8899]`. Blocks until killed; each connection is served on
  its own thread so one slow client never stalls a wake. Supervise it as a tmux window on the fleet
  host (see below).

## Request

- **Method**: `POST`. Any other method → `405`.
- **Path**: ignored (canonical `/wake`) — the daemon may forward whatever path the board's
  `webhook_url` carries; no rewrite needed.
- **Body**: the board's existing inbox-event webhook payload, verbatim — the JSON the board's `emit()`
  already POSTs to each agent's `webhook_url`. No new "wake-req" frame shape. Fields consumed:

  | field       | use                                                        |
  |-------------|------------------------------------------------------------|
  | `recipient` | **required** — the agent to wake; must be a valid agent name (demux key) |
  | `type`      | event type, sanitized into the wake line (e.g. `task.assigned`, `message.direct`) |
  | `task_id`   | preferred pointer in the wake line when present            |
  | `event_seq` | fallback pointer (messages / non-task events)              |

  Other fields (`actor`, `project_id`, `channel_id`, `document_id`, `data`, `created_at`) are accepted
  and ignored.

## Behavior

The listener demuxes by `recipient` → looks up that agent's tmux window in `$FLEET_SESSION` (default
`main`) → injects a one-line wake via `tmux send-keys`:

```
[notification] task #<task_id> (<type>) — check_notifications
[notification] message #<event_seq> (<type>) — check_notifications
[notification] <type> — check_notifications        # neither id present
```

`type` is sanitized (ascii alnum + `.`/`_`/`-`, capped) before it reaches the pane — the only
untrusted string that could reach `send-keys`; ids are integers and `recipient` is name-validated.

## Responses (all best-effort)

| status | meaning |
|--------|---------|
| `200 {"status":"woken","agent":"<r>"}`    | recipient has a live window; wake injected |
| `202 {"status":"accepted","agent":"<r>"}` | no live window; agent picks the event up on its next `/loop` poll (not an error) |
| `400 {"error":...}` | malformed request, invalid JSON, or missing/invalid `recipient` |
| `405 {"error":"POST only"}` | non-POST method |

A missed or failed POST is harmless: the board's `check_notifications` poll is the fallback path, so
the tunnel/board may treat any non-2xx as retryable/ignorable.

## Supervision (fleet host)

There is no systemd/launchd for the fleet — windows live in the `$FLEET_SESSION` tmux session. Run the
notifier as a dedicated window under a keep-alive wrapper:

```sh
tmux new-window -t main: -n fleet-notify \
  'while true; do fleet notify; echo "[fleet-notify] exited, restart in 2s"; sleep 2; done'
```

The reverse-tunnel daemon (`fleet-tunnel`) is supervised the same way, with
`FLEET_TUNNEL_UPSTREAM=http://127.0.0.1:8899` pointing at this listener.
