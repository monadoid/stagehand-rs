//! Metrics helpers mirroring the Stagehand Python implementation.
//!
//! Provides token accounting structures alongside lightweight timing helpers
//! for inference latency measurements.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Known function categories tracked by Stagehand when collecting metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StagehandFunctionName {
    Act,
    Extract,
    Observe,
    Agent,
}

/// Aggregated metrics for token usage and latency across Stagehand functions.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StagehandMetrics {
    pub act_prompt_tokens: u64,
    pub act_completion_tokens: u64,
    pub act_inference_time_ms: u64,

    pub extract_prompt_tokens: u64,
    pub extract_completion_tokens: u64,
    pub extract_inference_time_ms: u64,

    pub observe_prompt_tokens: u64,
    pub observe_completion_tokens: u64,
    pub observe_inference_time_ms: u64,

    pub agent_prompt_tokens: u64,
    pub agent_completion_tokens: u64,
    pub agent_inference_time_ms: u64,

    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_inference_time_ms: u64,
}

impl StagehandMetrics {
    /// Merge the values from another metrics instance into this one.
    pub fn merge(&mut self, other: &StagehandMetrics) {
        self.act_prompt_tokens += other.act_prompt_tokens;
        self.act_completion_tokens += other.act_completion_tokens;
        self.act_inference_time_ms += other.act_inference_time_ms;

        self.extract_prompt_tokens += other.extract_prompt_tokens;
        self.extract_completion_tokens += other.extract_completion_tokens;
        self.extract_inference_time_ms += other.extract_inference_time_ms;

        self.observe_prompt_tokens += other.observe_prompt_tokens;
        self.observe_completion_tokens += other.observe_completion_tokens;
        self.observe_inference_time_ms += other.observe_inference_time_ms;

        self.agent_prompt_tokens += other.agent_prompt_tokens;
        self.agent_completion_tokens += other.agent_completion_tokens;
        self.agent_inference_time_ms += other.agent_inference_time_ms;

        self.total_prompt_tokens += other.total_prompt_tokens;
        self.total_completion_tokens += other.total_completion_tokens;
        self.total_inference_time_ms += other.total_inference_time_ms;
    }

    /// Record metrics for a specific Stagehand function and update cumulative totals.
    pub fn record(
        &mut self,
        function: StagehandFunctionName,
        prompt_tokens: u64,
        completion_tokens: u64,
        inference_time_ms: u64,
    ) {
        match function {
            StagehandFunctionName::Act => {
                self.act_prompt_tokens += prompt_tokens;
                self.act_completion_tokens += completion_tokens;
                self.act_inference_time_ms += inference_time_ms;
            }
            StagehandFunctionName::Extract => {
                self.extract_prompt_tokens += prompt_tokens;
                self.extract_completion_tokens += completion_tokens;
                self.extract_inference_time_ms += inference_time_ms;
            }
            StagehandFunctionName::Observe => {
                self.observe_prompt_tokens += prompt_tokens;
                self.observe_completion_tokens += completion_tokens;
                self.observe_inference_time_ms += inference_time_ms;
            }
            StagehandFunctionName::Agent => {
                self.agent_prompt_tokens += prompt_tokens;
                self.agent_completion_tokens += completion_tokens;
                self.agent_inference_time_ms += inference_time_ms;
            }
        }

        self.total_prompt_tokens += prompt_tokens;
        self.total_completion_tokens += completion_tokens;
        self.total_inference_time_ms += inference_time_ms;
    }
}

/// Start an inference timer using [`Instant::now`].
pub fn start_inference_timer() -> Instant {
    Instant::now()
}

/// Return the elapsed milliseconds since the provided start instant.
pub fn get_inference_time_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

/// Helper for tests to convert milliseconds to [`Duration`].
pub fn duration_from_millis(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_updates_totals() {
        let mut metrics = StagehandMetrics::default();
        metrics.record(StagehandFunctionName::Act, 10, 5, 100);
        metrics.record(StagehandFunctionName::Act, 2, 3, 40);
        metrics.record(StagehandFunctionName::Observe, 1, 1, 20);

        assert_eq!(metrics.act_prompt_tokens, 12);
        assert_eq!(metrics.act_completion_tokens, 8);
        assert_eq!(metrics.act_inference_time_ms, 140);
        assert_eq!(metrics.observe_inference_time_ms, 20);
        assert_eq!(metrics.total_prompt_tokens, 13);
        assert_eq!(metrics.total_completion_tokens, 9);
        assert_eq!(metrics.total_inference_time_ms, 160);
    }

    #[test]
    fn merge_combines_two_instances() {
        let mut a = StagehandMetrics::default();
        a.record(StagehandFunctionName::Extract, 4, 2, 50);

        let mut b = StagehandMetrics::default();
        b.record(StagehandFunctionName::Extract, 1, 1, 20);
        b.record(StagehandFunctionName::Agent, 3, 2, 30);

        a.merge(&b);
        assert_eq!(a.extract_prompt_tokens, 5);
        assert_eq!(a.extract_completion_tokens, 3);
        assert_eq!(a.extract_inference_time_ms, 70);
        assert_eq!(a.agent_prompt_tokens, 3);
        assert_eq!(a.total_completion_tokens, 5);
    }

    #[test]
    fn timer_reports_elapsed_millis() {
        let start = start_inference_timer();
        std::thread::sleep(duration_from_millis(10));
        let elapsed = get_inference_time_ms(start);
        assert!(elapsed >= 10);
    }
}
