use std::collections::BTreeMap;

use loom_proto::{
    ContentPart, Decision, SessionEvent, SessionId, SessionState, ToolCall, ToolResult,
    ToolResultDetails, TurnOutcome, Usage,
};
use opentelemetry::logs::AnyValue;
use opentelemetry_sdk::logs::InMemoryLogExporter;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};

use super::*;

fn identity() -> AgentIdentity {
    AgentIdentity {
        version: "0.1.0".into(),
        instance: "inst-1".into(),
        workspace: "/work/loom".into(),
    }
}

fn enabled() -> TelemetryConfig {
    TelemetryConfig {
        enabled: true,
        ..TelemetryConfig::default()
    }
}

struct Exported {
    /// `(name, attributes, value)` of every sum data point.
    points: Vec<(String, BTreeMap<String, String>, f64)>,
    /// `(body, attributes)` of every event, in order.
    events: Vec<(String, BTreeMap<String, String>)>,
    resource: BTreeMap<String, String>,
}

impl Exported {
    fn value(&self, name: &str, attributes: &[(&str, &str)]) -> f64 {
        self.points
            .iter()
            .filter(|(n, attrs, _)| {
                n == name
                    && attributes
                        .iter()
                        .all(|(k, v)| attrs.get(*k).map(String::as_str) == Some(*v))
            })
            .map(|(_, _, value)| value)
            .sum()
    }

    fn names(&self) -> Vec<&str> {
        self.events.iter().map(|(body, _)| body.as_str()).collect()
    }

    fn event(&self, body: &str, index: usize) -> &BTreeMap<String, String> {
        &self
            .events
            .iter()
            .filter(|(b, _)| b == body)
            .nth(index)
            .unwrap_or_else(|| panic!("no event {body} #{index}"))
            .1
    }
}

fn any_to_string(value: &AnyValue) -> String {
    match value {
        AnyValue::String(s) => s.to_string(),
        AnyValue::Int(i) => i.to_string(),
        AnyValue::Double(d) => d.to_string(),
        AnyValue::Boolean(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

fn run(config: TelemetryConfig, feed: impl FnOnce(&dyn SessionObserver)) -> Exported {
    let metrics = InMemoryMetricExporter::default();
    let logs = InMemoryLogExporter::default();
    let telemetry = Telemetry::with_exporters(
        &config,
        &identity(),
        Some(metrics.clone()),
        Some(logs.clone()),
    );
    feed(telemetry.observer().as_ref());
    telemetry.flush();

    let finished = metrics.get_finished_metrics().unwrap();
    let last = finished.last().expect("no metrics were exported");
    let mut points = Vec::new();
    for scope in last.scope_metrics() {
        for metric in scope.metrics() {
            let attrs = |iter: &mut dyn Iterator<Item = &KeyValue>| {
                iter.map(|kv| (kv.key.to_string(), kv.value.to_string()))
                    .collect::<BTreeMap<_, _>>()
            };
            match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                    for point in sum.data_points() {
                        points.push((
                            metric.name().to_owned(),
                            attrs(&mut point.attributes()),
                            point.value() as f64,
                        ));
                    }
                }
                AggregatedMetrics::F64(MetricData::Sum(sum)) => {
                    for point in sum.data_points() {
                        points.push((
                            metric.name().to_owned(),
                            attrs(&mut point.attributes()),
                            point.value(),
                        ));
                    }
                }
                other => panic!("unexpected metric data {other:?}"),
            }
        }
    }
    let emitted = logs.get_emitted_logs().unwrap();
    let resource = last
        .resource()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let events = emitted
        .iter()
        .map(|log| {
            let body = log.record.body().map(any_to_string).unwrap_or_default();
            let attrs = log
                .record
                .attributes_iter()
                .map(|(k, v)| (k.to_string(), any_to_string(v)))
                .collect();
            (body, attrs)
        })
        .collect();
    telemetry.shutdown();
    Exported {
        points,
        events,
        resource,
    }
}

fn call(id: &str, name: &str, arguments: &str) -> SessionEvent {
    SessionEvent::ToolCallRequested {
        call: ToolCall {
            call_id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
            risk: None,
        },
    }
}

fn finished(
    id: &str,
    content: &str,
    is_error: bool,
    details: Option<ToolResultDetails>,
) -> SessionEvent {
    SessionEvent::ToolCallFinished {
        call_id: id.into(),
        result: ToolResult {
            content: content.into(),
            is_error,
            details,
        },
    }
}

fn state(state: SessionState) -> SessionEvent {
    SessionEvent::StateChanged { state }
}

/// A turn that exercises every metric and event: an approved edit, a shell
/// command allowed by the policy, a write the policy denies, a read the user
/// denies, and a failed request.
fn turn(observer: &dyn SessionObserver) {
    let s = SessionId("s1".into());
    let events = vec![
        SessionEvent::UserMessage {
            content: vec![ContentPart::text("fix the bug")],
        },
        state(SessionState::Thinking),
        SessionEvent::AssistantDelta { text: "Let".into() },
        SessionEvent::AssistantMessage {
            text: "Let me fix it".into(),
            reasoning: None,
            model: "anthropic/claude-x".into(),
        },
        SessionEvent::Usage {
            usage: Usage {
                input: 10,
                output: 5,
                cache_read: 100,
                cache_write: 7,
                reasoning: 2,
                total: 122,
            },
        },
        call("c1", "edit", r#"{"path":"src/lib.rs","old":"a","new":"b"}"#),
        state(SessionState::WaitingApproval),
        SessionEvent::ApprovalRequired {
            call_id: "c1".into(),
            reason: "ask".into(),
        },
        SessionEvent::ApprovalResolved {
            call_id: "c1".into(),
            decision: Decision::Allow,
        },
        SessionEvent::ToolCallStarted {
            call_id: "c1".into(),
        },
        state(SessionState::RunningTool),
        finished(
            "c1",
            "edited",
            false,
            Some(ToolResultDetails::Edit {
                path: "src/lib.rs".into(),
                diff: "--- a\n+++ b\n@@\n-a\n+b\n+c\n".into(),
                first_changed_line: Some(1),
            }),
        ),
        call(
            "c2",
            "shell",
            r#"{"command":"git commit -m x && gh pr create --fill"}"#,
        ),
        SessionEvent::ToolCallStarted {
            call_id: "c2".into(),
        },
        finished("c2", "ok", false, None),
        call("c3", "write", r#"{"path":"b.py","content":"x"}"#),
        finished(
            "c3",
            "this tool call was denied by policy (rule 1)",
            true,
            None,
        ),
        call("c4", "read", r#"{"path":"secret"}"#),
        SessionEvent::ApprovalResolved {
            call_id: "c4".into(),
            decision: Decision::Deny,
        },
        finished("c4", "the user denied this tool call", true, None),
        call("c5", "nope", "{}"),
        finished("c5", "unknown tool: nope", true, None),
        state(SessionState::Thinking),
        SessionEvent::Error {
            message: "rate limited".into(),
        },
        SessionEvent::TurnFinished {
            outcome: TurnOutcome::Failed,
        },
        state(SessionState::Idle),
    ];
    observer.session_created(&s);
    for event in events {
        observer.event(&s, &event);
    }
}

#[test]
fn records_claude_code_metrics() {
    let out = run(enabled(), turn);
    assert_eq!(
        out.value(
            "loom.session.count",
            &[("start_type", "fresh"), ("session.id", "s1")]
        ),
        1.0
    );
    let tokens = |kind| {
        out.value(
            "loom.token.usage",
            &[
                ("type", kind),
                ("model", "claude-x"),
                ("provider", "anthropic"),
            ],
        )
    };
    assert_eq!(
        [
            tokens("input"),
            tokens("output"),
            tokens("cacheRead"),
            tokens("cacheCreation")
        ],
        [10.0, 5.0, 100.0, 7.0]
    );
    assert_eq!(
        out.value(
            "loom.lines_of_code.count",
            &[("type", "added"), ("model", "claude-x")]
        ),
        2.0
    );
    assert_eq!(
        out.value("loom.lines_of_code.count", &[("type", "removed")]),
        1.0
    );
    assert_eq!(out.value("loom.commit.count", &[]), 1.0);
    assert_eq!(out.value("loom.pull_request.count", &[]), 1.0);
    assert_eq!(
        out.value(
            "loom.code_edit_tool.decision",
            &[
                ("tool_name", "Edit"),
                ("decision", "accept"),
                ("source", "user_temporary"),
                ("language", "Rust")
            ]
        ),
        1.0
    );
    assert_eq!(
        out.value(
            "loom.code_edit_tool.decision",
            &[
                ("tool_name", "Write"),
                ("decision", "reject"),
                ("source", "config"),
                ("language", "Python")
            ]
        ),
        1.0
    );
    assert_eq!(out.value("loom.code_edit_tool.decision", &[]), 2.0);
    assert!(out.value("loom.active_time.total", &[("type", "cli")]) > 0.0);
    assert_eq!(out.resource["service.name"], "loom-agent");
    assert_eq!(out.resource["service.version"], "0.1.0");
    assert_eq!(out.resource["loom.workspace"], "/work/loom");
}

#[test]
fn records_claude_code_events_with_content_redacted() {
    let out = run(enabled(), turn);
    assert_eq!(
        out.names(),
        [
            "loom.user_prompt",
            "loom.assistant_response",
            "loom.api_request",
            "loom.tool_decision",
            "loom.tool_result",
            "loom.tool_decision",
            "loom.tool_result",
            "loom.tool_decision",
            "loom.tool_decision",
            "loom.api_error",
        ]
    );
    let prompt = out.event("loom.user_prompt", 0);
    assert_eq!(prompt["event.name"], "user_prompt");
    assert_eq!(prompt["session.id"], "s1");
    assert_eq!(prompt["prompt"], REDACTED);
    assert_eq!(prompt["prompt_length"], "11");
    assert_eq!(
        out.event("loom.assistant_response", 0)["response"],
        REDACTED
    );

    let request = out.event("loom.api_request", 0);
    assert_eq!(request["model"], "claude-x");
    assert_eq!(request["input_tokens"], "10");
    assert_eq!(request["cache_read_tokens"], "100");
    assert!(request.contains_key("duration_ms"));

    let decisions: Vec<_> = (0..4)
        .map(|i| {
            let d = out.event("loom.tool_decision", i);
            (
                d["tool_name"].as_str(),
                d["decision"].as_str(),
                d["source"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        decisions,
        [
            ("edit", "accept", "user_temporary"),
            ("shell", "accept", "config"),
            ("write", "reject", "config"),
            ("read", "reject", "user_reject"),
        ]
    );
    let edit = out.event("loom.tool_result", 0);
    assert_eq!(edit["tool_use_id"], "c1");
    assert_eq!(edit["success"], "true");
    assert_eq!(edit["decision_source"], "user_temporary");
    assert!(!edit.contains_key("tool_parameters"));
    assert_eq!(
        out.event("loom.tool_result", 1)["decision_source"],
        "config"
    );

    let error = out.event("loom.api_error", 0);
    assert_eq!(error["error"], "rate limited");
    assert_eq!(error["model"], "claude-x");

    let sequences: Vec<u64> = out
        .events
        .iter()
        .map(|(_, attrs)| attrs["event.sequence"].parse().unwrap())
        .collect();
    assert_eq!(sequences, (0..10).collect::<Vec<_>>());
}

#[test]
fn opt_in_content_and_prefix() {
    let config = TelemetryConfig {
        prefix: "claude_code".into(),
        log_user_prompts: true,
        log_assistant_responses: true,
        log_tool_details: true,
        content_max_length: 12,
        ..enabled()
    };
    let out = run(config, turn);
    assert_eq!(out.value("claude_code.commit.count", &[]), 1.0);
    assert_eq!(out.value("loom.commit.count", &[]), 0.0);
    assert_eq!(
        out.event("claude_code.user_prompt", 0)["prompt"],
        "fix the bug"
    );
    // The limit is shorter than the truncation marker, so only its start is left.
    assert_eq!(
        out.event("claude_code.assistant_response", 0)["response"],
        "…[TRUNCATED:"
    );
    let shell = out.event("claude_code.tool_result", 1);
    let parameters: serde_json::Value = serde_json::from_str(&shell["tool_parameters"]).unwrap();
    assert_eq!(parameters["bash_command"], "git");
    assert_eq!(parameters["git_commit"], true);
    assert!(shell["tool_input"].contains("gh pr create"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exports_over_otlp_http() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let config = TelemetryConfig {
        protocol: Some(OtlpProtocol::HttpJson),
        endpoint: Some(format!("{}/otlp/", server.uri())),
        headers: [("authorization".to_owned(), "Bearer t".to_owned())].into(),
        ..enabled()
    };
    let telemetry = Telemetry::start(&config, &identity()).unwrap().unwrap();
    turn(telemetry.observer().as_ref());
    tokio::task::spawn_blocking(move || {
        telemetry.flush();
        telemetry.shutdown();
    })
    .await
    .unwrap();

    let requests = server.received_requests().await.unwrap();
    let body = |path: &str| {
        requests
            .iter()
            .filter(|r| r.url.path() == path)
            .map(|r| {
                assert_eq!(r.headers.get("authorization").unwrap(), "Bearer t");
                String::from_utf8_lossy(&r.body).into_owned()
            })
            .collect::<String>()
    };
    let metrics = body("/otlp/v1/metrics");
    assert!(metrics.contains("loom.token.usage"), "{metrics}");
    assert!(metrics.contains("loom-agent"), "{metrics}");
    let logs = body("/otlp/v1/logs");
    assert!(logs.contains("loom.tool_result"), "{logs}");
}

#[test]
fn disabled_by_default() {
    assert!(
        Telemetry::start(&TelemetryConfig::default(), &identity())
            .unwrap()
            .is_none()
    );
}

#[test]
fn rejects_unknown_protocols() {
    let config = TelemetryConfig {
        headers: [("bad header".to_owned(), "x".to_owned())].into(),
        protocol: Some(OtlpProtocol::Grpc),
        ..enabled()
    };
    assert!(matches!(
        Telemetry::start(&config, &identity()),
        Err(TelemetryError::Config(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_exporter_starts_and_stops_without_a_collector() {
    let config = TelemetryConfig {
        protocol: Some(OtlpProtocol::Grpc),
        endpoint: Some("http://127.0.0.1:9".into()),
        headers: [("authorization".to_owned(), "Bearer t".to_owned())].into(),
        ..enabled()
    };
    let telemetry = Telemetry::start(&config, &identity()).unwrap().unwrap();
    tokio::task::spawn_blocking(move || telemetry.shutdown())
        .await
        .unwrap();
}
