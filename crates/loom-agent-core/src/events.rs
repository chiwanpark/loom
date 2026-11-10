use std::sync::Mutex;

use loom_proto::{SequencedEvent, SessionEvent, SessionId, SessionState};

use crate::store::EventStore;
use tokio::sync::broadcast;

const CHANNEL_CAPACITY: usize = 4096;

#[derive(Debug)]
struct Log {
    next_seq: u64,
    events: Vec<SequencedEvent>,
    state: SessionState,
}

pub struct EventHub {
    log: Mutex<Log>,
    tx: broadcast::Sender<SequencedEvent>,
    store: Option<(EventStore, SessionId)>,
}

impl std::fmt::Debug for EventHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventHub").field("log", &self.log).finish()
    }
}

#[derive(Debug)]
pub struct Subscription {
    pub backlog: Vec<SequencedEvent>,
    pub live: broadcast::Receiver<SequencedEvent>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    pub fn new() -> Self {
        Self::restore(None, Vec::new())
    }

    pub fn restore(store: Option<(EventStore, SessionId)>, events: Vec<SequencedEvent>) -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let next_seq = events.last().map(|e| e.seq + 1).unwrap_or(1);
        Self {
            log: Mutex::new(Log {
                next_seq,
                events,
                state: SessionState::Idle,
            }),
            tx,
            store,
        }
    }

    pub fn events(&self) -> Vec<SequencedEvent> {
        self.lock().events.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Log> {
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn emit(&self, event: SessionEvent) -> u64 {
        let mut log = self.lock();
        let seq = log.next_seq;
        log.next_seq += 1;
        match &event {
            SessionEvent::AssistantMessage { .. } => log.events.retain(|e| {
                !matches!(
                    e.event,
                    SessionEvent::AssistantDelta { .. } | SessionEvent::ReasoningDelta { .. }
                )
            }),
            SessionEvent::ToolCallFinished { call_id, .. } => log.events.retain(|e| {
                !matches!(&e.event, SessionEvent::ToolOutput { call_id: id, .. } if id == call_id)
            }),
            SessionEvent::StateChanged { state } => log.state = *state,
            _ => {}
        }
        let record = SequencedEvent { seq, event };
        if let Some((store, session)) = &self.store
            && !record.event.is_ephemeral()
        {
            store.append(session, &record);
        }
        log.events.push(record.clone());
        let _ = self.tx.send(record);
        seq
    }

    pub fn set_state(&self, state: SessionState) {
        if self.state() != state {
            self.emit(SessionEvent::StateChanged { state });
        }
    }

    pub fn state(&self) -> SessionState {
        self.lock().state
    }

    pub fn last_seq(&self) -> u64 {
        self.lock().next_seq - 1
    }

    pub fn subscribe(&self, since_seq: Option<u64>) -> Subscription {
        let log = self.lock();
        let since = since_seq.unwrap_or(0);
        let backlog = log
            .events
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect();
        let live = self.tx.subscribe();
        drop(log);
        Subscription { backlog, live }
    }
}

#[cfg(test)]
mod tests {
    use loom_proto::{OutputChunk, OutputStream, ToolResult};

    use super::*;

    #[test]
    fn subscription_has_no_gaps() {
        let hub = EventHub::new();
        hub.emit(SessionEvent::Warning {
            message: "a".into(),
        });
        let mut sub = hub.subscribe(None);
        hub.emit(SessionEvent::Warning {
            message: "b".into(),
        });
        assert_eq!(sub.backlog.len(), 1);
        assert_eq!(sub.backlog[0].seq, 1);
        assert_eq!(sub.live.try_recv().unwrap().seq, 2);
        assert_eq!(hub.subscribe(Some(1)).backlog.len(), 1);
        assert_eq!(hub.last_seq(), 2);
    }

    #[test]
    fn compacts_ephemeral_events() {
        let hub = EventHub::new();
        hub.emit(SessionEvent::AssistantDelta { text: "a".into() });
        hub.emit(SessionEvent::ReasoningDelta { text: "r".into() });
        hub.emit(SessionEvent::AssistantMessage {
            text: "a".into(),
            reasoning: None,
            model: "m".into(),
        });
        hub.emit(SessionEvent::ToolOutput {
            call_id: "c".into(),
            chunk: OutputChunk {
                stream: OutputStream::Stdout,
                data: b"x".to_vec(),
            },
        });
        hub.emit(SessionEvent::ToolCallFinished {
            call_id: "c".into(),
            result: ToolResult {
                content: "x".into(),
                is_error: false,
                details: None,
            },
        });
        let seqs: Vec<u64> = hub.subscribe(None).backlog.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [3, 5]);
    }

    #[test]
    fn tracks_state() {
        let hub = EventHub::new();
        hub.set_state(SessionState::Idle);
        assert_eq!(hub.last_seq(), 0);
        hub.set_state(SessionState::Thinking);
        assert_eq!(hub.state(), SessionState::Thinking);
        assert_eq!(hub.last_seq(), 1);
    }
}
