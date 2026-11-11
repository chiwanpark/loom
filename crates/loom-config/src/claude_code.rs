//! Telemetry settings taken from Claude Code's settings.
//!
//! An organization configures Claude Code's OpenTelemetry export with
//! environment variables in the `env` block of its managed settings. This
//! turns those variables into a [`TelemetryConfig`], noting what Loom cannot
//! represent.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::agent::{OtlpProtocol, TelemetryConfig, Temporality};
use crate::error::ConfigError;
use crate::units::HumanDuration;

const ORIGIN: &str = "Claude Code settings";

/// Keys Loom reads; the rest of the telemetry keys are reported as ignored.
const ENABLE: &str = "CLAUDE_CODE_ENABLE_TELEMETRY";
const CONTENT_MAX_LENGTH: &str = "CLAUDE_CODE_OTEL_CONTENT_MAX_LENGTH";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedTelemetry {
    pub config: TelemetryConfig,
    /// Settings that were ignored or changed, one sentence each.
    pub notes: Vec<String>,
}

impl ImportedTelemetry {
    /// The `telemetry:` section of `agent.yaml`.
    pub fn to_yaml(&self) -> String {
        #[derive(Serialize)]
        struct Section<'a> {
            telemetry: &'a TelemetryConfig,
        }
        serde_saphyr::to_string(&Section {
            telemetry: &self.config,
        })
        .unwrap_or_default()
    }
}

fn is_telemetry_key(key: &str) -> bool {
    key == ENABLE || key.starts_with("OTEL_") || key.starts_with("CLAUDE_CODE_OTEL_")
}

/// The telemetry variables in the `env` block of Claude Code settings.
/// Other variables, which may hold unrelated secrets, are left out.
fn telemetry_env(settings: &Map<String, Value>) -> BTreeMap<String, String> {
    let Some(Value::Object(env)) = settings.get("env") else {
        return BTreeMap::new();
    };
    env.iter()
        .filter(|(key, _)| is_telemetry_key(key))
        .filter_map(|(key, value)| {
            let value = match value {
                Value::String(s) => s.trim().to_owned(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some((key.clone(), value))
        })
        .collect()
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_positive(value: &str) -> Option<u64> {
    value.parse::<u64>().ok().filter(|n| *n > 0)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_owned())
}

/// `key=value,key2=value2`, as in `OTEL_EXPORTER_OTLP_HEADERS`.
fn parse_pairs(value: &str) -> BTreeMap<String, String> {
    value
        .split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let key = key.trim();
            (!key.is_empty()).then(|| (key.to_owned(), percent_decode(value.trim())))
        })
        .collect()
}

/// Whether an `OTEL_*_EXPORTER` list asks for OTLP export. Other exporters
/// (`console`, `prometheus`) are noted.
fn wants_otlp(key: &str, value: &str, notes: &mut Vec<String>) -> bool {
    let entries: Vec<String> = value
        .split(',')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    let others: Vec<&str> = entries
        .iter()
        .map(String::as_str)
        .filter(|e| *e != "otlp" && *e != "none")
        .collect();
    if !others.is_empty() {
        notes.push(format!(
            "{key}: Loom exports only over OTLP, so {} is ignored",
            others.join(", ")
        ));
    }
    !entries.iter().any(|e| e == "none") && entries.iter().any(|e| e == "otlp")
}

/// Turns the telemetry variables of Claude Code settings into Loom's
/// telemetry section. `None` when the settings configure no telemetry.
pub fn telemetry_from_claude_code(
    settings: &Map<String, Value>,
) -> Result<Option<ImportedTelemetry>, ConfigError> {
    let mut env = telemetry_env(settings);
    if env.is_empty() {
        return Ok(None);
    }
    let mut config = TelemetryConfig::default();
    let mut notes = Vec::new();
    let mut take = |key: &str| env.remove(key);

    let enabled = take(ENABLE);
    match enabled.as_deref().map(|v| (v, parse_bool(v))) {
        Some((_, Some(enabled))) => config.enabled = enabled,
        Some((value, None)) => notes.push(format!(
            "{ENABLE}: {value:?} is not a boolean; telemetry stays disabled"
        )),
        None => notes.push(format!(
            "{ENABLE} is not set, so Claude Code exports nothing; telemetry stays disabled"
        )),
    }

    // Claude Code exports a signal only when its exporter names otlp.
    let mut exporter = |key: &str, notes: &mut Vec<String>| match take(key) {
        Some(value) => wants_otlp(key, &value, notes),
        None => {
            notes.push(format!(
                "{key} is not set, so Claude Code does not export them"
            ));
            false
        }
    };
    config.metrics = exporter("OTEL_METRICS_EXPORTER", &mut notes);
    config.logs = exporter("OTEL_LOGS_EXPORTER", &mut notes);

    let mut take = |key: &str| env.remove(key);
    if let Some(value) = take("OTEL_EXPORTER_OTLP_PROTOCOL") {
        match value.parse::<OtlpProtocol>() {
            Ok(protocol) => config.protocol = Some(protocol),
            Err(err) => notes.push(format!("OTEL_EXPORTER_OTLP_PROTOCOL: {err}")),
        }
    }
    config.endpoint = take("OTEL_EXPORTER_OTLP_ENDPOINT").filter(|e| !e.is_empty());
    if let Some(value) = take("OTEL_EXPORTER_OTLP_HEADERS") {
        config.headers = parse_pairs(&value);
    }
    let mut interval = |key: &str, target: &mut HumanDuration, notes: &mut Vec<String>| {
        if let Some(value) = take(key) {
            match parse_positive(&value) {
                Some(ms) => *target = HumanDuration(Duration::from_millis(ms)),
                None => notes.push(format!(
                    "{key}: {value:?} is not a positive number of milliseconds"
                )),
            }
        }
    };
    interval(
        "OTEL_METRIC_EXPORT_INTERVAL",
        &mut config.metrics_interval,
        &mut notes,
    );
    interval(
        "OTEL_LOGS_EXPORT_INTERVAL",
        &mut config.logs_interval,
        &mut notes,
    );

    let mut take = |key: &str| env.remove(key);
    let temporality_key = "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE";
    if let Some(value) = take(temporality_key) {
        match value.to_ascii_lowercase().as_str() {
            "cumulative" => config.temporality = Temporality::Cumulative,
            "delta" => config.temporality = Temporality::Delta,
            _ => notes.push(format!(
                "{temporality_key}: {value:?} is not supported; using cumulative"
            )),
        }
    }
    if let Some(value) = take("OTEL_SERVICE_NAME").filter(|v| !v.is_empty()) {
        config.service_name = value;
    }
    if let Some(value) = take("OTEL_RESOURCE_ATTRIBUTES") {
        config.resource_attributes = parse_pairs(&value);
    }

    let mut flag = |key: &str, notes: &mut Vec<String>| {
        let value = take(key)?;
        let parsed = parse_bool(&value);
        if parsed.is_none() {
            notes.push(format!("{key}: {value:?} is not a boolean"));
        }
        parsed
    };
    let prompts = flag("OTEL_LOG_USER_PROMPTS", &mut notes);
    config.log_user_prompts = prompts.unwrap_or(false);
    // Claude Code logs responses with prompts unless told otherwise.
    config.log_assistant_responses = flag("OTEL_LOG_ASSISTANT_RESPONSES", &mut notes)
        .or(prompts)
        .unwrap_or(false);
    config.log_tool_details = flag("OTEL_LOG_TOOL_DETAILS", &mut notes).unwrap_or(false);

    if let Some(value) = env.remove(CONTENT_MAX_LENGTH) {
        match parse_positive(&value).and_then(|n| usize::try_from(n).ok()) {
            Some(n) => config.content_max_length = n,
            None => notes.push(format!(
                "{CONTENT_MAX_LENGTH}: {value:?} is not a positive number"
            )),
        }
    }

    // Everything left is something Loom has no setting for: per-signal
    // endpoints, the headers helper script, Prometheus, attribute toggles.
    for (key, value) in env {
        // Header values carry credentials.
        let value = if key.ends_with("_HEADERS") {
            "…".to_owned()
        } else {
            format!("{value:?}")
        };
        notes.push(format!("{key}={value} is not supported and is ignored"));
    }

    config.validate(ORIGIN)?;
    Ok(Some(ImportedTelemetry { config, notes }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::AgentConfig;

    fn import(env: Value) -> Result<Option<ImportedTelemetry>, ConfigError> {
        let Value::Object(settings) = json!({"env": env, "permissions": {"deny": []}}) else {
            unreachable!()
        };
        telemetry_from_claude_code(&settings)
    }

    #[test]
    fn imports_a_typical_organization_setup() {
        let imported = import(json!({
            "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
            "OTEL_METRICS_EXPORTER": "otlp",
            "OTEL_LOGS_EXPORTER": "otlp,console",
            "OTEL_EXPORTER_OTLP_PROTOCOL": "http/json",
            "OTEL_EXPORTER_OTLP_ENDPOINT": "https://otel.example.com:4318",
            "OTEL_EXPORTER_OTLP_HEADERS": "Authorization=Bearer%20secret, x-team=ml",
            "OTEL_METRIC_EXPORT_INTERVAL": 10000,
            "OTEL_LOGS_EXPORT_INTERVAL": "2500",
            "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE": "delta",
            "OTEL_RESOURCE_ATTRIBUTES": "department=eng,cost_center=42",
            "OTEL_LOG_USER_PROMPTS": true,
            "OTEL_LOG_TOOL_DETAILS": "0",
            "CLAUDE_CODE_OTEL_CONTENT_MAX_LENGTH": "1000",
            "OTEL_METRICS_INCLUDE_SESSION_ID": "false",
            "ANTHROPIC_API_KEY": "not telemetry",
        }))
        .unwrap()
        .unwrap();
        let config = &imported.config;
        assert!(config.enabled && config.metrics && config.logs);
        assert_eq!(config.protocol, Some(OtlpProtocol::HttpJson));
        assert_eq!(
            config.endpoint.as_deref(),
            Some("https://otel.example.com:4318")
        );
        assert_eq!(
            config.headers,
            BTreeMap::from([
                ("Authorization".into(), "Bearer secret".into()),
                ("x-team".into(), "ml".into()),
            ])
        );
        assert_eq!(
            config.metrics_interval.as_duration(),
            Duration::from_secs(10)
        );
        assert_eq!(
            config.logs_interval.as_duration(),
            Duration::from_millis(2500)
        );
        assert_eq!(config.temporality, Temporality::Delta);
        assert_eq!(config.resource_attributes["cost_center"], "42");
        assert!(config.log_user_prompts && config.log_assistant_responses);
        assert!(!config.log_tool_details);
        assert_eq!(config.content_max_length, 1000);
        assert_eq!(config.prefix, "loom");
        assert_eq!(config.service_name, "loom-agent");
        assert_eq!(
            imported.notes,
            vec![
                "OTEL_LOGS_EXPORTER: Loom exports only over OTLP, so console is ignored",
                "OTEL_METRICS_INCLUDE_SESSION_ID=\"false\" is not supported and is ignored",
            ]
        );

        let yaml = imported.to_yaml();
        assert!(yaml.starts_with("telemetry:\n"), "{yaml}");
        assert!(!yaml.contains("not telemetry"), "{yaml}");
        let back: AgentConfig = serde_saphyr::from_str(&yaml).unwrap();
        assert_eq!(&back.telemetry, config);
    }

    #[test]
    fn missing_switches_leave_telemetry_off() {
        let imported = import(json!({
            "OTEL_EXPORTER_OTLP_ENDPOINT": "http://collector:4318",
            "OTEL_METRICS_EXPORTER": "none",
            "OTEL_EXPORTER_OTLP_LOGS_HEADERS": "authorization=secret",
        }))
        .unwrap()
        .unwrap();
        assert!(!imported.config.enabled);
        assert!(!imported.config.metrics && !imported.config.logs);
        assert_eq!(
            imported.notes,
            vec![
                "CLAUDE_CODE_ENABLE_TELEMETRY is not set, so Claude Code exports nothing; telemetry stays disabled",
                "OTEL_LOGS_EXPORTER is not set, so Claude Code does not export them",
                "OTEL_EXPORTER_OTLP_LOGS_HEADERS=… is not supported and is ignored",
            ]
        );
    }

    #[test]
    fn settings_without_telemetry_import_nothing() {
        assert_eq!(import(json!({"ANTHROPIC_MODEL": "x"})).unwrap(), None);
        assert_eq!(telemetry_from_claude_code(&Map::new()).unwrap(), None);
    }

    #[test]
    fn invalid_values_are_rejected_or_noted() {
        let error = import(json!({
            "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
            "OTEL_EXPORTER_OTLP_ENDPOINT": "otel.example.com:4317",
        }))
        .unwrap_err();
        assert!(error.to_string().contains("telemetry.endpoint"), "{error}");

        let imported = import(json!({
            "CLAUDE_CODE_ENABLE_TELEMETRY": "maybe",
            "OTEL_METRICS_EXPORTER": "otlp",
            "OTEL_LOGS_EXPORTER": "otlp",
            "OTEL_EXPORTER_OTLP_PROTOCOL": "carrier-pigeon",
            "OTEL_METRIC_EXPORT_INTERVAL": "0",
        }))
        .unwrap()
        .unwrap();
        assert!(!imported.config.enabled);
        assert_eq!(imported.config.protocol, None);
        assert_eq!(imported.notes.len(), 3, "{:?}", imported.notes);
    }
}
