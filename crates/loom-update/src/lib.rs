//! Updates Loom's binaries from the archives of its GitHub releases, as
//! `scripts/install.sh` installs them: an archive must match its checksum in
//! the release's `SHA256SUMS`, and every new binary must run before any of
//! them replaces an old one.

use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub use semver::Version;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Where the releases are.
pub const RELEASES_URL: &str = "https://github.com/chiwanpark/loom/releases";
/// Names another place for the releases, as for tests.
pub const RELEASES_ENV: &str = "LOOM_RELEASES_URL";

/// How long a new binary may take to print its version.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("{0}")]
    Unsupported(String),
    #[error("could not download {url}: {source}")]
    Download {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{0}")]
    Release(String),
    #[error("{archive} does not match its checksum in SHA256SUMS")]
    Checksum { archive: String },
    #[error("{0}")]
    Archive(String),
    #[error("cannot write to {}: {source}", .dir.display())]
    NotWritable {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the new {binary} does not run: {message}")]
    Broken { binary: String, message: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = UpdateError> = std::result::Result<T, E>;

/// What a release archive holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Package {
    /// `loom-agent`, statically linked.
    Agent,
    /// The desktop client `loom` with the `loom-agent` it starts.
    Desktop,
}

impl Package {
    /// The archive's name, before its version.
    pub fn archive(self) -> &'static str {
        match self {
            Self::Agent => "loom-agent",
            Self::Desktop => "loom",
        }
    }

    /// The binaries it installs.
    pub fn binaries(self) -> &'static [&'static str] {
        match self {
            Self::Agent => &["loom-agent"],
            Self::Desktop => &["loom", "loom-agent"],
        }
    }

    /// The target of its archive for this machine.
    pub fn target(self) -> Result<&'static str> {
        use std::env::consts::{ARCH, OS};
        match (self, OS, ARCH) {
            (Self::Agent, "linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
            (Self::Agent, "linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
            (Self::Desktop, "linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
            _ => Err(UpdateError::Unsupported(format!(
                "the releases have no {} for {OS} {ARCH}; build it from source",
                self.archive()
            ))),
        }
    }
}

/// The version of this build.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// The directory of the running binary, where its update goes.
pub fn install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    // Linux names a binary that was replaced while it runs so.
    let exe = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(path) => PathBuf::from(path),
        None => exe,
    };
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| UpdateError::Unsupported(format!("{} has no directory", exe.display())))
}

/// A release's archive of a package for this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The archive's file name, such as
    /// `loom-agent-0.3.0-x86_64-unknown-linux-musl.tar.gz`.
    pub archive: String,
    url: String,
    sha256: String,
}

/// A binary that an update replaced or added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub binary: &'static str,
    pub path: PathBuf,
    /// What the binary it replaced printed for `--version`, if there was one.
    pub old: Option<String>,
    /// What the new binary prints for `--version`.
    pub new: String,
}

pub struct Updater {
    http: reqwest::Client,
    releases: String,
}

impl Default for Updater {
    fn default() -> Self {
        Self::new()
    }
}

impl Updater {
    /// Finds the releases where [`RELEASES_ENV`] says, or on GitHub.
    pub fn new() -> Self {
        let url = std::env::var(RELEASES_ENV)
            .ok()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| RELEASES_URL.to_owned());
        Self::with_releases_url(url)
    }

    pub fn with_releases_url(url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("loom/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            http,
            releases: url.into().trim_end_matches('/').to_owned(),
        }
    }

    /// The latest release, or the release of `version`, with its archive of
    /// `package` for this machine.
    pub async fn release(&self, package: Package, version: Option<&Version>) -> Result<Release> {
        let target = package.target()?;
        let base = &self.releases;
        let dir = match version {
            Some(version) => format!("{base}/download/v{version}"),
            None => format!("{base}/latest/download"),
        };
        let sums = self
            .get(&format!("{dir}/SHA256SUMS"))
            .await?
            .text()
            .await
            .map_err(|source| UpdateError::Download {
                url: format!("{dir}/SHA256SUMS"),
                source,
            })?;
        let release = match version {
            Some(version) => format!("release v{version}"),
            None => "the latest release".to_owned(),
        };
        let (sha256, archive, found) =
            find_archive(&sums, package.archive(), target).ok_or_else(|| {
                UpdateError::Release(format!(
                    "{release} has no {} archive for {target}",
                    package.archive()
                ))
            })?;
        if let Some(version) = version
            && found != *version
        {
            return Err(UpdateError::Release(format!(
                "{release} lists {} {found}",
                package.archive()
            )));
        }
        Ok(Release {
            version: found,
            url: format!("{dir}/{archive}"),
            archive,
            sha256,
        })
    }

    /// Downloads the release's archive, checks it, and puts the binaries of
    /// `package` in `dir`, each one moved over the old one so that what runs
    /// the old one keeps running. `progress` hears the bytes received so far
    /// and the total, when the server tells it.
    pub async fn install(
        &self,
        package: Package,
        release: &Release,
        dir: &Path,
        mut progress: impl FnMut(u64, Option<u64>) + Send,
    ) -> Result<Vec<Installed>> {
        // Fails before the download when the binaries cannot be replaced.
        let staging = tempfile::Builder::new()
            .prefix(".loom-update.")
            .tempdir_in(dir)
            .map_err(|source| UpdateError::NotWritable {
                dir: dir.to_path_buf(),
                source,
            })?;
        let archive = staging.path().join(&release.archive);
        let mut response = self.get(&release.url).await?;
        let total = response.content_length();
        let mut file = tokio::fs::File::create(&archive).await?;
        let mut hasher = Sha256::new();
        let mut received = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|source| UpdateError::Download {
                url: release.url.clone(),
                source,
            })?
        {
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            received += chunk.len() as u64;
            progress(received, total);
        }
        file.flush().await?;
        drop(file);
        if hex(&hasher.finalize()) != release.sha256 {
            return Err(UpdateError::Checksum {
                archive: release.archive.clone(),
            });
        }

        let binaries = package.binaries();
        let stem = release.archive.trim_end_matches(".tar.gz").to_owned();
        let out = staging.path().to_path_buf();
        tokio::task::spawn_blocking(move || extract(&archive, &stem, binaries, &out))
            .await
            .map_err(std::io::Error::other)??;

        let mut installed = Vec::new();
        for &binary in binaries {
            let new = version_of(&staging.path().join(binary))
                .await
                .map_err(|message| UpdateError::Broken {
                    binary: binary.to_owned(),
                    message,
                })?;
            let path = dir.join(binary);
            let old = version_of(&path).await.ok();
            installed.push(Installed {
                binary,
                path,
                old,
                new,
            });
        }
        for item in &installed {
            tokio::fs::rename(staging.path().join(item.binary), &item.path).await?;
        }
        Ok(installed)
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response> {
        self.http
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|source| UpdateError::Download {
                url: url.to_owned(),
                source,
            })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The checksum, file name, and version of the archive `name` for `target`
/// in a `SHA256SUMS` file.
fn find_archive(sums: &str, name: &str, target: &str) -> Option<(String, String, Version)> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.split_once(' ')?;
        let file = file.trim_start().trim_start_matches('*');
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let version = file
            .strip_prefix(name)?
            .strip_prefix('-')?
            .strip_suffix(".tar.gz")?
            .strip_suffix(target)?
            .strip_suffix('-')?;
        let version = Version::parse(version).ok()?;
        Some((hash.to_ascii_lowercase(), file.to_owned(), version))
    })
}

/// Writes the `binaries` in the archive's directory `stem` to `out`.
fn extract(archive: &Path, stem: &str, binaries: &[&str], out: &Path) -> Result<()> {
    let name = archive
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let bad = |err: std::io::Error| UpdateError::Archive(format!("cannot unpack {name}: {err}"));
    let file = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut found = Vec::new();
    for entry in tar.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let path = entry.path().map_err(bad)?.into_owned();
        let parts: Vec<Component> = path.components().collect();
        let [Component::Normal(dir), Component::Normal(file)] = parts.as_slice() else {
            continue;
        };
        let Some(&binary) = binaries.iter().find(|b| *file == **b) else {
            continue;
        };
        if *dir != stem || entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let dest = out.join(binary);
        let mut target = std::fs::File::create(&dest)?;
        std::io::copy(&mut entry, &mut target).map_err(bad)?;
        target.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        found.push(binary);
    }
    match binaries.iter().find(|b| !found.contains(b)) {
        Some(missing) => Err(UpdateError::Archive(format!("{name} has no {missing}"))),
        None => Ok(()),
    }
}

/// What the binary at `path` prints for `--version`.
async fn version_of(path: &Path) -> Result<String, String> {
    let mut tries = 0;
    let output = loop {
        let run = tokio::process::Command::new(path)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(VERSION_TIMEOUT, run).await {
            Err(_) => return Err("it did not print its version in time".to_owned()),
            Ok(Ok(output)) => break output,
            // A process forked while the file was open for writing can hold
            // it open for a moment.
            Ok(Err(err)) if err.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 20 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(Err(err)) => return Err(err.to_string()),
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{}: {}", output.status, stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests;
