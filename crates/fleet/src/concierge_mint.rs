//! `concierge_mint` — the pure plan for minting a per-operator concierge thin instance (task_1259).
//!
//! cameron chose per-operator concierges (task_1259): one approved role doc (doc_3392, `roles/concierge`) and
//! N thin instances, each pinned to a single operator — not N bespoke agents. This module is the PURE core of
//! the mint: given an operator and their channels it produces the exact board rows the mint will create, so
//! the plan is reviewable and unit-tested before any live board mutation. The live apply (create the channel,
//! register the agent, write the person->handling-agent binding, launch the window) wires these shapes to the
//! board API and is staged behind cameron's operator-set confirm; the person->handling-agent binding-row
//! schema is owned by v-task-board and lands with the apply slice.
//!
//! Both routing surfaces read ONE projection of the single `metadata.operator` pin (task_1259 resolver):
//! - SLACK: the per-operator CHANNEL row's bridge metadata — the live membrain slack bridge keys on the
//!   channel and hot-reloads it from board channel metadata, so a new operator is a new row with no bridge
//!   code change (v-slack-bridge).
//! - BOARD: a durable person->handling-agent binding row (written at apply, read by `routed_to=operator`
//!   resolution + person-DM reachability) — v-task-board owns that row's schema.

use serde_json::{Value, json};

/// The deterministic board agent id for operator `op`'s concierge instance: `concierge-<op>`. Legible, and the
/// registry/window pin is obvious from the id. The `metadata.operator` pin (not this id) is the routing key.
pub fn concierge_agent_id(operator: &str) -> String {
    format!("concierge-{operator}")
}

/// The mint plan for one per-operator concierge: the agent id plus the two board-row projections of the single
/// `metadata.operator` pin — the per-operator CHANNEL row's bridge metadata, and the AGENT metadata pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConciergeMintPlan {
    /// `concierge-<operator>`.
    pub agent_id: String,
    /// Board CHANNEL metadata the membrain slack bridge reads for this operator's channel.
    pub channel_metadata: Value,
    /// Board AGENT metadata pins for this operator's concierge instance.
    pub agent_metadata: Value,
}

/// Build the mint plan for `operator` whose per-operator board channel is `operator_channel` and whose Slack
/// channel is `slack_channel`. Pure — no board I/O; the caller applies the plan. The channel metadata matches
/// the exact keys the live bridge consumes (v-slack-bridge, task_1259): `bridge_config.external_channel_id`
/// routes to the operator's Slack channel, `outbound_authors` gates outbound to this instance alone,
/// `full_inbound_threads` syncs in-thread replies, and `direction=both` bridges both ways.
pub fn concierge_mint_plan(
    operator: &str,
    operator_channel: &str,
    slack_channel: &str,
) -> ConciergeMintPlan {
    let agent_id = concierge_agent_id(operator);
    let channel_metadata = json!({
        "bridge_config": {
            "external_channel_id": slack_channel,
            "bridge_instance": "membrain",
            "source": "slack",
        },
        "outbound_authors": [agent_id],
        "full_inbound_threads": true,
        "direction": "both",
    });
    let agent_metadata = json!({
        "operator": operator,
        "operator_channel": operator_channel,
        "slack_channel": slack_channel,
    });
    ConciergeMintPlan {
        agent_id,
        channel_metadata,
        agent_metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_is_concierge_dash_operator() {
        assert_eq!(concierge_agent_id("markpod"), "concierge-markpod");
        assert_eq!(concierge_agent_id("cameron"), "concierge-cameron");
    }

    #[test]
    fn mint_plan_projects_the_pin_onto_the_channel_and_agent_rows() {
        let plan = concierge_mint_plan("markpod", "channel_412", "C0ABCDEF");
        assert_eq!(plan.agent_id, "concierge-markpod");
        // CHANNEL row (the slack-bridge contract): exact keys the live membrain bridge reads.
        assert_eq!(
            plan.channel_metadata["bridge_config"]["external_channel_id"],
            json!("C0ABCDEF")
        );
        assert_eq!(
            plan.channel_metadata["bridge_config"]["bridge_instance"],
            json!("membrain")
        );
        assert_eq!(
            plan.channel_metadata["bridge_config"]["source"],
            json!("slack")
        );
        // Outbound is gated to this instance alone (the per-operator outbound authz).
        assert_eq!(
            plan.channel_metadata["outbound_authors"],
            json!(["concierge-markpod"])
        );
        assert_eq!(plan.channel_metadata["full_inbound_threads"], json!(true));
        assert_eq!(plan.channel_metadata["direction"], json!("both"));
        // AGENT pins: metadata.operator is the routing lookup key + context boundary.
        assert_eq!(plan.agent_metadata["operator"], json!("markpod"));
        assert_eq!(
            plan.agent_metadata["operator_channel"],
            json!("channel_412")
        );
        assert_eq!(plan.agent_metadata["slack_channel"], json!("C0ABCDEF"));
    }
}
