//! What the status bar shows of an agent's OpenTelemetry export.

use loom_client::proto::{SignalStatus, TelemetryStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Telemetry is off.
    Off,
    /// On, but nothing has been exported yet.
    Waiting,
    /// The last export of every signal that exported succeeded.
    Healthy,
    /// The last export of a signal failed, or the exporter did not start.
    Failing,
    /// The status could not be fetched, e.g. from an older agent.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub health: Health,
    pub label: &'static str,
    pub tooltip: String,
}

const RESTART: &str = "Telemetry settings apply when the agent restarts.";

pub fn summarize(status: &TelemetryStatus, now: u64) -> Summary {
    match status {
        TelemetryStatus::Off => Summary {
            health: Health::Off,
            label: "OTEL off",
            tooltip: format!(
                "OpenTelemetry export is off. Turn it on with telemetry.enabled in \
                 the agent's config.\n{RESTART}"
            ),
        },
        TelemetryStatus::Failed { error } => Summary {
            health: Health::Failing,
            label: "OTEL not started",
            tooltip: format!("OpenTelemetry export could not start: {error}\n{RESTART}"),
        },
        TelemetryStatus::Exporting {
            endpoint,
            protocol,
            signals,
        } => {
            let target = match endpoint {
                Some(url) => url.clone(),
                None => "the collector from OTEL_EXPORTER_OTLP_ENDPOINT, or localhost".into(),
            };
            let mut lines = vec![format!("OpenTelemetry export to {target} ({protocol})")];
            lines.extend(signals.iter().map(|s| signal_line(s, now)));
            lines.push(RESTART.into());
            let (health, label) = if signals.iter().any(|s| s.consecutive_failures > 0) {
                (Health::Failing, "OTEL failing")
            } else if signals.iter().all(|s| s.exports == 0) {
                (Health::Waiting, "OTEL waiting")
            } else {
                (Health::Healthy, "OTEL")
            };
            Summary {
                health,
                label,
                tooltip: lines.join("\n"),
            }
        }
    }
}

/// When asking the agent failed.
pub fn unknown(error: &str) -> Summary {
    Summary {
        health: Health::Unknown,
        label: "OTEL unknown",
        tooltip: format!("Could not get the telemetry status: {error}"),
    }
}

fn signal_line(signal: &SignalStatus, now: u64) -> String {
    let name = &signal.signal;
    if signal.exports == 0 {
        return format!("{name}: nothing exported yet");
    }
    let last_success = match signal.last_success_at {
        Some(at) => format!("last success {}", ago(now.saturating_sub(at))),
        None => "no success yet".into(),
    };
    match (&signal.last_failure, signal.consecutive_failures) {
        (Some(failure), failed) if failed > 0 => {
            let times = if failed == 1 {
                "once".to_owned()
            } else {
                format!("{failed} times in a row")
            };
            format!(
                "{name}: failed {times}, {} ({last_success}): {}",
                ago(now.saturating_sub(failure.at)),
                failure.error
            )
        }
        _ => {
            let failures = match signal.failures {
                0 => String::new(),
                1 => ", 1 failure earlier".into(),
                n => format!(", {n} failures earlier"),
            };
            format!(
                "{name}: {last_success}, {} export{}{failures}",
                signal.exports,
                if signal.exports == 1 { "" } else { "s" }
            )
        }
    }
}

fn ago(seconds: u64) -> String {
    match seconds {
        0..5 => "just now".into(),
        5..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use loom_client::proto::ExportFailure;

    use super::*;

    const NOW: u64 = 1_000_000;

    fn signal(name: &str, exports: u64, failures: u64, consecutive: u64) -> SignalStatus {
        SignalStatus {
            signal: name.into(),
            exports,
            failures,
            consecutive_failures: consecutive,
            last_success_at: (exports > failures).then_some(NOW - 120),
            last_failure: (failures > 0).then(|| ExportFailure {
                at: NOW - 30,
                error: "HTTP export failed with status code: 503".into(),
            }),
        }
    }

    fn exporting(signals: Vec<SignalStatus>) -> TelemetryStatus {
        TelemetryStatus::Exporting {
            endpoint: Some("http://collector:4318".into()),
            protocol: "http/protobuf".into(),
            signals,
        }
    }

    #[test]
    fn off_and_start_failures() {
        let off = summarize(&TelemetryStatus::Off, NOW);
        assert_eq!((off.health, off.label), (Health::Off, "OTEL off"));
        let failed = summarize(
            &TelemetryStatus::Failed {
                error: "bad protocol".into(),
            },
            NOW,
        );
        assert_eq!(failed.health, Health::Failing);
        assert!(
            failed.tooltip.contains("bad protocol"),
            "{}",
            failed.tooltip
        );
        assert_eq!(unknown("old agent").health, Health::Unknown);
    }

    #[test]
    fn waiting_until_something_is_exported() {
        let summary = summarize(
            &exporting(vec![signal("metrics", 0, 0, 0), signal("logs", 0, 0, 0)]),
            NOW,
        );
        assert_eq!(summary.health, Health::Waiting);
        assert_eq!(
            summary.tooltip,
            "OpenTelemetry export to http://collector:4318 (http/protobuf)\n\
             metrics: nothing exported yet\n\
             logs: nothing exported yet\n\
             Telemetry settings apply when the agent restarts."
        );
    }

    #[test]
    fn healthy_after_recovering_from_failures() {
        let summary = summarize(
            &exporting(vec![signal("metrics", 0, 0, 0), signal("logs", 5, 2, 0)]),
            NOW,
        );
        assert_eq!((summary.health, summary.label), (Health::Healthy, "OTEL"));
        assert!(
            summary
                .tooltip
                .contains("logs: last success 2m ago, 5 exports, 2 failures earlier"),
            "{}",
            summary.tooltip
        );
    }

    #[test]
    fn failing_when_the_last_export_of_a_signal_failed() {
        let summary = summarize(
            &exporting(vec![signal("metrics", 3, 0, 0), signal("logs", 4, 3, 3)]),
            NOW,
        );
        assert_eq!(summary.health, Health::Failing);
        assert!(
            summary.tooltip.contains(
                "logs: failed 3 times in a row, 30s ago (last success 2m ago): \
                 HTTP export failed with status code: 503"
            ),
            "{}",
            summary.tooltip
        );
        let never = summarize(&exporting(vec![signal("logs", 1, 1, 1)]), NOW);
        assert!(
            never
                .tooltip
                .contains("logs: failed once, 30s ago (no success yet)"),
            "{}",
            never.tooltip
        );
    }

    #[test]
    fn unconfigured_endpoints_come_from_the_environment() {
        let status = TelemetryStatus::Exporting {
            endpoint: None,
            protocol: "grpc".into(),
            signals: Vec::new(),
        };
        assert!(
            summarize(&status, NOW).tooltip.starts_with(
                "OpenTelemetry export to the collector from OTEL_EXPORTER_OTLP_ENDPOINT"
            )
        );
    }

    #[test]
    fn ages() {
        assert_eq!(ago(3), "just now");
        assert_eq!(ago(42), "42s ago");
        assert_eq!(ago(150), "2m ago");
        assert_eq!(ago(7200), "2h ago");
        assert_eq!(ago(200_000), "2d ago");
    }
}
