//! Aggregate counters for the CLI-wrap paths.
//!
//! Two shapes of number arrive from the CLI and they must not be treated the
//! same way. A result event's `usage` is **per turn**, so it is summed. Its
//! `total_cost_usd` is a **running total for that CLI session**, so only the
//! increment since the previous turn of the same session may be added — see
//! [`cost_delta`].

use crate::messages::content::SessionUsage;

#[derive(Debug, Default)]
pub struct Stats {
    /// Stateless `/query` and `/query/stream` calls.
    pub total_queries: u64,
    /// Completed turns on `/sessions` sessions.
    pub total_session_turns: u64,
    /// Input tokens billed at full rate — excludes the cache counters below.
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cost_usd: f64,
}

impl Stats {
    /// Fold one completed turn into the totals.
    ///
    /// `cost_delta` must already be the increment for this turn, not the
    /// session's running total.
    pub fn record_turn(&mut self, usage: Option<&SessionUsage>, cost_delta: f64) {
        if let Some(usage) = usage {
            self.total_input_tokens += usage.input_tokens;
            self.total_output_tokens += usage.output_tokens;
            self.total_cache_read_tokens += usage.cache_read_input_tokens.unwrap_or(0);
            self.total_cache_creation_tokens += usage.cache_creation_input_tokens.unwrap_or(0);
        }
        self.total_cost_usd += cost_delta;
    }
}

/// Increment to charge for a turn, given the running total already seen for
/// that CLI session. Updates `seen` in place.
///
/// A stateless query is its own CLI session, so it starts from `0.0` and the
/// first reported total is charged whole.
///
/// Never returns a negative increment: the CLI is expected to report a
/// monotonic total, and a decrease (a restarted or re-numbered session) should
/// not subtract from the running figure.
pub fn cost_delta(seen: &mut f64, reported: Option<f64>) -> f64 {
    let Some(reported) = reported else {
        return 0.0;
    };
    let delta = (reported - *seen).max(0.0);
    *seen = reported;
    delta
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cache_read: u64, cache_create: u64) -> SessionUsage {
        SessionUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: Some(cache_read),
            cache_creation_input_tokens: Some(cache_create),
        }
    }

    /// Measured against the CLI: three turns of one session reported
    /// 0.084893 → 0.100401 → 0.1135895. Summing those verbatim would report
    /// roughly 2.6x the real spend.
    #[test]
    fn session_cost_charges_only_the_increment() {
        let mut stats = Stats::default();
        let mut seen = 0.0;

        for reported in [0.084893, 0.100401, 0.1135895] {
            stats.record_turn(None, cost_delta(&mut seen, Some(reported)));
        }

        assert!((stats.total_cost_usd - 0.1135895).abs() < 1e-9);
    }

    #[test]
    fn a_stateless_query_is_charged_whole() {
        let mut stats = Stats::default();
        let mut seen = 0.0;

        stats.record_turn(None, cost_delta(&mut seen, Some(0.0575785)));

        assert!((stats.total_cost_usd - 0.0575785).abs() < 1e-9);
    }

    #[test]
    fn a_missing_cost_charges_nothing_and_keeps_the_watermark() {
        let mut seen = 0.5;

        assert_eq!(cost_delta(&mut seen, None), 0.0);
        assert_eq!(seen, 0.5);
    }

    /// A total that goes backwards must not subtract from the running figure.
    #[test]
    fn a_decreasing_total_charges_nothing() {
        let mut seen = 1.0;

        assert_eq!(cost_delta(&mut seen, Some(0.25)), 0.0);
    }

    #[test]
    fn usage_is_summed_per_turn_across_every_bucket() {
        let mut stats = Stats::default();

        stats.record_turn(Some(&usage(2, 3, 18112, 7514)), 0.0);
        stats.record_turn(Some(&usage(2, 3, 25626, 261)), 0.0);

        assert_eq!(stats.total_input_tokens, 4);
        assert_eq!(stats.total_output_tokens, 6);
        assert_eq!(stats.total_cache_read_tokens, 43738);
        assert_eq!(stats.total_cache_creation_tokens, 7775);
    }
}
