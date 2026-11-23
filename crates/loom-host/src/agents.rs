use std::path::PathBuf;

use anyhow::{Context, bail};
use async_trait::async_trait;
use loom_update::{Package, Updater, Version};

use crate::driver::Agent;

#[async_trait]
pub trait AgentSource: Send + Sync {
    async fn agent(
        &self,
        version: Option<&str>,
        architecture: Option<&str>,
    ) -> anyhow::Result<Agent>;
}

pub struct AgentCache {
    path: Option<PathBuf>,
    dir: PathBuf,
    lock: tokio::sync::Mutex<()>,
}

impl AgentCache {
    pub fn new(path: Option<PathBuf>, dir: PathBuf) -> Self {
        Self {
            path,
            dir,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn fixed(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }
}

#[async_trait]
impl AgentSource for AgentCache {
    async fn agent(
        &self,
        version: Option<&str>,
        architecture: Option<&str>,
    ) -> anyhow::Result<Agent> {
        if let Some(path) = &self.path {
            if !path.is_file() {
                bail!("the agent binary {} is not there", path.display());
            }
            return Ok(Agent {
                path: path.clone(),
                version: None,
            });
        }
        if let Some(architecture) = architecture
            && architecture != std::env::consts::ARCH
        {
            bail!(
                "the Incus host has {architecture}, but loom-host runs on {}; set agent.path to a loom-agent for {architecture}",
                std::env::consts::ARCH
            );
        }
        let text = version.unwrap_or(env!("CARGO_PKG_VERSION"));
        let version = Version::parse(text)
            .with_context(|| format!("{text:?} is not a version of loom-agent"))?;
        let dir = self.dir.join(version.to_string());
        let path = dir.join("loom-agent");
        let agent = Agent {
            path: path.clone(),
            version: Some(version.to_string()),
        };
        if path.is_file() {
            return Ok(agent);
        }
        let _lock = self.lock.lock().await;
        if path.is_file() {
            return Ok(agent);
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("cannot make {}", dir.display()))?;
        tracing::info!(%version, "downloading loom-agent");
        let updater = Updater::new();
        let release = updater
            .release(Package::Agent, Some(&version))
            .await
            .with_context(|| format!("cannot find loom-agent {version}"))?;
        updater
            .install(Package::Agent, &release, &dir, |_, _| {})
            .await
            .with_context(|| format!("cannot download loom-agent {version}"))?;
        Ok(agent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_fixed_path_wins_and_the_cache_needs_this_architecture() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("loom-agent");
        std::fs::write(&binary, "x").unwrap();
        let fixed = AgentCache::new(Some(binary.clone()), dir.path().join("cache"));
        let agent = fixed.agent(Some("9.9.9"), Some("riscv64")).await.unwrap();
        assert_eq!(agent.path, binary);
        let missing = AgentCache::new(Some(dir.path().join("none")), dir.path().join("cache"));
        assert!(missing.agent(None, None).await.is_err());

        let cache = AgentCache::new(None, dir.path().join("cache"));
        let other = if std::env::consts::ARCH == "aarch64" {
            "x86_64"
        } else {
            "aarch64"
        };
        assert!(cache.agent(Some("1.2.3"), Some(other)).await.is_err());
        assert!(cache.agent(Some("one"), None).await.is_err());
        let cached = dir.path().join("cache/1.2.3/loom-agent");
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, "x").unwrap();
        let agent = cache
            .agent(Some("1.2.3"), Some(std::env::consts::ARCH))
            .await
            .unwrap();
        assert_eq!(agent.path, cached);
        assert_eq!(agent.version.as_deref(), Some("1.2.3"));
    }
}
