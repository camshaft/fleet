//! `resolver` — the board↔external channel MAP the sync planners take.
//!
//! [`crate::sync::plan_outbound`] / [`crate::sync::plan_inbound`] take the channel mapping as an injected
//! resolver closure, so a bridge is decoupled from where the map comes from — a static config list, the
//! board's `external-links` table, or per-channel board metadata. This module is the concrete map: a set
//! of links, each pairing a board channel id with an external channel id (a Slack channel/DM, a voice
//! session, …); it resolves both directions in O(1).
//!
//! The external side is an opaque `String`, so the SAME map serves every transport. Keeping the map behind
//! these two lookups is what lets any external-source bridge reuse the sync planner unchanged.

use serde::Deserialize;
use std::collections::HashMap;

/// One board↔external channel link. Deserialized from a transport's config (e.g. a TOML `[[channel_map]]`
/// row) or built from the board's link/metadata tables. `external_channel` accepts the legacy `slack_channel`
/// key as an alias so pre-genericization Slack configs (and the deployed agenix secret) keep parsing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelLink {
    /// The board channel id.
    pub board_channel_id: i64,
    /// The external channel id (e.g. a Slack channel/DM id `C0123ABCD` / `D0…`, a voice session id).
    #[serde(alias = "slack_channel")]
    pub external_channel: String,
}

/// A bidirectional board↔external channel map. Built once from the links; cheap O(1) lookups both ways.
#[derive(Debug, Clone, Default)]
pub struct ChannelMap {
    board_to_external: HashMap<i64, String>,
    external_to_board: HashMap<String, i64>,
}

impl ChannelMap {
    /// Build the map from the links. On a duplicate key in either direction the LAST link wins (a later
    /// entry overrides an earlier one) — deterministic and order-defined, so a copy-paste dup doesn't
    /// silently fan a channel two ways.
    pub fn from_links(links: &[ChannelLink]) -> ChannelMap {
        let mut board_to_external = HashMap::with_capacity(links.len());
        let mut external_to_board = HashMap::with_capacity(links.len());
        for link in links {
            board_to_external.insert(link.board_channel_id, link.external_channel.clone());
            external_to_board.insert(link.external_channel.clone(), link.board_channel_id);
        }
        ChannelMap {
            board_to_external,
            external_to_board,
        }
    }

    /// The external channel a board channel maps to (OUT direction), or `None` if unmapped.
    pub fn board_to_external(&self, board_channel_id: i64) -> Option<String> {
        self.board_to_external.get(&board_channel_id).cloned()
    }

    /// The board channel an external channel maps to (IN direction), or `None` if unmapped.
    pub fn external_to_board(&self, external_channel: &str) -> Option<i64> {
        self.external_to_board.get(external_channel).copied()
    }

    /// Number of board→external links (distinct board channel ids).
    pub fn len(&self) -> usize {
        self.board_to_external.len()
    }

    /// Whether the map has no links — i.e. nothing to mirror in either direction (a valid, dormant state).
    pub fn is_empty(&self) -> bool {
        self.board_to_external.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(board: i64, external: &str) -> ChannelLink {
        ChannelLink {
            board_channel_id: board,
            external_channel: external.to_string(),
        }
    }

    #[test]
    fn resolves_both_directions() {
        let m = ChannelMap::from_links(&[link(7, "C7"), link(8, "C8")]);
        assert_eq!(m.board_to_external(7).as_deref(), Some("C7"));
        assert_eq!(m.board_to_external(8).as_deref(), Some("C8"));
        assert_eq!(m.external_to_board("C7"), Some(7));
        assert_eq!(m.external_to_board("C8"), Some(8));
        assert_eq!(m.len(), 2);
        assert!(!m.is_empty());
    }

    #[test]
    fn unmapped_is_none() {
        let m = ChannelMap::from_links(&[link(7, "C7")]);
        assert!(m.board_to_external(99).is_none());
        assert!(m.external_to_board("Cnope").is_none());
    }

    #[test]
    fn empty_map_is_dormant() {
        let m = ChannelMap::from_links(&[]);
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert!(m.board_to_external(1).is_none());
        assert!(m.external_to_board("C1").is_none());
    }

    #[test]
    fn last_link_wins_on_duplicate_key() {
        // A later entry overrides an earlier one, in both directions.
        let m = ChannelMap::from_links(&[link(7, "C7"), link(7, "C7b")]);
        assert_eq!(m.board_to_external(7).as_deref(), Some("C7b"), "last board link wins");
        assert_eq!(m.external_to_board("C7b"), Some(7));

        let m2 = ChannelMap::from_links(&[link(1, "Cdup"), link(2, "Cdup")]);
        assert_eq!(m2.external_to_board("Cdup"), Some(2), "last external link wins");
    }

    #[test]
    fn feeds_sync_planner_closures() {
        // The map is used as the resolver closures the sync planner takes — pin that shape.
        let m = ChannelMap::from_links(&[link(7, "C7")]);
        let out_resolve = |cid: i64| m.board_to_external(cid);
        let in_resolve = |ch: &str| m.external_to_board(ch);
        assert_eq!(out_resolve(7).as_deref(), Some("C7"));
        assert_eq!(in_resolve("C7"), Some(7));
    }

    #[test]
    fn deserializes_legacy_slack_channel_key_via_alias() {
        // The deployed Slack config (agenix secret) uses `slack_channel`; the genericized field accepts it
        // via #[serde(alias)] so the cutover to bridge-core doesn't break the live daemon's config.
        let link: ChannelLink =
            serde_json::from_str(r#"{"board_channel_id": 30, "slack_channel": "D0BDKL68Z46"}"#).unwrap();
        assert_eq!(link.board_channel_id, 30);
        assert_eq!(link.external_channel, "D0BDKL68Z46");
        // The genericized key also works.
        let link2: ChannelLink =
            serde_json::from_str(r#"{"board_channel_id": 7, "external_channel": "C7"}"#).unwrap();
        assert_eq!(link2.external_channel, "C7");
    }
}
