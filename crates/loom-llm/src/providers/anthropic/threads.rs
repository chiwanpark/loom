use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::request::{assistant_param, billing_block, cache_tail};
use crate::error::LlmError;
use crate::types::{AssistantMessage, CompletionRequest, Message};

pub(super) const BETA: &str = "message-threads-2026-08-12";
const MAX_SESSIONS: usize = 64;

#[derive(Default)]
pub(super) struct Threads(Mutex<VecDeque<(String, Entry)>>);

enum Entry {
    Off,
    Ready(Last),
}

struct Last {
    prefix: [u8; 32],
    len: usize,
    reply_id: String,
}

pub(super) enum Mode {
    Plain,
    Create,
    Continue { previous: String, offset: usize },
}

impl Mode {
    pub fn threaded(&self) -> bool {
        !matches!(self, Self::Plain)
    }

    pub fn body(&self, full: &Value, text: Option<&str>, version: &str) -> Value {
        let fields = full.as_object().unwrap();
        let mut fields = match self {
            Self::Continue { .. } => fields
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "system" | "tools" | "messages"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            _ => fields.clone(),
        };
        match self {
            Self::Plain => {}
            Self::Create => {
                fields.insert("thread".into(), json!({"type": "create"}));
            }
            Self::Continue { previous, offset } => {
                fields.insert("messages".into(), json!(messages(full)[*offset..]));
                fields.insert(
                    "thread".into(),
                    json!({"type": "continue", "previous_message_id": previous}),
                );
            }
        }
        if let Some(text) = text {
            let system = fields.entry("system").or_insert_with(|| json!([]));
            system
                .as_array_mut()
                .unwrap()
                .insert(0, billing_block(text, version));
        }
        let mut body = Value::Object(fields);
        cache_tail(&mut body);
        body
    }

    pub fn fallback(&self, error: &LlmError) -> Option<Self> {
        let LlmError::Api {
            status,
            code,
            message,
            ..
        } = error
        else {
            return None;
        };
        match (self, code.as_deref()) {
            (
                Self::Continue { .. },
                Some(
                    "thread_not_found"
                    | "thread_already_continued"
                    | "thread_continue_in_flight"
                    | "thread_fingerprint_mismatch",
                ),
            ) => Some(Self::Create),
            (mode, Some("thread_unsupported_request")) if mode.threaded() => Some(Self::Plain),
            (mode, _)
                if mode.threaded()
                    && *status == 400
                    && (message.contains("thread: Extra inputs are not permitted")
                        || message.contains(BETA)) =>
            {
                Some(Self::Plain)
            }
            _ => None,
        }
    }
}

impl Threads {
    pub fn prepare(
        &self,
        session: &str,
        token: &str,
        full: &Value,
        request: &CompletionRequest,
    ) -> Mode {
        let last = {
            let mut entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
            match entries.iter().position(|(id, _)| id == session) {
                Some(i) => match &entries[i].1 {
                    Entry::Off => return Mode::Plain,
                    Entry::Ready(_) => entries.remove(i).map(|(_, entry)| entry),
                },
                None => None,
            }
        };
        let Some(Entry::Ready(last)) = last else {
            return Mode::Create;
        };
        let reply = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(reply) => Some(reply),
                _ => None,
            });
        let history = messages(full);
        if history.get(last.len).is_some_and(|m| m["role"] == "user")
            && reply.and_then(|r| r.response_id.as_deref()) == Some(last.reply_id.as_str())
            && Prefix::new(token, full, &history[..last.len]).finish() == last.prefix
        {
            Mode::Continue {
                previous: last.reply_id,
                offset: last.len,
            }
        } else {
            Mode::Create
        }
    }

    pub fn disable(&self, session: &str) {
        self.put(session.to_owned(), Entry::Off);
    }

    pub fn record(&self, session: String, mut prefix: Prefix, reply: &AssistantMessage) {
        let Some(reply_id) = reply.response_id.as_ref().filter(|id| !id.is_empty()) else {
            return;
        };
        let param = assistant_param(reply);
        if param["content"].as_array().unwrap().is_empty()
            || reply.tool_calls().any(|call| !call.arguments.is_object())
        {
            return;
        }
        prefix.add(&param);
        self.put(
            session,
            Entry::Ready(Last {
                len: prefix.len,
                prefix: prefix.finish(),
                reply_id: reply_id.clone(),
            }),
        );
    }

    fn put(&self, session: String, entry: Entry) {
        let mut entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(entry, Entry::Ready(_))
            && entries
                .iter()
                .any(|(id, existing)| id == &session && matches!(existing, Entry::Off))
        {
            return;
        }
        entries.retain(|(id, _)| id != &session);
        if entries.len() >= MAX_SESSIONS {
            entries.pop_front();
        }
        entries.push_back((session, entry));
    }
}

pub(super) struct Prefix {
    hash: Sha256,
    len: usize,
}

impl Prefix {
    pub fn for_request(token: &str, full: &Value) -> Option<Self> {
        let history = messages(full);
        (history.last()?["role"] == "user").then(|| Self::new(token, full, history))
    }

    fn new(token: &str, full: &Value, history: &[Value]) -> Self {
        let mut hash = Sha256::new();
        hash.update(Sha256::digest(token.as_bytes()));
        for (key, value) in full.as_object().unwrap() {
            if key != "messages" {
                hash.update(serde_json::to_vec(&(key, value)).unwrap());
            }
        }
        let mut prefix = Self { hash, len: 0 };
        for message in history {
            prefix.add(message);
        }
        prefix
    }

    fn add(&mut self, message: &Value) {
        self.hash.update(serde_json::to_vec(message).unwrap());
        self.len += 1;
    }

    fn finish(self) -> [u8; 32] {
        self.hash.finalize().into()
    }
}

fn messages(body: &Value) -> &[Value] {
    body["messages"].as_array().unwrap()
}
