use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{Instant, sleep_until};

use crate::error::{Result, ToolError};
use crate::pty::{PtyExit, PtyProcess, PtySpawn};
use crate::sandbox::SandboxLauncher;
use crate::tool::{
    OutputChunk, OutputStream, Risk, Tool, ToolContext, ToolDetails, ToolOutput, ToolSpec,
    lenient_opt_u64, parse_args,
};
use crate::truncate::{Limits, format_size, tail};
use crate::vtext::PlainText;

const READ_BUFFER_BYTES: usize = 8192;
const PTY_COLS: u16 = 120;
const PTY_ROWS: u16 = 40;
const EOF_CHAR: u8 = 0x04;
const DRAIN_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub shell: PathBuf,
    pub default_timeout: Option<Duration>,
    pub spill_dir: PathBuf,
    pub limits: Limits,
    pub pty: bool,
    pub sandbox: Option<SandboxLauncher>,
    pub max_saved_output: Option<u64>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        let bash = Path::new("/bin/bash");
        Self {
            shell: if bash.exists() {
                bash.to_path_buf()
            } else {
                PathBuf::from("/bin/sh")
            },
            default_timeout: None,
            sandbox: None,
            max_saved_output: None,
            spill_dir: std::env::temp_dir(),
            limits: Limits::default(),
            pty: false,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ShellTool {
    config: ShellConfig,
}

impl ShellTool {
    pub fn new(config: ShellConfig) -> Self {
        Self { config }
    }

    fn argv(&self, root: &Path, command: &str) -> (PathBuf, Vec<OsString>) {
        let shell_argv: Vec<OsString> = vec![
            self.config.shell.clone().into_os_string(),
            "-c".into(),
            command.into(),
        ];
        match &self.config.sandbox {
            Some(launcher) => {
                let mut launcher = launcher.clone();
                launcher.policy.writable.insert(0, root.to_path_buf());
                launcher.argv(&shell_argv)
            }
            None => (self.config.shell.clone(), shell_argv[1..].to_vec()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ShellArgs {
    command: String,
    #[serde(default, deserialize_with = "lenient_opt_u64")]
    timeout: Option<u64>,
}

#[async_trait]
impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        let limits = self.config.limits;
        ToolSpec {
            name: "shell".into(),
            description: format!(
                "Run a shell command with {} in the workspace root. Returns combined stdout and stderr. Output is limited to the last {} lines or {}; when it is cut, the full output is saved to a file. {}",
                self.config.shell.display(),
                limits.max_lines,
                format_size(limits.max_bytes as u64),
                if self.config.pty {
                    "Commands run in a terminal but get no input (stdin is at end of file), so avoid interactive programs."
                } else {
                    "Commands do not get a terminal or stdin, so avoid interactive programs."
                }
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to run"
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Timeout in seconds"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::Exec
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: ShellArgs = parse_args(args)?;
        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "command must not be empty".into(),
            ));
        }
        ctx.ensure_not_cancelled()?;
        let _guard = tokio::select! {
            guard = ctx.lock_workspace() => guard,
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
        };

        let timeout = args
            .timeout
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs)
            .or(self.config.default_timeout);
        let started = Instant::now();
        if self.config.pty {
            return self.call_pty(ctx, &args.command, timeout, started).await;
        }
        let (program, argv) = self.argv(ctx.workspace.root(), &args.command);
        let mut child = Command::new(&program)
            .args(&argv)
            .current_dir(ctx.workspace.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| ToolError::io(format!("cannot start {}", program.display()), err))?;
        let mut group = ProcessGroup::new(child.id());
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let mut stdout_buffer = vec![0u8; READ_BUFFER_BYTES];
        let mut stderr_buffer = vec![0u8; READ_BUFFER_BYTES];
        let mut collector = OutputCollector::new(&self.config);
        let deadline = timeout.map(|timeout| started + timeout);
        let mut drain_deadline = None;
        let mut status: Option<ExitStatus> = None;
        let mut timed_out = false;
        let mut cancelled = false;

        loop {
            if status.is_some() && stdout.is_none() && stderr.is_none() {
                break;
            }
            tokio::select! {
                _ = ctx.cancel.cancelled(), if !cancelled && !timed_out && status.is_none() => {
                    cancelled = true;
                    group.kill();
                }
                _ = sleep_opt(deadline), if !cancelled && !timed_out && status.is_none() => {
                    timed_out = true;
                    group.kill();
                }
                _ = sleep_opt(drain_deadline), if drain_deadline.is_some() => break,
                read = read_opt(&mut stdout, &mut stdout_buffer), if stdout.is_some() => {
                    match read {
                        Ok(0) | Err(_) => stdout = None,
                        Ok(n) => collector.push(ctx, OutputStream::Stdout, &stdout_buffer[..n]).await?,
                    }
                }
                read = read_opt(&mut stderr, &mut stderr_buffer), if stderr.is_some() => {
                    match read {
                        Ok(0) | Err(_) => stderr = None,
                        Ok(n) => collector.push(ctx, OutputStream::Stderr, &stderr_buffer[..n]).await?,
                    }
                }
                result = child.wait(), if status.is_none() => {
                    let exit = result.map_err(|err| ToolError::io("cannot wait for command", err))?;
                    group.reaped();
                    status = Some(exit);
                    drain_deadline = Some(Instant::now() + DRAIN_GRACE);
                }
            }
        }

        let collected = collector.finish().await?;
        Ok(build_output(Run {
            collected,
            exit_code: status.and_then(|status| status.code()),
            signal: status.and_then(|status| status.signal()),
            timed_out,
            cancelled,
            timeout,
            duration: started.elapsed(),
        }))
    }
}

impl ShellTool {
    async fn call_pty(
        &self,
        ctx: &ToolContext,
        command: &str,
        timeout: Option<Duration>,
        started: Instant,
    ) -> Result<ToolOutput> {
        let (program, argv) = self.argv(ctx.workspace.root(), command);
        let (process, mut output) = PtyProcess::spawn(PtySpawn {
            program,
            args: argv
                .into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            cwd: ctx.workspace.root().to_path_buf(),
            env: vec![("TERM".into(), "xterm-256color".into())],
            cols: PTY_COLS,
            rows: PTY_ROWS,
        })
        .map_err(|err| {
            ToolError::io(format!("cannot start {}", self.config.shell.display()), err)
        })?;
        let _ = process.write(&[EOF_CHAR]);
        let mut text = PlainText::new();
        let mut collector = OutputCollector::new(&self.config);
        let deadline = timeout.map(|timeout| started + timeout);
        let mut drain_deadline = None;
        let mut exit: Option<PtyExit> = None;
        let mut data_open = true;
        let mut timed_out = false;
        let mut cancelled = false;
        loop {
            if exit.is_some() && !data_open {
                break;
            }
            tokio::select! {
                _ = ctx.cancel.cancelled(), if !cancelled && !timed_out && exit.is_none() => {
                    cancelled = true;
                    process.kill();
                }
                _ = sleep_opt(deadline), if !cancelled && !timed_out && exit.is_none() => {
                    timed_out = true;
                    process.kill();
                }
                _ = sleep_opt(drain_deadline), if drain_deadline.is_some() => break,
                chunk = output.data.recv(), if data_open => match chunk {
                    Some(data) => {
                        collector.emit(ctx, OutputStream::Stdout, &data).await;
                        collector.record(text.feed(&data).as_bytes()).await?;
                    }
                    None => data_open = false,
                },
                status = &mut output.exit, if exit.is_none() => {
                    exit = Some(status.unwrap_or(PtyExit { code: None, signal: None }));
                    drain_deadline = Some(Instant::now() + DRAIN_GRACE);
                }
            }
        }
        collector.record(text.finish().as_bytes()).await?;
        let collected = collector.finish().await?;
        Ok(build_output(Run {
            collected,
            exit_code: exit.and_then(|e| e.code),
            signal: exit.and_then(|e| e.signal),
            timed_out,
            cancelled,
            timeout,
            duration: started.elapsed(),
        }))
    }
}

struct Run {
    collected: Collected,
    exit_code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    timeout: Option<Duration>,
    duration: Duration,
}

fn build_output(run: Run) -> ToolOutput {
    let Run {
        collected,
        exit_code,
        signal,
        timed_out,
        cancelled,
        timeout,
        duration,
    } = run;
    let mut content = if collected.text.is_empty() {
        "(no output)".to_string()
    } else {
        collected.text
    };
    let mut notices = Vec::new();
    if collected.truncated {
        let location = collected
            .full_output_path
            .as_ref()
            .map(|path| {
                format!(
                    " Full output: {} (inspect it with the shell tool)",
                    path.display()
                )
            })
            .unwrap_or_default();
        notices.push(format!(
            "[Showing the last {} of {} lines.{location}]",
            collected.shown_lines, collected.total_lines
        ));
    }
    if timed_out {
        notices.push(format!(
            "[Command timed out after {}s]",
            timeout.unwrap_or_default().as_secs()
        ));
    } else if cancelled {
        notices.push("[Command cancelled]".to_string());
    } else if let Some(code) = exit_code.filter(|code| *code != 0) {
        notices.push(format!("[Exit code: {code}]"));
    } else if let Some(signal) = signal {
        notices.push(format!("[Killed by signal {signal}]"));
    }
    if !notices.is_empty() {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push('\n');
        content.push_str(&notices.join("\n"));
    }

    ToolOutput {
        content,
        is_error: timed_out || cancelled || exit_code != Some(0),
        details: ToolDetails::Shell {
            exit_code,
            signal,
            timed_out,
            cancelled,
            truncated: collected.truncated,
            total_lines: collected.total_lines,
            total_bytes: collected.total_bytes,
            full_output_path: collected.full_output_path,
            duration_ms: duration.as_millis() as u64,
        },
    }
}

async fn sleep_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn read_opt<R: AsyncRead + Unpin>(
    reader: &mut Option<R>,
    buffer: &mut [u8],
) -> std::io::Result<usize> {
    match reader {
        Some(reader) => reader.read(buffer).await,
        None => std::future::pending().await,
    }
}

struct ProcessGroup {
    pgid: Option<Pid>,
}

impl ProcessGroup {
    fn new(pid: Option<u32>) -> Self {
        Self {
            pgid: pid
                .and_then(|pid| i32::try_from(pid).ok())
                .map(Pid::from_raw),
        }
    }

    fn kill(&self) {
        if let Some(pgid) = self.pgid {
            let _ = killpg(pgid, Signal::SIGKILL);
        }
    }

    fn reaped(&mut self) {
        self.pgid = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

struct Collected {
    text: String,
    truncated: bool,
    shown_lines: usize,
    total_lines: usize,
    total_bytes: u64,
    full_output_path: Option<PathBuf>,
}

struct OutputCollector {
    limits: Limits,
    spill_dir: PathBuf,
    max_saved: Option<u64>,
    saved: u64,
    head: Vec<u8>,
    tail: Vec<u8>,
    tail_trimmed: bool,
    spill: Option<(tokio::fs::File, PathBuf)>,
    total_bytes: u64,
    newlines: usize,
    ends_with_newline: bool,
}

impl OutputCollector {
    fn new(config: &ShellConfig) -> Self {
        Self {
            limits: config.limits,
            spill_dir: config.spill_dir.clone(),
            max_saved: config.max_saved_output,
            saved: 0,
            head: Vec::new(),
            tail: Vec::new(),
            tail_trimmed: false,
            spill: None,
            total_bytes: 0,
            newlines: 0,
            ends_with_newline: false,
        }
    }

    async fn push(&mut self, ctx: &ToolContext, stream: OutputStream, data: &[u8]) -> Result<()> {
        self.emit(ctx, stream, data).await;
        self.record(data).await
    }

    async fn emit(&self, ctx: &ToolContext, stream: OutputStream, data: &[u8]) {
        if let Some(sink) = &ctx.output {
            let _ = sink
                .send(OutputChunk {
                    stream,
                    data: data.to_vec(),
                })
                .await;
        }
    }

    async fn record(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.total_bytes += data.len() as u64;
        self.newlines += data.iter().filter(|byte| **byte == b'\n').count();
        if let Some(last) = data.last() {
            self.ends_with_newline = *last == b'\n';
        }

        if self.spill.is_none() && self.head.len() + data.len() <= self.limits.max_bytes {
            self.head.extend_from_slice(data);
            return Ok(());
        }
        if self.spill.is_none() {
            self.open_spill().await?;
            self.tail = std::mem::take(&mut self.head);
        }
        if let Some((file, _)) = &mut self.spill {
            let room = self
                .max_saved
                .map_or(data.len() as u64, |max| max.saturating_sub(self.saved));
            let take = (data.len() as u64).min(room) as usize;
            if take > 0 {
                file.write_all(&data[..take])
                    .await
                    .map_err(|err| ToolError::io("cannot save command output", err))?;
                self.saved += take as u64;
            }
        }
        self.tail.extend_from_slice(data);
        let keep = self.limits.max_bytes;
        if self.tail.len() > keep * 2 {
            self.tail.drain(..self.tail.len() - keep);
            self.tail_trimmed = true;
        }
        Ok(())
    }

    async fn open_spill(&mut self) -> Result<()> {
        let temp = tempfile::Builder::new()
            .prefix("loom-shell-")
            .suffix(".log")
            .tempfile_in(&self.spill_dir)
            .map_err(|err| ToolError::io("cannot save command output", err))?;
        let (file, path) = temp
            .keep()
            .map_err(|err| ToolError::io("cannot save command output", err.error))?;
        let mut file = tokio::fs::File::from_std(file);
        self.saved = self.head.len() as u64;
        file.write_all(&self.head)
            .await
            .map_err(|err| ToolError::io("cannot save command output", err))?;
        self.spill = Some((file, path));
        Ok(())
    }

    async fn finish(mut self) -> Result<Collected> {
        let spilled = self.spill.is_some();
        let bytes = if spilled { &self.tail } else { &self.head };
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        if self.tail_trimmed {
            match text.find('\n') {
                Some(index) => {
                    text.drain(..=index);
                }
                None => text.clear(),
            }
        }
        let shown = tail(&text, self.limits);
        let truncated = shown.truncated || self.tail_trimmed;
        if truncated && !spilled {
            self.open_spill().await?;
        }
        let full_output_path = match self.spill.take() {
            Some((mut file, path)) => {
                file.flush()
                    .await
                    .map_err(|err| ToolError::io("cannot save command output", err))?;
                Some(path)
            }
            None => None,
        };
        let total_lines =
            self.newlines + usize::from(self.total_bytes > 0 && !self.ends_with_newline);
        Ok(Collected {
            text: shown.text,
            truncated,
            shown_lines: shown.lines,
            total_lines,
            total_bytes: self.total_bytes,
            full_output_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::workspace::{FileAccess, Workspace};

    fn context() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::new(dir.path()).unwrap());
        (dir, ToolContext::new(workspace))
    }

    fn tool_with_spill(spill_dir: &Path, limits: Limits) -> ShellTool {
        ShellTool::new(ShellConfig {
            spill_dir: spill_dir.to_path_buf(),
            limits,
            ..ShellConfig::default()
        })
    }

    #[tokio::test]
    async fn runs_in_workspace_root() {
        let (dir, ctx) = context();
        let output = ShellTool::default()
            .call(&ctx, json!({"command": "pwd"}))
            .await
            .unwrap();
        assert_eq!(
            output.content.trim(),
            dir.path().canonicalize().unwrap().to_str().unwrap()
        );
        assert!(!output.is_error);
    }

    #[tokio::test]
    async fn combines_stdout_and_stderr() {
        let (_dir, ctx) = context();
        let output = ShellTool::default()
            .call(&ctx, json!({"command": "echo out; echo err >&2"}))
            .await
            .unwrap();
        assert!(output.content.contains("out\n"));
        assert!(output.content.contains("err\n"));
    }

    #[tokio::test]
    async fn reports_exit_code() {
        let (_dir, ctx) = context();
        let output = ShellTool::default()
            .call(&ctx, json!({"command": "echo failing; exit 3"}))
            .await
            .unwrap();
        assert!(output.is_error);
        assert_eq!(output.content, "failing\n\n[Exit code: 3]");
        assert!(matches!(
            output.details,
            ToolDetails::Shell {
                exit_code: Some(3),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn reports_no_output() {
        let (_dir, ctx) = context();
        let output = ShellTool::default()
            .call(&ctx, json!({"command": "true"}))
            .await
            .unwrap();
        assert_eq!(output.content, "(no output)");
    }

    #[tokio::test]
    async fn times_out_and_kills_process_group() {
        let (dir, ctx) = context();
        let marker = dir.path().join("marker");
        let command = format!("(sleep 2; touch {}) & sleep 30", marker.display());
        let output = ShellTool::default()
            .call(&ctx, json!({"command": command, "timeout": 1}))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.ends_with("[Command timed out after 1s]"));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn cancels_running_command() {
        let (_dir, ctx) = context();
        let cancel = CancellationToken::new();
        let ctx = ctx.with_cancel(cancel.clone());
        let task = tokio::spawn(async move {
            ShellTool::default()
                .call(&ctx, json!({"command": "echo started; sleep 30"}))
                .await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
        let output = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(output.is_error);
        assert_eq!(output.content, "started\n\n[Command cancelled]");
    }

    #[tokio::test]
    async fn streams_output_chunks() {
        let (_dir, ctx) = context();
        let (sender, mut receiver) = mpsc::channel(16);
        let ctx = ctx.with_output(sender);
        ShellTool::default()
            .call(&ctx, json!({"command": "echo 안녕"}))
            .await
            .unwrap();
        drop(ctx);
        let mut streamed = Vec::new();
        while let Some(chunk) = receiver.recv().await {
            assert_eq!(chunk.stream, OutputStream::Stdout);
            streamed.extend(chunk.data);
        }
        assert_eq!(String::from_utf8(streamed).unwrap(), "안녕\n");
    }

    #[tokio::test]
    async fn truncates_long_output_and_saves_full_output() {
        let (_dir, ctx) = context();
        let spill = tempfile::tempdir().unwrap();
        let tool = tool_with_spill(
            spill.path(),
            Limits {
                max_lines: 5,
                max_bytes: 1024,
            },
        );
        let output = tool
            .call(&ctx, json!({"command": "seq 1 100"}))
            .await
            .unwrap();
        assert!(
            output
                .content
                .starts_with("96\n97\n98\n99\n100\n\n[Showing the last 5 of 100 lines.")
        );
        let ToolDetails::Shell {
            full_output_path: Some(path),
            truncated: true,
            total_lines: 100,
            ..
        } = output.details
        else {
            panic!("unexpected details: {:?}", output.details);
        };
        let saved = std::fs::read_to_string(path).unwrap();
        assert_eq!(saved.lines().count(), 100);
    }

    #[tokio::test]
    async fn spills_large_output_without_buffering_everything() {
        let (_dir, ctx) = context();
        let spill = tempfile::tempdir().unwrap();
        let tool = tool_with_spill(
            spill.path(),
            Limits {
                max_lines: 3,
                max_bytes: 64,
            },
        );
        let output = tool
            .call(&ctx, json!({"command": "seq 1 20000"}))
            .await
            .unwrap();
        assert!(output.content.starts_with("19998\n19999\n20000\n"));
        let ToolDetails::Shell {
            full_output_path: Some(path),
            total_lines,
            total_bytes,
            ..
        } = output.details
        else {
            panic!("unexpected details");
        };
        assert_eq!(total_lines, 20000);
        assert_eq!(std::fs::metadata(path).unwrap().len(), total_bytes);
    }

    #[tokio::test]
    async fn does_not_wait_for_background_processes() {
        let (_dir, ctx) = context();
        let started = Instant::now();
        let output = ShellTool::default()
            .call(&ctx, json!({"command": "sleep 5 & echo done"}))
            .await
            .unwrap();
        assert!(output.content.starts_with("done\n"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn waits_for_file_tools_to_finish() {
        let (_dir, ctx) = context();
        let guard = ctx
            .workspace
            .locks()
            .lock_file(&ctx.workspace.root().join("a"), FileAccess::Write)
            .await;
        let task = {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                ShellTool::default()
                    .call(&ctx, json!({"command": "echo ok"}))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!task.is_finished());
        drop(guard);
        let output = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(output.content, "ok\n");
    }

    #[tokio::test]
    async fn rejects_empty_command() {
        let (_dir, ctx) = context();
        let result = ShellTool::default()
            .call(&ctx, json!({"command": "  "}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    fn pty_tool(dir: &Path) -> ShellTool {
        ShellTool::new(ShellConfig {
            spill_dir: dir.to_path_buf(),
            pty: true,
            ..ShellConfig::default()
        })
    }

    #[tokio::test]
    async fn pty_mode_streams_raw_bytes_and_returns_plain_text() {
        let (dir, ctx) = context();
        let (tx, mut rx) = mpsc::channel(64);
        let ctx = ctx.with_output(tx);
        let output = pty_tool(dir.path())
            .call(
                &ctx,
                json!({"command": "test -t 1 && echo tty; printf '\\033[31mred\\033[0m\\n'; printf 'step 1\\rstep 2\\n'; echo $TERM"}),
            )
            .await
            .unwrap();
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content, "tty\nred\nstep 2\nxterm-256color\n");
        let mut raw = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            raw.extend(chunk.data);
        }
        let raw = String::from_utf8_lossy(&raw);
        assert!(raw.contains("\x1b[31mred"), "{raw:?}");
    }

    #[tokio::test]
    async fn pty_mode_reports_exit_codes_and_gives_stdin_eof() {
        let (dir, ctx) = context();
        let tool = pty_tool(dir.path());
        let output = tool
            .call(&ctx, json!({"command": "echo failing; exit 4"}))
            .await
            .unwrap();
        assert!(output.is_error);
        assert_eq!(output.content, "failing\n\n[Exit code: 4]");
        let output = tool
            .call(&ctx, json!({"command": "cat; echo after", "timeout": 5}))
            .await
            .unwrap();
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.ends_with("after\n"), "{:?}", output.content);
    }

    #[tokio::test]
    async fn pty_mode_times_out_and_cancels() {
        let (dir, ctx) = context();
        let tool = pty_tool(dir.path());
        let started = std::time::Instant::now();
        let output = tool
            .call(&ctx, json!({"command": "sleep 30", "timeout": 1}))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("timed out"), "{}", output.content);
        assert!(started.elapsed() < Duration::from_secs(10));

        let cancel = CancellationToken::new();
        let ctx = ctx.with_cancel(cancel.clone());
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            trigger.cancel();
        });
        let output = tool
            .call(&ctx, json!({"command": "sleep 30"}))
            .await
            .unwrap();
        assert!(output.content.contains("cancelled"), "{}", output.content);
    }
}
