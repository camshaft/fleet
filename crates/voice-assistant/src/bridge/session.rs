//! `VoiceBridge` — the async board session the audio loop drives (INBOUND + OUTBOUND over `bridge-core`).
//!
//! This is the thin async I/O shell over the pure planners: it owns the [`BoardClient`], the resolved
//! [`ChannelMap`], and the persisted firehose cursor, and exposes the two operations the audio loop needs:
//!   - [`VoiceBridge::post_transcript`] (INBOUND): a finalized STT transcript -> an attributed board post
//!     (`sender` = the bridge agent, `external_author` = `voice:<speaker>`), via `bridge_core::plan_inbound`.
//!   - [`VoiceBridge::poll_replies`] (OUTBOUND): poll the firehose and return the speakable replies on the
//!     voice channel (George's turn replies AND proactive posts), advancing + persisting the cursor.
//!
//! Async by design (operator directive #370: the shared core is tokio, not thread-per-blocking-call); the
//! caller provides the runtime. The audio loop (cpal capture, sherpa STT/TTS) runs off that runtime on
//! blocking threads, but board I/O is async here. The methods are `async fn` over the shared
//! [`bridge_core::BoardClient`]; no runtime is pulled by this module itself.

use std::path::PathBuf;

use bridge_core::{plan_inbound, BoardClient, ChannelLink, ChannelMap};

use super::cursor::{load_cursor, save_cursor};
use super::reply::{plan_replies, SpokenReply};

/// How many firehose events to pull per poll (matches slack-bridge's outbound loop).
const POLL_LIMIT: usize = 100;

/// The external-source discriminator for the voice transport (`external_author = "voice:<speaker>"`,
/// `external-links` rows with `source = "voice"`).
pub const SOURCE: &str = "voice";

/// An async board session for the voice bridge. Holds the client, the channel map, and the firehose cursor.
pub struct VoiceBridge {
    client: BoardClient,
    map: ChannelMap,
    /// The external voice channel id (e.g. `voice:green`) the speaker's transcripts post into.
    voice_channel: String,
    /// This bridge's own board agent id (the INBOUND `sender`; deliberately NOT an outbound author, so its
    /// posts don't reflect back out).
    bridge_agent: String,
    /// The speaker's external id — `external_author = "voice:<speaker>"`.
    speaker: String,
    /// Where the firehose cursor is persisted.
    state_dir: PathBuf,
    /// Last handled firehose seq; loaded on construction, advanced + persisted as replies are polled.
    cursor: i64,
}

impl VoiceBridge {
    /// Build the session from resolved config parts + the static channel links. Sync: the reqwest client
    /// needs no runtime to construct, and the cursor loads from disk. Board-registered links are merged in
    /// later via [`refresh_map`](Self::refresh_map) (async). The cursor is `-1` when there's no persisted
    /// one yet, so the caller can detect a first run and initialize at the firehose head.
    pub fn new(
        board_api: &str,
        voice_channel: String,
        bridge_agent: String,
        speaker: String,
        state_dir: PathBuf,
        static_links: &[ChannelLink],
    ) -> Self {
        let cursor = load_cursor(&state_dir).unwrap_or(-1);
        VoiceBridge {
            client: BoardClient::new(board_api),
            map: ChannelMap::from_links(static_links),
            voice_channel,
            bridge_agent,
            speaker,
            state_dir,
            cursor,
        }
    }

    /// True when no cursor has ever been persisted (first run) — the caller should
    /// [`initialize_cursor_at_head`](Self::initialize_cursor_at_head) so the backlog isn't replayed as speech.
    pub fn is_fresh(&self) -> bool {
        self.cursor < 0
    }

    /// Merge the board-registered `voice` channel links (best-effort) with the `static_links` given at
    /// construction and rebuild the map. Board links are the dynamic source of truth; static links are
    /// applied AFTER so an explicit local override wins (`ChannelMap` is last-wins). A board read error is
    /// fail-soft — keeps the current (static) map. Call periodically so a link registered while running is
    /// honored without a restart.
    pub async fn refresh_map(&mut self, static_links: &[ChannelLink]) {
        let mut links = match self.client.list_channel_links(SOURCE).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[voice-bridge] could not read board channel links ({e}); using static config");
                Vec::new()
            }
        };
        links.extend_from_slice(static_links);
        if !links.is_empty() {
            self.map = ChannelMap::from_links(&links);
        }
    }

    /// INBOUND: post a finalized transcript as an attributed board post. `Err` when the voice channel isn't
    /// mapped to a board channel (nothing to post into) or the board post fails.
    pub async fn post_transcript(&self, text: &str) -> Result<(), String> {
        let planned = plan_inbound(
            &self.voice_channel,
            SOURCE,
            &self.speaker,
            text,
            None,
            &self.bridge_agent,
            |ext| self.map.external_to_board(ext),
        );
        match planned {
            Some(p) => self.client.post_raw(p.board_channel_id, &p.body).await.map(|_| ()),
            None => Err(format!(
                "voice channel {:?} is not mapped to a board channel",
                self.voice_channel
            )),
        }
    }

    /// OUTBOUND: poll the firehose once and return the speakable replies on mapped voice channels, then
    /// advance and persist the cursor past everything in the batch. The cursor advances on poll (not after a
    /// successful speak): a voice reply is ephemeral, so a crash mid-speak drops at most the current reply
    /// instead of risking a re-speak loop — acceptable for a voice assistant (the user can just ask again).
    pub async fn poll_replies(&mut self) -> Result<Vec<SpokenReply>, String> {
        let events = self.client.poll_events(self.cursor.max(0), POLL_LIMIT).await?;
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let (replies, new_cursor) = plan_replies(&events, self.cursor.max(0), |bid| {
            self.map.board_to_external(bid)
        });
        if new_cursor > self.cursor {
            self.cursor = new_cursor;
            save_cursor(&self.state_dir, self.cursor);
        }
        Ok(replies)
    }

    /// First-run cursor init: walk the firehose to its current head WITHOUT emitting speech, so the bridge
    /// doesn't replay the whole board backlog on first boot. Persists the head. On a poll error it leaves
    /// the cursor at 0 (start-from-now-ish) rather than looping.
    pub async fn initialize_cursor_at_head(&mut self) {
        let mut since = 0i64;
        loop {
            match self.client.poll_events(since, POLL_LIMIT).await {
                Ok(evs) if evs.is_empty() => break,
                // since_seq is exclusive, so a non-empty page always has a max > since -> terminates.
                Ok(evs) => since = evs.iter().map(|e| e.seq).max().unwrap_or(since),
                Err(e) => {
                    eprintln!("[voice-bridge] cursor init poll failed ({e}); starting at {since}");
                    break;
                }
            }
        }
        self.cursor = since;
        save_cursor(&self.state_dir, self.cursor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("voice-session-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn link(board: i64, external: &str) -> ChannelLink {
        ChannelLink {
            board_channel_id: board,
            external_channel: external.to_string(),
        }
    }

    #[test]
    fn fresh_when_no_cursor_persisted() {
        let d = tmp_dir("fresh");
        let vb = VoiceBridge::new(
            "http://127.0.0.1:8079/api",
            "voice:green".into(),
            "voice-bridge".into(),
            "cameron".into(),
            d,
            &[link(7, "voice:green")],
        );
        assert!(vb.is_fresh(), "no persisted cursor -> fresh");
    }

    #[test]
    fn not_fresh_when_cursor_exists() {
        let d = tmp_dir("existing");
        save_cursor(&d, 100);
        let vb = VoiceBridge::new(
            "http://127.0.0.1:8079/api",
            "voice:green".into(),
            "voice-bridge".into(),
            "cameron".into(),
            d,
            &[],
        );
        assert!(!vb.is_fresh(), "a persisted cursor -> not fresh");
    }
}
