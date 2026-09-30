//! `channel_config` — the generic per-channel bridge config read from board channel metadata.
//!
//! A bridge discovers the channels it manages by reading `metadata.bridge_config` off each board channel
//! ([`crate::board::BoardChannel`]), filtered by its own `source` + `bridge_instance`. This is the single
//! shared config path for every transport (Slack, voice, …), so they resolve channels identically (the
//! #316 convergence). The board-metadata source means a channel wired/re-wired to a bridge is picked up
//! live, without a restart.
//!
//! Authorization is deliberately NOT here: outbound authors are board-side #150 authz, which the board reads
//! at TOP-LEVEL `metadata.outbound_authors` (the existence of a `channel.outbound_reflect` event IS the
//! authorization). So `bridge_config` carries only what a daemon needs to build its channel map — `source`,
//! `external_channel_id`, `bridge_instance` — keeping authz single-sourced (top-level, board-owned) rather
//! than duplicated in two places that could drift.

use crate::board::BoardChannel;
use crate::resolver::{ChannelLink, ChannelMap};
use serde::Deserialize;
use serde_json::Value;

/// The generic per-channel bridge config stored under `metadata.bridge_config` on a board channel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BridgeConfig {
    /// The external source discriminator (e.g. `"slack"`, `"voice"`).
    pub source: String,
    /// The external channel id to sync this board channel with (a Slack channel/DM id, a voice session id).
    pub external_channel_id: String,
    /// Which daemon instance manages this channel (e.g. `"membrain"` vs `"operator-dm"`).
    pub bridge_instance: String,
}

/// A board channel wired to a bridge: its board id + the resolved [`BridgeConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bridged {
    /// The board channel id.
    pub board_channel_id: i64,
    /// The channel's `bridge_config`.
    pub config: BridgeConfig,
}

/// Coerce a channel's `metadata` to a JSON object: accept an object, or a JSON-encoded string (`"{...}"`);
/// anything else becomes `Null`. Pure. (The board stores metadata as either shape depending on the writer.)
fn as_object(meta: &Value) -> Value {
    match meta {
        Value::Object(_) => meta.clone(),
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// Filter board channels to those wired to (`source`, `bridge_instance`) via their `metadata.bridge_config`.
/// A channel is kept iff it has a well-formed `bridge_config` matching BOTH `source` and `bridge_instance`.
/// Malformed/absent config on one channel is skipped (fail-soft — never drops the others). Pure.
pub fn bridged_channels(
    channels: &[BoardChannel],
    source: &str,
    bridge_instance: &str,
) -> Vec<Bridged> {
    let mut out = Vec::new();
    for ch in channels {
        let meta = as_object(&ch.metadata);
        let Some(cfg_val) = meta.get("bridge_config") else {
            continue;
        };
        let cfg: BridgeConfig = match serde_json::from_value(cfg_val.clone()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if cfg.source == source && cfg.bridge_instance == bridge_instance {
            out.push(Bridged {
                board_channel_id: ch.id,
                config: cfg,
            });
        }
    }
    out
}

/// Build the board↔external [`ChannelMap`] (the resolver the sync planners take) from bridged channels. Pure.
pub fn channel_map(bridged: &[Bridged]) -> ChannelMap {
    let links: Vec<ChannelLink> = bridged
        .iter()
        .map(|b| ChannelLink {
            board_channel_id: b.board_channel_id,
            external_channel: b.config.external_channel_id.clone(),
        })
        .collect();
    ChannelMap::from_links(&links)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chan(id: i64, metadata: Value) -> BoardChannel {
        BoardChannel {
            id,
            name: format!("c{id}"),
            metadata,
        }
    }

    #[test]
    fn keeps_only_matching_source_and_instance() {
        let channels = vec![
            chan(
                30,
                json!({"bridge_config": {"source": "slack", "external_channel_id": "C30", "bridge_instance": "membrain"}}),
            ),
            chan(
                31,
                json!({"bridge_config": {"source": "slack", "external_channel_id": "C31", "bridge_instance": "operator-dm"}}),
            ),
            chan(
                32,
                json!({"bridge_config": {"source": "voice", "external_channel_id": "V1", "bridge_instance": "membrain"}}),
            ),
        ];
        let b = bridged_channels(&channels, "slack", "membrain");
        assert_eq!(b.len(), 1, "only the slack+membrain channel matches");
        assert_eq!(b[0].board_channel_id, 30);
        assert_eq!(b[0].config.external_channel_id, "C30");
    }

    #[test]
    fn accepts_string_encoded_metadata() {
        // The board sometimes stores metadata as a JSON-encoded string.
        let channels = vec![chan(
            7,
            json!(
                "{\"bridge_config\": {\"source\": \"slack\", \"external_channel_id\": \"C7\", \"bridge_instance\": \"membrain\"}}"
            ),
        )];
        let b = bridged_channels(&channels, "slack", "membrain");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].config.external_channel_id, "C7");
    }

    #[test]
    fn skips_absent_or_malformed_config_but_keeps_others() {
        let channels = vec![
            chan(1, json!({})),                                     // no bridge_config
            chan(2, json!({"bridge_config": {"source": "slack"}})), // malformed (missing fields)
            chan(3, Value::Null),                                   // no metadata
            chan(
                4,
                json!({"bridge_config": {"source": "slack", "external_channel_id": "C4", "bridge_instance": "membrain"}}),
            ),
        ];
        let b = bridged_channels(&channels, "slack", "membrain");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].board_channel_id, 4);
    }

    #[test]
    fn different_source_or_instance_excluded() {
        let channels = vec![chan(
            9,
            json!({"bridge_config": {"source": "voice", "external_channel_id": "V", "bridge_instance": "membrain"}}),
        )];
        assert!(
            bridged_channels(&channels, "slack", "membrain").is_empty(),
            "wrong source excluded"
        );
        let channels2 = vec![chan(
            9,
            json!({"bridge_config": {"source": "slack", "external_channel_id": "C", "bridge_instance": "other"}}),
        )];
        assert!(
            bridged_channels(&channels2, "slack", "membrain").is_empty(),
            "wrong instance excluded"
        );
    }

    #[test]
    fn builds_bidirectional_map() {
        let b = vec![Bridged {
            board_channel_id: 30,
            config: BridgeConfig {
                source: "slack".into(),
                external_channel_id: "C30".into(),
                bridge_instance: "membrain".into(),
            },
        }];
        let m = channel_map(&b);
        assert_eq!(m.board_to_external(30).as_deref(), Some("C30"));
        assert_eq!(m.external_to_board("C30"), Some(30));
    }
}
