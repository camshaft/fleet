//! `resolver` — the board↔Slack channel MAP, built from the TOML config.
//!
//! [`crate::sync::plan_outbound`] / [`crate::sync::plan_inbound`] take the channel mapping as an injected
//! resolver so the adapter is decoupled from where the map comes from. This module provides the concrete
//! map the daemon uses TODAY: a list of links in the TOML config (mandate #159 — config is TOML, no env).
//! Each link pairs a board channel id with a Slack channel id; the map resolves both directions.
//!
//! When board-core #149 slice 2 lands its generic external-link table + read API, a board-backed resolver
//! can replace (or back-fill) this config-driven one behind the same two lookups — the transport just
//! swaps which `ChannelMap` (or closure) it hands to the sync planner. Keeping the map behind these lookups
//! is also what lets a second external-source adapter (GitHub, #136) reuse the sync planner unchanged.

use serde::Deserialize;
use std::collections::HashMap;

/// One board↔Slack channel link (a row of the TOML `[[channel_map]]` array).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelLink {
    /// The board channel id.
    pub board_channel_id: i64,
    /// The Slack channel id (e.g. `C0123ABCD`).
    pub slack_channel: String,
}

/// A bidirectional board↔Slack channel map. Built once from the config links; cheap O(1) lookups both ways.
#[derive(Debug, Clone, Default)]
pub struct ChannelMap {
    board_to_slack: HashMap<i64, String>,
    slack_to_board: HashMap<String, i64>,
}

impl ChannelMap {
    /// Build the map from the configured links. On a duplicate key in either direction the LAST link wins
    /// (a later config line overrides an earlier one) — deterministic and order-defined, so a copy-paste
    /// dup doesn't silently fan a channel two ways.
    pub fn from_links(links: &[ChannelLink]) -> ChannelMap {
        let mut board_to_slack = HashMap::with_capacity(links.len());
        let mut slack_to_board = HashMap::with_capacity(links.len());
        for link in links {
            board_to_slack.insert(link.board_channel_id, link.slack_channel.clone());
            slack_to_board.insert(link.slack_channel.clone(), link.board_channel_id);
        }
        ChannelMap {
            board_to_slack,
            slack_to_board,
        }
    }

    /// The Slack channel a board channel maps to (OUT direction), or `None` if unmapped.
    pub fn board_to_slack(&self, board_channel_id: i64) -> Option<String> {
        self.board_to_slack.get(&board_channel_id).cloned()
    }

    /// The board channel a Slack channel maps to (IN direction), or `None` if unmapped.
    pub fn slack_to_board(&self, slack_channel: &str) -> Option<i64> {
        self.slack_to_board.get(slack_channel).copied()
    }

    /// Number of board→Slack links (distinct board channel ids).
    pub fn len(&self) -> usize {
        self.board_to_slack.len()
    }

    /// Whether the map has no links — i.e. nothing to mirror in either direction (a valid, dormant state).
    pub fn is_empty(&self) -> bool {
        self.board_to_slack.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(board: i64, slack: &str) -> ChannelLink {
        ChannelLink {
            board_channel_id: board,
            slack_channel: slack.to_string(),
        }
    }

    #[test]
    fn resolves_both_directions() {
        let m = ChannelMap::from_links(&[link(7, "C7"), link(8, "C8")]);
        assert_eq!(m.board_to_slack(7).as_deref(), Some("C7"));
        assert_eq!(m.board_to_slack(8).as_deref(), Some("C8"));
        assert_eq!(m.slack_to_board("C7"), Some(7));
        assert_eq!(m.slack_to_board("C8"), Some(8));
        assert_eq!(m.len(), 2);
        assert!(!m.is_empty());
    }

    #[test]
    fn unmapped_is_none() {
        let m = ChannelMap::from_links(&[link(7, "C7")]);
        assert!(m.board_to_slack(99).is_none());
        assert!(m.slack_to_board("Cnope").is_none());
    }

    #[test]
    fn empty_map_is_dormant() {
        let m = ChannelMap::from_links(&[]);
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert!(m.board_to_slack(1).is_none());
        assert!(m.slack_to_board("C1").is_none());
    }

    #[test]
    fn last_link_wins_on_duplicate_key() {
        // A later config line overrides an earlier one, in both directions.
        let m = ChannelMap::from_links(&[link(7, "C7"), link(7, "C7b")]);
        assert_eq!(m.board_to_slack(7).as_deref(), Some("C7b"), "last board link wins");
        assert_eq!(m.slack_to_board("C7b"), Some(7));

        let m2 = ChannelMap::from_links(&[link(1, "Cdup"), link(2, "Cdup")]);
        assert_eq!(m2.slack_to_board("Cdup"), Some(2), "last slack link wins");
    }

    #[test]
    fn feeds_sync_planner_closures() {
        // The map is used as the resolver closures the sync planner takes — pin that shape.
        let m = ChannelMap::from_links(&[link(7, "C7")]);
        let out_resolve = |cid: i64| m.board_to_slack(cid);
        let in_resolve = |ch: &str| m.slack_to_board(ch);
        assert_eq!(out_resolve(7).as_deref(), Some("C7"));
        assert_eq!(in_resolve("C7"), Some(7));
    }
}
