//! Turns session events into OpenTelemetry metrics and log events, named and
//! shaped like Claude Code's so the same dashboards work.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use loom_agent_core::SessionObserver;
use loom_agent_core::policy::DENIED_BY_POLICY;
use loom_config::TelemetryConfig;
use loom_proto::{
    ContentPart, Decision, SessionEvent, SessionId, SessionState, ToolResult, ToolResultDetails,
    Usage,
};
use opentelemetry::logs::{AnyValue, LogRecord, Logger, Severity};
use opentelemetry::metrics::{Counter, Meter};
use opentelemetry::{KeyValue, Value};
use opentelemetry_sdk::logs::SdkLogger;
use serde_json::Value as Json;

use crate::mappers;

pub const REDACTED: &str = "<REDACTED>";

pub(crate) struct Counters {
    session: Counter<u64>,
    tokens: Counter<u64>,
    lines: Counter<u64>,
    commits: Counter<u64>,
    pull_requests: Counter<u64>,
    edit_decisions: Counter<u64>,
    active_time: Counter<f64>,
}

impl Counters {
    pub(crate) fn new(meter: &Meter, prefix: &str) -> Self {
        let name = |suffix: &str| format!("{prefix}.{suffix}");
        Self {
            session: meter
                .u64_counter(name("session.count"))
                .with_description("Count of sessions started")
                .build(),
            tokens: meter
                .u64_counter(name("token.usage"))
                .with_description("Number of tokens used")
                .with_unit("tokens")
                .build(),
            lines: meter
                .u64_counter(name("lines_of_code.count"))
                .with_description(
                    "Count of lines of code modified, with the 'type' attribute indicating \
                     whether lines were added or removed",
                )
                .build(),
            commits: meter
                .u64_counter(name("commit.count"))
                .with_description("Number of git commits created")
                .build(),
            pull_requests: meter
                .u64_counter(name("pull_request.count"))
                .with_description("Number of pull requests created")
                .build(),
            edit_decisions: meter
                .u64_counter(name("code_edit_tool.decision"))
                .with_description(
                    "Count of code editing tool permission decisions (accept/reject) for the \
                     edit and write tools",
                )
                .build(),
            active_time: meter
                .f64_counter(name("active_time.total"))
                .with_description("Total active time in seconds")
                .with_unit("s")
                .build(),
        }
    }
}

/// What the recorder remembers about one session between its events.
#[derive(Default)]
struct Track {
    /// `provider/model` of the last response.
    model: Option<String>,
    /// When the session last started waiting for the model.
    request_started: Option<Instant>,
    /// The length of the request whose usage comes next.
    request_duration: Option<Duration>,
    /// When the session last left the idle state.
    busy_since: Option<Instant>,
    tools: HashMap<String, ToolTrack>,
}

struct ToolTrack {
    name: String,
    arguments: Json,
    started: Option<Instant>,
    /// `(decision, source)`, once known.
    decision: Option<(&'static str, &'static str)>,
}

pub(crate) struct Recorder {
    config: TelemetryConfig,
    version: String,
    counters: Option<Counters>,
    logger: Option<SdkLogger>,
    sequence: AtomicU64,
    sessions: Mutex<HashMap<SessionId, Track>>,
}

/// Attributes of one data point or event.
type Attributes = Vec<KeyValue>;

fn kv(key: &'static str, value: impl Into<Value>) -> KeyValue {
    KeyValue::new(key, value)
}

fn any_value(value: Value) -> AnyValue {
    match value {
        Value::Bool(b) => AnyValue::Boolean(b),
        Value::I64(i) => AnyValue::Int(i),
        Value::F64(f) => AnyValue::Double(f),
        Value::String(s) => AnyValue::String(s),
        other => AnyValue::String(other.to_string().into()),
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Splits `provider/model` into its parts.
fn model_attributes(model: Option<&str>) -> Attributes {
    match model.and_then(|m| m.split_once('/')) {
        Some((provider, name)) => vec![
            kv("model", name.to_owned()),
            kv("provider", provider.to_owned()),
        ],
        None => model
            .map(|m| vec![kv("model", m.to_owned())])
            .unwrap_or_default(),
    }
}

fn prompt_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lines added and removed by a successful edit or write.
fn line_counts(track: &ToolTrack, result: &ToolResult) -> Option<(u64, u64)> {
    match &result.details {
        Some(
            ToolResultDetails::Edit { diff, .. } | ToolResultDetails::WriteWithDiff { diff, .. },
        ) => Some(mappers::diff_line_counts(diff)),
        Some(ToolResultDetails::Write { .. }) => mappers::string_arg(&track.arguments, "content")
            .filter(|content| !content.is_empty())
            .map(|content| (content.lines().count() as u64, 0)),
        _ => None,
    }
}

impl Recorder {
    pub(crate) fn new(
        config: TelemetryConfig,
        version: String,
        counters: Option<Counters>,
        logger: Option<SdkLogger>,
    ) -> Self {
        Self {
            config,
            version,
            counters,
            logger,
            sequence: AtomicU64::new(0),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn base(&self, session: &SessionId) -> Attributes {
        vec![
            kv("session.id", session.0.clone()),
            kv("app.version", self.version.clone()),
        ]
    }

    fn content(&self, enabled: bool, text: &str) -> String {
        if enabled {
            mappers::truncate(text, self.config.content_max_length)
        } else {
            REDACTED.into()
        }
    }

    fn add(
        &self,
        counter: impl Fn(&Counters) -> &Counter<u64>,
        value: u64,
        attributes: &[KeyValue],
    ) {
        if let Some(counters) = &self.counters {
            counter(counters).add(value, attributes);
        }
    }

    fn event(&self, name: &str, mut attributes: Attributes) {
        let Some(logger) = &self.logger else {
            return;
        };
        let now = SystemTime::now();
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        attributes.splice(
            0..0,
            [
                kv("event.name", name.to_owned()),
                kv(
                    "event.timestamp",
                    humantime::format_rfc3339_millis(now).to_string(),
                ),
                kv("event.sequence", int(sequence)),
            ],
        );
        let mut record = logger.create_log_record();
        record.set_body(AnyValue::from(format!("{}.{name}", self.config.prefix)));
        record.set_timestamp(now);
        record.set_observed_timestamp(now);
        record.set_severity_number(Severity::Info);
        record.set_severity_text("INFO");
        record.add_attributes(
            attributes
                .into_iter()
                .map(|KeyValue { key, value, .. }| (key, any_value(value))),
        );
        logger.emit(record);
    }

    fn decision(
        &self,
        session: &SessionId,
        call_id: &str,
        track: &ToolTrack,
        decision: &'static str,
        source: &'static str,
    ) {
        let mut attributes = self.base(session);
        attributes.extend([
            kv("tool_name", track.name.clone()),
            kv("tool_use_id", call_id.to_owned()),
            kv("decision", decision),
            kv("source", source),
        ]);
        if self.config.log_tool_details {
            attributes.push(kv(
                "tool_parameters",
                mappers::tool_parameters(&track.name, &track.arguments).to_string(),
            ));
        }
        self.event("tool_decision", attributes);

        if let Some(tool) = mappers::code_edit_tool(&track.name) {
            let mut attributes = self.base(session);
            attributes.extend([
                kv("decision", decision),
                kv("source", source),
                kv("tool_name", tool),
            ]);
            if let Some(language) =
                mappers::string_arg(&track.arguments, "path").and_then(mappers::language)
            {
                attributes.push(kv("language", language));
            }
            self.add(|c| &c.edit_decisions, 1, &attributes);
        }
    }

    fn tool_result(
        &self,
        session: &SessionId,
        call_id: &str,
        track: &ToolTrack,
        model: Option<&str>,
        result: &ToolResult,
    ) {
        let duration = track.started.map(|t| t.elapsed()).unwrap_or_default();
        let (decision, source) = track.decision.unwrap_or(("accept", "config"));
        let mut attributes = self.base(session);
        attributes.extend([
            kv("tool_name", track.name.clone()),
            kv("tool_use_id", call_id.to_owned()),
            kv("success", if result.is_error { "false" } else { "true" }),
            kv("duration_ms", millis(duration)),
            kv("decision_type", decision),
            kv("decision_source", source),
            kv(
                "tool_input_size_bytes",
                int(track.arguments.to_string().len() as u64),
            ),
            kv("tool_result_size_bytes", int(result.content.len() as u64)),
        ]);
        if result.is_error {
            attributes.push(kv("error_type", mappers::error_type(&result.content)));
        }
        if self.config.log_tool_details {
            if result.is_error {
                attributes.push(kv(
                    "error",
                    mappers::truncate(&result.content, self.config.content_max_length),
                ));
            }
            attributes.extend([
                kv(
                    "tool_parameters",
                    mappers::tool_parameters(&track.name, &track.arguments).to_string(),
                ),
                kv("tool_input", mappers::tool_input(&track.arguments)),
            ]);
        }
        self.event("tool_result", attributes);

        if result.is_error {
            return;
        }
        if mappers::code_edit_tool(&track.name).is_some()
            && let Some((added, removed)) = line_counts(track, result)
        {
            let mut attributes = self.base(session);
            attributes.extend(model_attributes(model));
            for (kind, lines) in [("added", added), ("removed", removed)] {
                let mut attributes = attributes.clone();
                attributes.push(kv("type", kind));
                self.add(|c| &c.lines, lines, &attributes);
            }
        }
        if mappers::is_shell(&track.name)
            && let Some(command) = mappers::string_arg(&track.arguments, "command")
        {
            let attributes = self.base(session);
            if mappers::creates_commit(command) {
                self.add(|c| &c.commits, 1, &attributes);
            }
            let pull_requests = mappers::pull_requests_created(command);
            if pull_requests > 0 {
                self.add(|c| &c.pull_requests, pull_requests, &attributes);
            }
        }
    }

    fn usage(&self, session: &SessionId, track: &mut Track, usage: &Usage) {
        let model = model_attributes(track.model.as_deref());
        let mut attributes = self.base(session);
        attributes.extend(model.iter().cloned());
        attributes.extend([
            kv("input_tokens", int(usage.input)),
            kv("output_tokens", int(usage.output)),
            kv("cache_read_tokens", int(usage.cache_read)),
            kv("cache_creation_tokens", int(usage.cache_write)),
            kv("reasoning_tokens", int(usage.reasoning)),
        ]);
        if let Some(duration) = track.request_duration.take() {
            attributes.push(kv("duration_ms", millis(duration)));
        }
        self.event("api_request", attributes);

        let mut base = self.base(session);
        base.extend(model);
        for (kind, tokens) in [
            ("input", usage.input),
            ("output", usage.output),
            ("cacheRead", usage.cache_read),
            ("cacheCreation", usage.cache_write),
        ] {
            let mut attributes = base.clone();
            attributes.push(kv("type", kind));
            self.add(|c| &c.tokens, tokens, &attributes);
        }
    }

    fn state(&self, session: &SessionId, track: &mut Track, state: SessionState) {
        let now = Instant::now();
        if state == SessionState::Idle {
            if let Some(since) = track.busy_since.take()
                && let Some(counters) = &self.counters
            {
                let mut attributes = self.base(session);
                attributes.push(kv("type", "cli"));
                counters
                    .active_time
                    .add(now.duration_since(since).as_secs_f64(), &attributes);
            }
        } else {
            track.busy_since.get_or_insert(now);
        }
        if state == SessionState::Thinking {
            track.request_started.get_or_insert(now);
        }
    }
}

impl SessionObserver for Recorder {
    fn session_created(&self, session: &SessionId) {
        let mut attributes = self.base(session);
        attributes.push(kv("start_type", "fresh"));
        self.add(|c| &c.session, 1, &attributes);
    }

    fn event(&self, session: &SessionId, event: &SessionEvent) {
        if event.is_ephemeral() {
            return;
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let track = sessions.entry(session.clone()).or_default();
        match event {
            SessionEvent::UserMessage { content } => {
                let prompt = prompt_text(content);
                let mut attributes = self.base(session);
                attributes.extend([
                    kv("prompt_length", int(prompt.chars().count() as u64)),
                    kv(
                        "prompt",
                        self.content(self.config.log_user_prompts, &prompt),
                    ),
                ]);
                self.event("user_prompt", attributes);
            }
            SessionEvent::StateChanged { state } => self.state(session, track, *state),
            SessionEvent::AssistantMessage { text, model, .. } => {
                track.model = Some(model.clone());
                track.request_duration = track.request_started.take().map(|t| t.elapsed());
                if !text.is_empty() {
                    let mut attributes = self.base(session);
                    attributes.extend(model_attributes(Some(model)));
                    attributes.extend([
                        kv("response_length", int(text.chars().count() as u64)),
                        kv(
                            "response",
                            self.content(self.config.log_assistant_responses, text),
                        ),
                    ]);
                    self.event("assistant_response", attributes);
                }
            }
            SessionEvent::Usage { usage } => self.usage(session, track, usage),
            SessionEvent::Error { message } => {
                let mut attributes = self.base(session);
                attributes.extend(model_attributes(track.model.as_deref()));
                attributes.push(kv(
                    "error",
                    mappers::truncate(message, self.config.content_max_length),
                ));
                if let Some(started) = track.request_started.take() {
                    attributes.push(kv("duration_ms", millis(started.elapsed())));
                }
                self.event("api_error", attributes);
            }
            SessionEvent::ToolCallRequested { call } => {
                let arguments = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| Json::String(call.arguments.clone()));
                track.tools.insert(
                    call.call_id.clone(),
                    ToolTrack {
                        name: call.name.clone(),
                        arguments,
                        started: None,
                        decision: None,
                    },
                );
            }
            SessionEvent::ApprovalResolved { call_id, decision } => {
                if let Some(tool) = track.tools.get_mut(call_id) {
                    let (decision, source) = match decision {
                        Decision::Allow => ("accept", "user_temporary"),
                        Decision::AllowForSession => ("accept", "user_permanent"),
                        Decision::Deny => ("reject", "user_reject"),
                    };
                    tool.decision = Some((decision, source));
                    self.decision(session, call_id, tool, decision, source);
                }
            }
            SessionEvent::ToolCallStarted { call_id } => {
                if let Some(tool) = track.tools.get_mut(call_id) {
                    tool.started = Some(Instant::now());
                    if tool.decision.is_none() {
                        tool.decision = Some(("accept", "config"));
                        self.decision(session, call_id, tool, "accept", "config");
                    }
                }
            }
            SessionEvent::ToolCallFinished { call_id, result } => {
                let Some(tool) = track.tools.remove(call_id) else {
                    return;
                };
                if tool.started.is_some() {
                    self.tool_result(session, call_id, &tool, track.model.as_deref(), result);
                } else if tool.decision.is_none() && result.content.starts_with(DENIED_BY_POLICY) {
                    self.decision(session, call_id, &tool, "reject", "config");
                }
            }
            SessionEvent::TurnFinished { .. } => {
                track.request_started = None;
                track.request_duration = None;
                track.tools.clear();
            }
            _ => {}
        }
    }
}
