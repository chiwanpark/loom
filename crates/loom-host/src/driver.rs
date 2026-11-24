use std::path::PathBuf;

use async_trait::async_trait;
use loom_client::oracle_proto::{CreateInstance, InstanceReport, VolumeUsage};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

pub type Input = Box<dyn AsyncRead + Unpin + Send>;
pub type Output = Box<dyn AsyncWrite + Unpin + Send>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriverReport {
    pub driver: String,
    pub cpus: u32,
    pub memory_mib: u64,
    pub architecture: Option<String>,
    pub storage_driver: Option<String>,
    pub incus_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warning,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckItem {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

impl CheckItem {
    pub fn new(name: &str, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            status,
            detail: detail.into(),
        }
    }
}

#[derive(Clone)]
pub struct Progress(Option<mpsc::UnboundedSender<String>>);

impl Progress {
    pub fn new(tx: mpsc::UnboundedSender<String>) -> Self {
        Self(Some(tx))
    }

    pub fn none() -> Self {
        Self(None)
    }

    pub fn step(&self, step: &str) {
        tracing::info!(step, "codespace step");
        if let Some(tx) = &self.0 {
            let _ = tx.send(step.to_owned());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub path: PathBuf,
    pub version: Option<String>,
}

#[async_trait]
pub trait Driver: Send + Sync {
    async fn report(&self) -> anyhow::Result<DriverReport>;

    async fn create(
        &self,
        request: &CreateInstance,
        oracle: &str,
        agent: &Agent,
        progress: &Progress,
    ) -> anyhow::Result<()>;

    async fn delete(&self, oracle: &str, instance: &str) -> anyhow::Result<()>;

    async fn stop(&self, oracle: &str, instance: &str) -> anyhow::Result<()>;

    async fn start(&self, oracle: &str, instance: &str) -> anyhow::Result<()>;

    async fn stdio(
        &self,
        oracle: &str,
        instance: &str,
        input: Input,
        output: Output,
        errors: Output,
    ) -> anyhow::Result<i32>;

    async fn list(&self, oracle: &str) -> anyhow::Result<Vec<InstanceReport>>;

    async fn check(&self, deep: bool) -> Vec<CheckItem>;

    fn local(&self) -> bool {
        true
    }

    async fn delete_volume(&self, oracle: &str, volume: &str) -> anyhow::Result<()>;

    async fn volume_usage(&self, oracle: &str, volumes: &[String]) -> Vec<VolumeUsage>;
}

pub fn valid_instance(name: &str) -> bool {
    name.strip_prefix("loom-").is_some_and(|id| {
        !id.is_empty()
            && id.len() <= 40
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

pub fn memory_mib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("MemTotal:"))
                .and_then(|rest| {
                    rest.trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse::<u64>()
                        .ok()
                })
        })
        .map(|kib| kib / 1024)
        .unwrap_or_default()
}

pub fn cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instances_have_the_loom_prefix() {
        assert!(valid_instance("loom-abc123"));
        for name in ["abc", "loom-", "loom-ABC", "loom-a/b", "other-abc"] {
            assert!(!valid_instance(name), "{name}");
        }
    }

    #[test]
    fn the_machine_has_cpus_and_memory() {
        assert!(cpus() >= 1);
        assert!(memory_mib() > 0);
    }
}
