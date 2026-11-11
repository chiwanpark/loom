//! What clients see of the export: each exporter is wrapped so the outcome
//! of every export is recorded.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use loom_config::OtlpProtocol;
use loom_proto::{ExportFailure, SignalStatus, TelemetryStatus};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;

/// The longest error message kept, in characters.
const MAX_ERROR_CHARS: usize = 300;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The export results of one signal.
#[derive(Debug)]
pub(crate) struct Signal(Mutex<SignalStatus>);

impl Signal {
    pub(crate) fn new(name: &str) -> Arc<Self> {
        Arc::new(Self(Mutex::new(SignalStatus {
            signal: name.to_owned(),
            ..SignalStatus::default()
        })))
    }

    fn record(&self, result: &OTelSdkResult) {
        let mut status = self.0.lock().unwrap_or_else(|e| e.into_inner());
        status.exports += 1;
        match result {
            Ok(()) => {
                status.last_success_at = Some(now_unix());
                status.consecutive_failures = 0;
            }
            Err(err) => {
                status.failures += 1;
                status.consecutive_failures += 1;
                status.last_failure = Some(ExportFailure {
                    at: now_unix(),
                    error: err.to_string().chars().take(MAX_ERROR_CHARS).collect(),
                });
            }
        }
    }

    fn snapshot(&self) -> SignalStatus {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The settings and export results of a running exporter.
#[derive(Debug)]
pub(crate) struct Board {
    pub endpoint: Option<String>,
    pub protocol: OtlpProtocol,
    pub signals: Vec<Arc<Signal>>,
}

impl Board {
    pub(crate) fn status(&self) -> TelemetryStatus {
        TelemetryStatus::Exporting {
            endpoint: self.endpoint.as_deref().map(without_userinfo),
            protocol: protocol_label(self.protocol).to_owned(),
            signals: self.signals.iter().map(|s| s.snapshot()).collect(),
        }
    }
}

pub(crate) fn protocol_label(protocol: OtlpProtocol) -> &'static str {
    match protocol {
        OtlpProtocol::Grpc => "grpc",
        OtlpProtocol::HttpProtobuf => "http/protobuf",
        OtlpProtocol::HttpJson => "http/json",
    }
}

/// `https://user:secret@host/path` as `https://host/path`, so credentials in
/// the URL stay on the agent.
fn without_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_owned(),
    }
}

/// An exporter that records the outcome of each export in `signal`.
#[derive(Debug)]
pub(crate) struct Recorded<E> {
    pub inner: E,
    pub signal: Arc<Signal>,
}

impl<E: PushMetricExporter> PushMetricExporter for Recorded<E> {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let result = self.inner.export(metrics).await;
        self.signal.record(&result);
        result
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}

impl<E: LogExporter> LogExporter for Recorded<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let result = self.inner.export(batch).await;
        self.signal.record(&result);
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn event_enabled(
        &self,
        level: opentelemetry::logs::Severity,
        target: &str,
        name: Option<&str>,
    ) -> bool {
        self.inner.event_enabled(level, target, name)
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.inner.set_resource(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_credentials_from_urls() {
        assert_eq!(
            without_userinfo("https://user:secret@collector:4318/otlp"),
            "https://collector:4318/otlp"
        );
        assert_eq!(
            without_userinfo("http://localhost:4318/a@b"),
            "http://localhost:4318/a@b"
        );
        assert_eq!(without_userinfo("localhost:4317"), "localhost:4317");
    }

    #[test]
    fn records_successes_and_failures() {
        let failure = || {
            Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
                "x".repeat(500),
            ))
        };
        let signal = Signal::new("logs");
        signal.record(&failure());
        signal.record(&Ok(()));
        signal.record(&failure());
        signal.record(&failure());
        let status = signal.snapshot();
        assert_eq!((status.exports, status.failures), (4, 3));
        assert_eq!(status.consecutive_failures, 2);
        signal.record(&Ok(()));
        assert_eq!(signal.snapshot().consecutive_failures, 0);
        assert!(status.last_success_at.is_some());
        let failure = status.last_failure.unwrap();
        assert_eq!(failure.error.chars().count(), MAX_ERROR_CHARS);
    }
}
