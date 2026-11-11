use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::{Result, ToolError};
use crate::workspace::{FileAccess, FileGuard, FileTracker, Workspace, WorkspaceGuard};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    ReadOnly,
    Write,
    Exec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputChunk {
    pub stream: OutputStream,
    pub data: Vec<u8>,
}

pub type OutputSink = mpsc::Sender<OutputChunk>;

#[derive(Clone)]
pub struct LockWaitHook(Arc<dyn Fn(bool) + Send + Sync>);

impl LockWaitHook {
    pub fn new(hook: impl Fn(bool) + Send + Sync + 'static) -> Self {
        Self(Arc::new(hook))
    }
}

impl std::fmt::Debug for LockWaitHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LockWaitHook")
    }
}

struct WaitNotice<'a>(&'a LockWaitHook);

impl Drop for WaitNotice<'_> {
    fn drop(&mut self) {
        (self.0.0)(false);
    }
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: Arc<Workspace>,
    pub files: Arc<FileTracker>,
    pub cancel: CancellationToken,
    pub output: Option<OutputSink>,
    pub lock_wait: Option<LockWaitHook>,
}

impl ToolContext {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self {
            workspace,
            files: Arc::new(FileTracker::default()),
            cancel: CancellationToken::new(),
            output: None,
            lock_wait: None,
        }
    }

    pub fn with_lock_wait(mut self, hook: LockWaitHook) -> Self {
        self.lock_wait = Some(hook);
        self
    }

    pub async fn lock_file(&self, path: &std::path::Path, access: FileAccess) -> FileGuard {
        let locks = self.workspace.locks();
        if let Some(guard) = locks.try_lock_file(path, access) {
            return guard;
        }
        let _notice = self.notice_wait();
        locks.lock_file(path, access).await
    }

    pub async fn lock_workspace(&self) -> WorkspaceGuard {
        let locks = self.workspace.locks();
        if let Some(guard) = locks.try_lock_workspace() {
            return guard;
        }
        let _notice = self.notice_wait();
        locks.lock_workspace().await
    }

    fn notice_wait(&self) -> Option<WaitNotice<'_>> {
        let hook = self.lock_wait.as_ref()?;
        (hook.0)(true);
        Some(WaitNotice(hook))
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_output(mut self, output: OutputSink) -> Self {
        self.output = Some(output);
        self
    }

    pub fn with_files(mut self, files: Arc<FileTracker>) -> Self {
        self.files = files;
        self
    }

    pub(crate) fn ensure_not_cancelled(&self) -> Result<()> {
        if self.cancel.is_cancelled() {
            Err(ToolError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub details: ToolDetails,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolDetails {
    Read {
        path: PathBuf,
        start_line: usize,
        end_line: usize,
        total_lines: usize,
        truncated: bool,
    },
    Edit {
        path: PathBuf,
        diff: String,
        first_changed_line: Option<usize>,
    },
    Write {
        path: PathBuf,
        bytes: usize,
        created: bool,
        #[serde(default)]
        diff: Option<String>,
    },
    Shell {
        exit_code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
        cancelled: bool,
        truncated: bool,
        total_lines: usize,
        total_bytes: u64,
        full_output_path: Option<PathBuf>,
        duration_ms: u64,
    },
    WebSearch {
        query: String,
        sources: usize,
    },
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    fn risk(&self, args: &Value) -> Risk;

    /// Why the tool can't be used right now, if it can't: it is then not
    /// offered to the model, and a call to it fails.
    async fn blocked(&self) -> Option<String> {
        None
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput>;
}

pub(crate) fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T> {
    serde_json::from_value(args).map_err(|err| ToolError::InvalidArguments(err.to_string()))
}

/// Reads an optional non-negative integer that a model may send as a float or
/// a string.
pub fn lenient_opt_u64<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            if let Some(value) = number.as_u64() {
                Ok(Some(value))
            } else if let Some(value) = number.as_f64().filter(|v| *v >= 0.0 && v.fract() == 0.0) {
                Ok(Some(value as u64))
            } else {
                Err(serde::de::Error::custom(format!(
                    "expected a non-negative integer, got {number}"
                )))
            }
        }
        Some(Value::String(text)) => text
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("expected an integer, got {text:?}"))),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected an integer, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct Args {
        #[serde(default, deserialize_with = "lenient_opt_u64")]
        value: Option<u64>,
    }

    #[tokio::test]
    async fn reports_lock_waits() {
        use std::sync::Mutex;

        let dir = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::new(dir.path()).unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let ctx =
            ToolContext::new(workspace.clone()).with_lock_wait(LockWaitHook::new(move |waiting| {
                record.lock().unwrap().push(waiting)
            }));
        let path = dir.path().join("a");
        drop(ctx.lock_file(&path, FileAccess::Read).await);
        assert!(seen.lock().unwrap().is_empty());

        let held = workspace.locks().lock_workspace().await;
        let waiter = {
            let ctx = ctx.clone();
            let path = path.clone();
            tokio::spawn(async move {
                drop(ctx.lock_file(&path, FileAccess::Write).await);
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(*seen.lock().unwrap(), [true]);
        drop(held);
        waiter.await.unwrap();
        assert_eq!(*seen.lock().unwrap(), [true, false]);
    }

    #[test]
    fn lenient_numbers_accept_common_shapes() {
        let parse = |value: Value| parse_args::<Args>(value).map(|args| args.value);
        assert_eq!(parse(json!({})).unwrap(), None);
        assert_eq!(parse(json!({"value": null})).unwrap(), None);
        assert_eq!(parse(json!({"value": 3})).unwrap(), Some(3));
        assert_eq!(parse(json!({"value": 3.0})).unwrap(), Some(3));
        assert_eq!(parse(json!({"value": "7"})).unwrap(), Some(7));
        assert!(parse(json!({"value": -1})).is_err());
        assert!(parse(json!({"value": 1.5})).is_err());
        assert!(parse(json!({"value": "x"})).is_err());
    }
}
