//! What the intercept sees in Claude Code traffic that the transformations
//! feed cannot show, kept for the Activity tips (`activity_facts.rs`). Memory
//! only: a signal a restart drops recurs with the traffic that raised it.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// Anthropic's prompt cache lasts an hour on a subscription, so a conversation
/// quiet for longer writes its whole context again on the next turn.
pub const IDLE_RESUME_GAP_MS: i64 = 65 * 60 * 1000;

static EXTRA_USAGE_IN_USE: AtomicBool = AtomicBool::new(false);
static USAGE_LIMIT_HIT: AtomicBool = AtomicBool::new(false);
/// Epoch seconds the last usage limit lifts (`anthropic-ratelimit-unified-reset`); 0 = unknown.
static USAGE_LIMIT_RESETS_AT: AtomicI64 = AtomicI64::new(0);
/// Epoch ms of the last conversation that came back after `IDLE_RESUME_GAP_MS`; 0 = none.
static IDLE_RESUME_AT_MS: AtomicI64 = AtomicI64::new(0);

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Observed {
    pub extra_usage_in_use: bool,
    pub usage_limit_hit: bool,
    pub usage_limit_resets_at: Option<i64>,
    pub idle_resume_at_ms: Option<i64>,
}

/// A Claude Code response head: `overage-in-use` is Anthropic's flag that
/// extra usage (billed per token) is paying for this request, and a 429 with
/// `unified-status: rejected` is the plan's usage limit, not a rate spike.
pub fn note_claude_response(overage_in_use: bool, usage_limit_resets_at: Option<Option<i64>>) {
    if overage_in_use {
        EXTRA_USAGE_IN_USE.store(true, Ordering::Relaxed);
    }
    if let Some(resets_at) = usage_limit_resets_at {
        USAGE_LIMIT_HIT.store(true, Ordering::Relaxed);
        USAGE_LIMIT_RESETS_AT.store(resets_at.unwrap_or(0), Ordering::Relaxed);
    }
}

/// A conversation sent a request `now_ms`, last active at `previous_ms`.
pub fn note_request_gap(previous_ms: i64, now_ms: i64) {
    if is_idle_resume(previous_ms, now_ms) {
        IDLE_RESUME_AT_MS.store(now_ms, Ordering::Relaxed);
    }
}

fn is_idle_resume(previous_ms: i64, now_ms: i64) -> bool {
    previous_ms > 0 && now_ms - previous_ms >= IDLE_RESUME_GAP_MS
}

pub fn observed() -> Observed {
    let idle = IDLE_RESUME_AT_MS.load(Ordering::Relaxed);
    let resets = USAGE_LIMIT_RESETS_AT.load(Ordering::Relaxed);
    Observed {
        extra_usage_in_use: EXTRA_USAGE_IN_USE.load(Ordering::Relaxed),
        usage_limit_hit: USAGE_LIMIT_HIT.load(Ordering::Relaxed),
        usage_limit_resets_at: (resets > 0).then_some(resets),
        idle_resume_at_ms: (idle > 0).then_some(idle),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_resume_is_a_known_conversation_back_after_its_cache_hour() {
        let minute = 60_000;
        assert!(is_idle_resume(1_000, 1_000 + 65 * minute));
        assert!(
            !is_idle_resume(1_000, 1_000 + 64 * minute),
            "cache still warm"
        );
        assert!(
            !is_idle_resume(0, 99 * minute),
            "never seen before: a new session"
        );
    }
}
