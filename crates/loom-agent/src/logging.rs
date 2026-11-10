//! Process-wide logging with a reloadable level filter.
//!
//! `logging.level` in `agent.yaml` is hot-reloadable, so `serve` installs the
//! filter behind a `reload` layer and follows configuration reloads. When
//! `LOOM_LOG` is set it always wins, both at startup and on reload.

use std::sync::OnceLock;

use loom_agent_core::{Agent, AgentNotice};
use loom_config::LogLevel;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry, reload};

pub const LOG_ENV: &str = "LOOM_LOG";

static HANDLE: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

pub fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    }
}

fn env_overrides() -> bool {
    std::env::var_os(LOG_ENV).is_some()
}

/// Installs the global subscriber on stderr. `level` applies unless `LOOM_LOG` is set.
pub fn init(level: &str) {
    let filter = EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new(level));
    let (filter, handle) = reload::Layer::new(filter);
    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()
        .is_ok();
    if installed {
        let _ = HANDLE.set(handle);
    }
}

/// Applies `level` to the running subscriber. Returns `false` when nothing changed
/// because `LOOM_LOG` takes precedence or logging was not set up through [`init`].
pub fn set_level(level: LogLevel) -> bool {
    if env_overrides() {
        return false;
    }
    let Some(handle) = HANDLE.get() else {
        return false;
    };
    handle.reload(EnvFilter::new(level_name(level))).is_ok()
}

/// Follows configuration reloads and applies `logging.level` each time.
pub fn follow_config(agent: &Agent) -> JoinHandle<()> {
    let agent = agent.clone();
    let mut notices = agent.notices();
    tokio::spawn(async move {
        let mut current = agent.config().logging.level;
        loop {
            match notices.recv().await {
                Ok(AgentNotice::ConfigReloaded { .. }) | Err(RecvError::Lagged(_)) => {
                    let next = agent.config().logging.level;
                    if next != current {
                        current = next;
                        if set_level(next) {
                            tracing::info!(level = level_name(next), "log level changed");
                        }
                    }
                }
                Ok(_) => {}
                Err(RecvError::Closed) => break,
            }
        }
    })
}
