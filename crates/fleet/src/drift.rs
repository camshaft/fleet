//! `drift` — Stage 1 of the active charter-drift correction mechanism (approved doc_3410, task_1325).
//!
//! This module is the PURE core of the top-of-tick charter self-check. It classifies a drift SIGNAL and
//! advances a per-agent drift-state counter that graduates a BEHAVIORAL-drift flag from an injected
//! return-to-charter directive to a hard stop-and-return once the same drift persists past N ticks
//! (default N=2). It performs NO I/O: the board query that yields the actionable-task count and the agent
//! presence, the persistence of [`DriftState`], and the kickoff-directive injection are wired at the edge by
//! a later slice. Keeping the logic pure makes every rule unit-tested here.
//!
//! Scope (Stage 1): the only live signal is [`DriftClass::SwitchDoNotIdle`] — an agent idle while holding
//! actionable, non-blocked assigned work (the task_736 condition), machine-checkable now from the board's
//! assignee task list via [`crate::board::open_task_count`] (which filters with `task_is_actionable`). The
//! behavioral-off-charter and charter-drift classes (doc_3410 Appendix taxonomy) extend [`DriftClass`] once
//! their ground truth (task_1117) and detector (task_521) land. Escalation here is behavioral-drift only, so
//! the hard stop never fires on a stale-charter case (doc_3410 goals 6 and 7).
#![allow(dead_code)] // slice 1: the pure core; the board/kickoff edge wiring lands in a later slice (task_1325).

/// A classified drift signal. Stage 1 carries only the switch-do-not-idle behavioral condition; the
/// behavioral-off-charter and charter-drift classes (doc_3410) are added when their ground truth and detector
/// land. [`is_behavioral`](DriftClass::is_behavioral) gates whether a class may escalate to a hard stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftClass {
    /// task_736: the agent is idle while holding actionable, non-blocked assigned work it should switch to.
    SwitchDoNotIdle,
}

impl DriftClass {
    /// Whether this class is BEHAVIORAL drift (correct charter, wrong behavior), which may take the
    /// enforcement ladder up to a hard stop. A charter-drift class (added later) returns false and routes to
    /// maintenance instead, so the hard stop never reaps a healthy agent with a stale charter (doc_3410
    /// goals 6 and 7).
    pub fn is_behavioral(self) -> bool {
        match self {
            DriftClass::SwitchDoNotIdle => true,
        }
    }
}

/// The switch-do-not-idle self-check (task_736), PURE. The signal fires when an agent is present but NOT
/// actively working (presence is `idle`, `away`, or `blocked`) while it still holds at least one actionable,
/// non-blocked assigned task (counted by [`crate::board::open_task_count`]). `busy`/`online` means the agent
/// is working the task, so it is not a drift. `offline` is a deliberate stand-down handled by a different
/// lifecycle, so it is excluded here to avoid flagging an intentionally-parked agent. Idling under `blocked`
/// presence still counts when OTHER actionable work waits (doc_3410 goal 5: idling under blocked_on=operator
/// while actionable backlog exists is drift) — a task the agent is genuinely blocked on is not actionable, so
/// it does not inflate the count.
pub fn switch_do_not_idle_signal(presence: &str, actionable_open: usize) -> Option<DriftClass> {
    let present_not_working = matches!(presence, "idle" | "away" | "blocked");
    if present_not_working && actionable_open > 0 {
        Some(DriftClass::SwitchDoNotIdle)
    } else {
        None
    }
}

/// The action the mechanism takes on a tick after [`DriftState::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftAction {
    /// No drift this tick (or it cleared): nothing is injected.
    None,
    /// Drift within the grace window: inject a return-to-charter / switch-to-actionable directive the agent
    /// must process this tick (doc_3410 goals 1 and 2).
    Directive(DriftClass),
    /// Behavioral drift persisted past N ticks: escalate to a hard stop-and-return (doc_3410 goal 3). Reached
    /// only for a behavioral class; a non-behavioral class keeps issuing its directive instead.
    Escalate(DriftClass),
}

/// Per-agent drift state: the current drift class and how many consecutive ticks it has persisted. Persisted
/// at the edge (a later slice) under the hub state dir, like the heartbeat touch-file. `consecutive` is a
/// tick count rather than a wall-clock, so the rule has no clock dependency and is deterministically tested.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriftState {
    class: Option<DriftClass>,
    consecutive: u32,
}

impl DriftState {
    /// Advance the state by one tick given this tick's `signal`, with escalation threshold `n` (doc_3410
    /// default N=2: a directive is issued while the same behavioral drift has persisted for up to `n` ticks;
    /// the tick after that escalates). Rules:
    /// - no signal, or the signal cleared: reset and take no action;
    /// - a new class (first flag, or a class different from last tick): start the counter at 1 and issue a
    ///   directive;
    /// - the same class persisting: increment; while `consecutive <= n` issue a directive, and once it
    ///   exceeds `n` escalate — but only for a BEHAVIORAL class; a non-behavioral class keeps issuing its
    ///   directive, since charter drift routes off the enforcement ladder.
    pub fn observe(&mut self, signal: Option<DriftClass>, n: u32) -> DriftAction {
        match signal {
            None => {
                *self = DriftState::default();
                DriftAction::None
            }
            Some(class) => {
                if self.class == Some(class) {
                    self.consecutive = self.consecutive.saturating_add(1);
                } else {
                    self.class = Some(class);
                    self.consecutive = 1;
                }
                if self.consecutive > n && class.is_behavioral() {
                    DriftAction::Escalate(class)
                } else {
                    DriftAction::Directive(class)
                }
            }
        }
    }

    /// The current consecutive-tick count for the active drift class (0 when clean). For the edge to persist.
    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }

    /// The active drift class, if any.
    pub fn class(&self) -> Option<DriftClass> {
        self.class
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_with_actionable_backlog_is_a_signal() {
        assert_eq!(
            switch_do_not_idle_signal("idle", 1),
            Some(DriftClass::SwitchDoNotIdle)
        );
        assert_eq!(
            switch_do_not_idle_signal("away", 3),
            Some(DriftClass::SwitchDoNotIdle)
        );
        // doc_3410 goal 5: idling under `blocked` presence while OTHER actionable work waits is drift.
        assert_eq!(
            switch_do_not_idle_signal("blocked", 1),
            Some(DriftClass::SwitchDoNotIdle)
        );
    }

    #[test]
    fn working_or_no_backlog_or_offline_is_not_a_signal() {
        // Actively working: not a drift regardless of backlog.
        assert_eq!(switch_do_not_idle_signal("busy", 5), None);
        assert_eq!(switch_do_not_idle_signal("online", 5), None);
        // Idle but nothing actionable to switch to: not a drift.
        assert_eq!(switch_do_not_idle_signal("idle", 0), None);
        assert_eq!(switch_do_not_idle_signal("away", 0), None);
        // Offline is a deliberate stand-down, excluded even with backlog.
        assert_eq!(switch_do_not_idle_signal("offline", 4), None);
    }

    #[test]
    fn a_clean_tick_resets_the_counter() {
        let mut s = DriftState::default();
        assert_eq!(
            s.observe(Some(DriftClass::SwitchDoNotIdle), 2),
            DriftAction::Directive(DriftClass::SwitchDoNotIdle)
        );
        assert_eq!(s.consecutive(), 1);
        assert_eq!(s.observe(None, 2), DriftAction::None);
        assert_eq!(s.consecutive(), 0);
        assert_eq!(s.class(), None);
    }

    #[test]
    fn behavioral_drift_graduates_directive_then_escalates_past_n() {
        let mut s = DriftState::default();
        let c = DriftClass::SwitchDoNotIdle;
        // N=2: ticks 1 and 2 get a directive (the grace window); tick 3 escalates.
        assert_eq!(s.observe(Some(c), 2), DriftAction::Directive(c));
        assert_eq!(s.observe(Some(c), 2), DriftAction::Directive(c));
        assert_eq!(s.observe(Some(c), 2), DriftAction::Escalate(c));
        // It stays escalated while the drift persists.
        assert_eq!(s.observe(Some(c), 2), DriftAction::Escalate(c));
    }

    #[test]
    fn correcting_before_n_avoids_escalation() {
        let mut s = DriftState::default();
        let c = DriftClass::SwitchDoNotIdle;
        assert_eq!(s.observe(Some(c), 2), DriftAction::Directive(c));
        // Agent corrected: a clean tick clears the state, so a later flag starts fresh at a directive.
        assert_eq!(s.observe(None, 2), DriftAction::None);
        assert_eq!(s.observe(Some(c), 2), DriftAction::Directive(c));
        assert_eq!(s.consecutive(), 1);
    }
}
