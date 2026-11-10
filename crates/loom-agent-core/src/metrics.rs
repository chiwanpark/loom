use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use loom_proto::{AgentMetrics, SessionEvent, TurnOutcome};

#[derive(Debug)]
pub struct Metrics {
    started: Instant,
    connections: AtomicU64,
    connections_total: AtomicU64,
    turns_completed: AtomicU64,
    turns_failed: AtomicU64,
    turns_interrupted: AtomicU64,
    tool_calls: AtomicU64,
    tool_errors: AtomicU64,
    approvals: AtomicU64,
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    compactions: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            connections: AtomicU64::new(0),
            connections_total: AtomicU64::new(0),
            turns_completed: AtomicU64::new(0),
            turns_failed: AtomicU64::new(0),
            turns_interrupted: AtomicU64::new(0),
            tool_calls: AtomicU64::new(0),
            tool_errors: AtomicU64::new(0),
            approvals: AtomicU64::new(0),
            input_tokens: AtomicU64::new(0),
            output_tokens: AtomicU64::new(0),
            compactions: AtomicU64::new(0),
        }
    }
}

fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
}

impl Metrics {
    pub fn observe(&self, event: &SessionEvent) {
        match event {
            SessionEvent::TurnFinished { outcome } => add(
                match outcome {
                    TurnOutcome::Completed => &self.turns_completed,
                    TurnOutcome::Failed => &self.turns_failed,
                    TurnOutcome::Interrupted => &self.turns_interrupted,
                },
                1,
            ),
            SessionEvent::ToolCallFinished { result, .. } => {
                add(&self.tool_calls, 1);
                if result.is_error {
                    add(&self.tool_errors, 1);
                }
            }
            SessionEvent::ApprovalRequired { .. } => add(&self.approvals, 1),
            SessionEvent::Usage { usage } => {
                add(&self.input_tokens, usage.prompt_tokens());
                add(&self.output_tokens, usage.output);
            }
            SessionEvent::ContextCompacted { .. } => add(&self.compactions, 1),
            _ => {}
        }
    }

    pub fn connection_opened(&self) {
        add(&self.connections, 1);
        add(&self.connections_total, 1);
    }

    pub fn connection_closed(&self) {
        self.connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self, sessions: u64, busy_sessions: u64) -> AgentMetrics {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        AgentMetrics {
            uptime_secs: self.started.elapsed().as_secs(),
            sessions,
            busy_sessions,
            connections: get(&self.connections),
            connections_total: get(&self.connections_total),
            turns_completed: get(&self.turns_completed),
            turns_failed: get(&self.turns_failed),
            turns_interrupted: get(&self.turns_interrupted),
            tool_calls: get(&self.tool_calls),
            tool_errors: get(&self.tool_errors),
            approvals: get(&self.approvals),
            input_tokens: get(&self.input_tokens),
            output_tokens: get(&self.output_tokens),
            compactions: get(&self.compactions),
        }
    }
}
