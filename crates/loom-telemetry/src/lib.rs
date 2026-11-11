//! OpenTelemetry export of Loom session activity: metrics and log events over
//! OTLP, modelled on Claude Code's telemetry and the pi-enhanced exporter.
//!
//! Metrics (`<prefix>.*`): `session.count`, `token.usage`,
//! `lines_of_code.count`, `commit.count`, `pull_request.count`,
//! `code_edit_tool.decision` and `active_time.total`. Loom has no price list,
//! so there is no `cost.usage`.
//!
//! Events (log records whose body is `<prefix>.<name>`): `user_prompt`,
//! `assistant_response`, `api_request`, `api_error`, `tool_decision` and
//! `tool_result`. Prompt, response and tool contents are redacted unless the
//! configuration opts in.

mod mappers;
mod recorder;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use loom_agent_core::SessionObserver;
use loom_config::{OtlpProtocol, TelemetryConfig, Temporality};
use opentelemetry::KeyValue;
use opentelemetry::logs::LoggerProvider as _;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_otlp::{
    LogExporter, MetricExporter, Protocol, WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::{
    BatchConfigBuilder, BatchLogProcessor, LogExporter as LogExport, SdkLoggerProvider,
};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

pub use recorder::REDACTED;

use recorder::{Counters, Recorder};

const SCOPE: &str = "loom";
const PROTOCOL_ENV: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("{0}")]
    Config(String),
    #[error("could not create the {signal} exporter: {source}")]
    Exporter {
        signal: &'static str,
        source: opentelemetry_otlp::ExporterBuildError,
    },
}

/// Facts about the agent that go into the OpenTelemetry resource.
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    pub version: String,
    pub instance: String,
    pub workspace: std::path::PathBuf,
}

/// A running exporter. Drop it or call [`Telemetry::shutdown`] to flush.
pub struct Telemetry {
    meters: Option<SdkMeterProvider>,
    logs: Option<SdkLoggerProvider>,
    recorder: Arc<Recorder>,
}

fn protocol(config: &TelemetryConfig) -> Result<OtlpProtocol, TelemetryError> {
    if let Some(protocol) = config.protocol {
        return Ok(protocol);
    }
    match std::env::var(PROTOCOL_ENV) {
        Ok(value) if !value.trim().is_empty() => value
            .parse()
            .map_err(|err| TelemetryError::Config(format!("{PROTOCOL_ENV}: {err}"))),
        _ => Ok(OtlpProtocol::HttpProtobuf),
    }
}

/// The URL of one signal: over HTTP the configured base gets the signal's
/// path; gRPC uses the base as it is.
fn signal_endpoint(base: &str, protocol: OtlpProtocol, path: &str) -> String {
    match protocol {
        OtlpProtocol::Grpc => base.to_owned(),
        _ => format!("{}/{path}", base.trim_end_matches('/')),
    }
}

fn metadata(
    headers: &HashMap<String, String>,
) -> Result<opentelemetry_otlp::tonic_types::metadata::MetadataMap, TelemetryError> {
    let mut map = http::HeaderMap::new();
    for (name, value) in headers {
        let invalid = |what: &str| {
            TelemetryError::Config(format!("telemetry.headers: invalid header {what} {name:?}"))
        };
        map.insert(
            http::HeaderName::try_from(name.as_str()).map_err(|_| invalid("name"))?,
            http::HeaderValue::try_from(value.as_str()).map_err(|_| invalid("value for"))?,
        );
    }
    Ok(opentelemetry_otlp::tonic_types::metadata::MetadataMap::from_headers(map))
}

macro_rules! exporter {
    ($builder:expr, $config:expr, $protocol:expr, $path:literal, $signal:literal) => {{
        let config: &TelemetryConfig = $config;
        let headers: HashMap<String, String> = config.headers.clone().into_iter().collect();
        let endpoint = config
            .endpoint
            .as_deref()
            .map(|base| signal_endpoint(base, $protocol, $path));
        let built = match $protocol {
            OtlpProtocol::Grpc => {
                let mut builder = $builder.with_tonic().with_metadata(metadata(&headers)?);
                if let Some(endpoint) = endpoint {
                    builder = builder.with_endpoint(endpoint);
                }
                builder.build()
            }
            protocol => {
                let mut builder = $builder
                    .with_http()
                    .with_protocol(if protocol == OtlpProtocol::HttpJson {
                        Protocol::HttpJson
                    } else {
                        Protocol::HttpBinary
                    })
                    .with_headers(headers);
                if let Some(endpoint) = endpoint {
                    builder = builder.with_endpoint(endpoint);
                }
                builder.build()
            }
        };
        built.map_err(|source| TelemetryError::Exporter {
            signal: $signal,
            source,
        })?
    }};
}

impl Telemetry {
    /// Starts exporting as `config` says, or returns `None` when telemetry is
    /// disabled. gRPC export needs to start inside a Tokio runtime.
    pub fn start(
        config: &TelemetryConfig,
        identity: &AgentIdentity,
    ) -> Result<Option<Self>, TelemetryError> {
        if !config.enabled || !(config.metrics || config.logs) {
            return Ok(None);
        }
        let protocol = protocol(config)?;
        let metrics = if config.metrics {
            let temporality = match config.temporality {
                Temporality::Cumulative => opentelemetry_sdk::metrics::Temporality::Cumulative,
                Temporality::Delta => opentelemetry_sdk::metrics::Temporality::Delta,
            };
            Some(exporter!(
                MetricExporter::builder().with_temporality(temporality),
                config,
                protocol,
                "v1/metrics",
                "metrics"
            ))
        } else {
            None
        };
        let logs = if config.logs {
            Some(exporter!(
                LogExporter::builder(),
                config,
                protocol,
                "v1/logs",
                "logs"
            ))
        } else {
            None
        };
        Ok(Some(Self::with_exporters(config, identity, metrics, logs)))
    }

    /// Builds the providers around the given exporters; tests pass in-memory
    /// ones.
    pub fn with_exporters<M, L>(
        config: &TelemetryConfig,
        identity: &AgentIdentity,
        metrics: Option<M>,
        logs: Option<L>,
    ) -> Self
    where
        M: PushMetricExporter,
        L: LogExport + 'static,
    {
        let resource = resource(config, identity);
        let meters = metrics.map(|exporter| {
            SdkMeterProvider::builder()
                .with_resource(resource.clone())
                .with_reader(
                    PeriodicReader::builder(exporter)
                        .with_interval(config.metrics_interval.as_duration())
                        .build(),
                )
                .build()
        });
        let logs = logs.map(|exporter| {
            SdkLoggerProvider::builder()
                .with_resource(resource)
                .with_log_processor(
                    BatchLogProcessor::builder(exporter)
                        .with_batch_config(
                            BatchConfigBuilder::default()
                                .with_scheduled_delay(config.logs_interval.as_duration())
                                .build(),
                        )
                        .build(),
                )
                .build()
        });
        let counters = meters
            .as_ref()
            .map(|provider| Counters::new(&provider.meter(SCOPE), &config.prefix));
        let logger = logs.as_ref().map(|provider| provider.logger(SCOPE));
        let recorder = Recorder::new(config.clone(), identity.version.clone(), counters, logger);
        Self {
            meters,
            logs,
            recorder: Arc::new(recorder),
        }
    }

    /// The observer to give the agent.
    pub fn observer(&self) -> Arc<dyn SessionObserver> {
        self.recorder.clone()
    }

    /// Exports everything recorded so far. Blocks until the exporters finish.
    pub fn flush(&self) {
        if let Some(meters) = &self.meters
            && let Err(err) = meters.force_flush()
        {
            tracing::warn!(error = %err, "could not export metrics");
        }
        if let Some(logs) = &self.logs
            && let Err(err) = logs.force_flush()
        {
            tracing::warn!(error = %err, "could not export telemetry events");
        }
    }

    /// Flushes and stops the exporters. Blocks until they finish.
    pub fn shutdown(self) {
        if let Some(meters) = &self.meters
            && let Err(err) = meters.shutdown()
        {
            tracing::warn!(error = %err, "could not stop the metrics exporter");
        }
        if let Some(logs) = &self.logs
            && let Err(err) = logs.shutdown()
        {
            tracing::warn!(error = %err, "could not stop the telemetry events exporter");
        }
    }
}

fn resource(config: &TelemetryConfig, identity: &AgentIdentity) -> Resource {
    // `Resource::builder` reads OTEL_RESOURCE_ATTRIBUTES; the configured
    // attributes come last so they win.
    Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attributes([
            KeyValue::new("service.version", identity.version.clone()),
            KeyValue::new("service.instance.id", identity.instance.clone()),
            KeyValue::new("loom.workspace", workspace_label(&identity.workspace)),
        ])
        .with_attributes(
            config
                .resource_attributes
                .iter()
                .map(|(k, v)| KeyValue::new(k.clone(), v.clone())),
        )
        .build()
}

fn workspace_label(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests;
