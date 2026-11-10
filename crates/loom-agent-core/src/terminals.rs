//! Interactive terminals (PTYs) hosted by the agent.
//!
//! Terminals belong to the agent, not to a connection. Each terminal is attached
//! to at most one connection, which receives its output and may send input,
//! resize, or close it. When that connection goes away the terminal is detached:
//! its output is discarded, and it is closed unless a client reattaches with
//! `AttachTerminal` within `terminal.detached_timeout`. Reattaching resizes the
//! PTY and sends `SIGWINCH` so full-screen programs redraw. Output produced while
//! detached is not replayed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use loom_proto::{ApiError, ErrorCode, ServerMessage, TerminalId};
use loom_tools::pty::{PtyExit, PtyProcess, PtySpawn};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const INPUT_QUEUE: usize = 256;
const MAX_DIMENSION: u16 = 1000;

/// Identifies a connection to the terminal manager.
pub type ConnectionId = u64;

#[derive(Clone)]
struct Sink {
    connection: ConnectionId,
    out: mpsc::Sender<ServerMessage>,
}

type SharedSink = Arc<Mutex<Option<Sink>>>;

struct Terminal {
    process: Arc<PtyProcess>,
    input: mpsc::Sender<Vec<u8>>,
    sink: SharedSink,
    /// Bumped on every attach and detach, so a stale detach timer does nothing.
    generation: u64,
    reader: Option<JoinHandle<()>>,
    writer: JoinHandle<()>,
}

impl Terminal {
    fn attached_to(&self) -> Option<ConnectionId> {
        lock(&self.sink).as_ref().map(|s| s.connection)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.writer.abort();
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
    }
}

#[derive(Default)]
struct State {
    next_id: TerminalId,
    open: HashMap<TerminalId, Terminal>,
}

/// The agent-wide terminal manager. Dropping it closes every terminal.
#[derive(Default)]
pub struct Terminals {
    state: Arc<Mutex<State>>,
    next_connection: AtomicU64,
}

impl Drop for Terminals {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn not_found(id: TerminalId) -> ApiError {
    ApiError::new(ErrorCode::NotFound, format!("no terminal {id}"))
}

fn check_size(cols: u16, rows: u16) -> Result<(), ApiError> {
    if cols == 0 || rows == 0 || cols > MAX_DIMENSION || rows > MAX_DIMENSION {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            format!("invalid terminal size {cols}x{rows}"),
        ));
    }
    Ok(())
}

pub fn interactive_shell(fallback: &Path) -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| fallback.to_path_buf())
}

impl Terminals {
    pub fn connection_id(&self) -> ConnectionId {
        self.next_connection.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn open(
        &self,
        connection: ConnectionId,
        shell: PathBuf,
        cwd: &Path,
        cols: u16,
        rows: u16,
        out: mpsc::Sender<ServerMessage>,
    ) -> Result<TerminalId, ApiError> {
        check_size(cols, rows)?;
        let (process, mut output) = PtyProcess::spawn(PtySpawn {
            program: shell,
            args: Vec::new(),
            cwd: cwd.to_path_buf(),
            env: vec![
                ("TERM".into(), "xterm-256color".into()),
                ("COLORTERM".into(), "truecolor".into()),
            ],
            cols,
            rows,
        })
        .map_err(|err| {
            ApiError::new(
                ErrorCode::Internal,
                format!("cannot open a terminal: {err}"),
            )
        })?;
        let process = Arc::new(process);

        let (input, mut queue) = mpsc::channel::<Vec<u8>>(INPUT_QUEUE);
        let writer_process = process.clone();
        let writer = tokio::spawn(async move {
            while let Some(data) = queue.recv().await {
                let process = writer_process.clone();
                let written = tokio::task::spawn_blocking(move || process.write(&data)).await;
                if !matches!(written, Ok(Ok(()))) {
                    return;
                }
            }
        });

        let sink: SharedSink = Arc::new(Mutex::new(Some(Sink { connection, out })));
        let mut state = lock(&self.state);
        state.next_id += 1;
        let id = state.next_id;
        let reader_sink = sink.clone();
        let weak = Arc::downgrade(&self.state);
        let reader = tokio::spawn(async move {
            while let Some(data) = output.data.recv().await {
                // Output while detached (or to a connection that is closing) is read and
                // dropped so the program never blocks. The connection detaches its
                // terminals itself when it is dropped, which also starts the close timer.
                let target = lock(&reader_sink).clone();
                if let Some(target) = target {
                    let _ = target
                        .out
                        .send(ServerMessage::TerminalOutput { terminal: id, data })
                        .await;
                }
            }
            let status = output
                .exit
                .await
                .unwrap_or(PtyExit {
                    code: None,
                    signal: None,
                })
                .status();
            let target = lock(&reader_sink).clone();
            if let Some(target) = target {
                let _ = target
                    .out
                    .send(ServerMessage::TerminalExited {
                        terminal: id,
                        status,
                    })
                    .await;
            }
            if let Some(state) = weak.upgrade() {
                let removed = lock(&state).open.remove(&id);
                if let Some(mut terminal) = removed {
                    // This task is finishing; do not abort it from Drop.
                    terminal.reader = None;
                }
            }
        });
        state.open.insert(
            id,
            Terminal {
                process,
                input,
                sink,
                generation: 0,
                reader: Some(reader),
                writer,
            },
        );
        Ok(id)
    }

    /// Runs `f` on a terminal attached to `connection`. Terminals attached to
    /// another connection (or detached) must be taken over with [`Self::attach`] first.
    fn with<T>(
        &self,
        connection: ConnectionId,
        id: TerminalId,
        f: impl FnOnce(&Terminal) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let state = lock(&self.state);
        let terminal = state.open.get(&id).ok_or_else(|| not_found(id))?;
        if terminal.attached_to() != Some(connection) {
            return Err(not_attached(id));
        }
        f(terminal)
    }

    pub async fn input(
        &self,
        connection: ConnectionId,
        id: TerminalId,
        data: Vec<u8>,
    ) -> Result<(), ApiError> {
        let input = self.with(connection, id, |t| Ok(t.input.clone()))?;
        input
            .send(data)
            .await
            .map_err(|_| ApiError::new(ErrorCode::NotFound, format!("terminal {id} is closed")))
    }

    pub fn resize(
        &self,
        connection: ConnectionId,
        id: TerminalId,
        cols: u16,
        rows: u16,
    ) -> Result<(), ApiError> {
        check_size(cols, rows)?;
        self.with(connection, id, |t| {
            t.process
                .resize(cols, rows)
                .map_err(|err| ApiError::new(ErrorCode::Internal, err.to_string()))
        })
    }

    pub fn close(&self, connection: ConnectionId, id: TerminalId) -> Result<(), ApiError> {
        let mut state = lock(&self.state);
        let terminal = state.open.get(&id).ok_or_else(|| not_found(id))?;
        if terminal.attached_to() != Some(connection) {
            return Err(not_attached(id));
        }
        let removed = state.open.remove(&id);
        drop(state);
        drop(removed);
        Ok(())
    }

    /// Moves a terminal to `connection`, whether it is detached or still attached
    /// to an older connection that has not been noticed as gone yet.
    pub fn attach(
        &self,
        connection: ConnectionId,
        id: TerminalId,
        cols: u16,
        rows: u16,
        out: mpsc::Sender<ServerMessage>,
    ) -> Result<(), ApiError> {
        check_size(cols, rows)?;
        let mut state = lock(&self.state);
        let terminal = state.open.get_mut(&id).ok_or_else(|| not_found(id))?;
        terminal.generation += 1;
        *lock(&terminal.sink) = Some(Sink { connection, out });
        terminal
            .process
            .resize(cols, rows)
            .map_err(|err| ApiError::new(ErrorCode::Internal, err.to_string()))?;
        terminal.process.redraw();
        Ok(())
    }

    /// Detaches every terminal of a closed connection. Each one is closed after
    /// `timeout` unless it is reattached first; a zero timeout closes them now.
    pub fn detach_connection(&self, connection: ConnectionId, timeout: Duration) {
        let mut state = lock(&self.state);
        let ids: Vec<TerminalId> = state
            .open
            .iter()
            .filter(|(_, t)| t.attached_to() == Some(connection))
            .map(|(id, _)| *id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let runtime = tokio::runtime::Handle::try_current().ok();
        if timeout.is_zero() || runtime.is_none() {
            let closed: Vec<Terminal> = ids.iter().filter_map(|id| state.open.remove(id)).collect();
            drop(state);
            drop(closed);
            return;
        }
        let mut timers = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(terminal) = state.open.get_mut(&id) {
                terminal.generation += 1;
                *lock(&terminal.sink) = None;
                timers.push((id, terminal.generation));
            }
        }
        drop(state);
        tracing::debug!(count = timers.len(), ?timeout, "terminals detached");
        let weak: Weak<Mutex<State>> = Arc::downgrade(&self.state);
        if let Some(runtime) = runtime {
            runtime.spawn(async move {
                tokio::time::sleep(timeout).await;
                let Some(state) = weak.upgrade() else { return };
                let mut state = lock(&state);
                let mut expired = Vec::new();
                for (id, generation) in timers {
                    if state
                        .open
                        .get(&id)
                        .is_some_and(|t| t.generation == generation)
                        && let Some(terminal) = state.open.remove(&id)
                    {
                        expired.push(terminal);
                    }
                }
                drop(state);
                if !expired.is_empty() {
                    tracing::info!(count = expired.len(), "closed detached terminals");
                }
                drop(expired);
            });
        }
    }

    pub fn close_all(&self) {
        let all: Vec<Terminal> = lock(&self.state).open.drain().map(|(_, t)| t).collect();
        drop(all);
    }
}

fn not_attached(id: TerminalId) -> ApiError {
    ApiError::new(
        ErrorCode::NotFound,
        format!("terminal {id} is not attached to this connection"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn output_until(rx: &mut mpsc::Receiver<ServerMessage>, needle: &str) {
        let mut seen = String::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(message) = rx.recv().await {
                if let ServerMessage::TerminalOutput { data, .. } = message {
                    seen.push_str(&String::from_utf8_lossy(&data));
                    if seen.contains(needle) {
                        return;
                    }
                }
            }
            panic!("channel closed before {needle:?}: {seen}");
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}: {seen}"));
    }

    fn open(
        terminals: &Terminals,
        connection: ConnectionId,
    ) -> (TerminalId, mpsc::Receiver<ServerMessage>) {
        let (tx, rx) = mpsc::channel(64);
        let cwd = std::env::temp_dir();
        let id = terminals
            .open(connection, "/bin/sh".into(), &cwd, 80, 24, tx)
            .unwrap();
        (id, rx)
    }

    #[tokio::test]
    async fn terminals_survive_detach_and_reattach() {
        let terminals = Terminals::default();
        let first = terminals.connection_id();
        let second = terminals.connection_id();
        let (id, mut rx) = open(&terminals, first);
        terminals
            .input(first, id, b"echo one=$((1))\n".to_vec())
            .await
            .unwrap();
        output_until(&mut rx, "one=1").await;

        // Another connection cannot drive a terminal it has not attached.
        let err = terminals
            .input(second, id, b"x".to_vec())
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(terminals.close(second, id).is_err());

        terminals.detach_connection(first, Duration::from_secs(60));
        assert!(terminals.input(first, id, b"x".to_vec()).await.is_err());

        let (tx, mut rx) = mpsc::channel(64);
        terminals.attach(second, id, 100, 30, tx).unwrap();
        terminals
            .input(second, id, b"stty size\n".to_vec())
            .await
            .unwrap();
        output_until(&mut rx, "30 100").await;
        // The old connection lost the terminal.
        assert!(terminals.resize(first, id, 80, 24).is_err());
        terminals.close(second, id).unwrap();
        assert_eq!(
            terminals
                .attach(second, id, 80, 24, mpsc::channel(1).0)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }

    #[tokio::test]
    async fn detached_terminals_close_after_the_timeout() {
        let terminals = Terminals::default();
        let connection = terminals.connection_id();
        let (kept, _rx1) = open(&terminals, connection);
        let (expired, _rx2) = open(&terminals, connection);
        terminals.detach_connection(connection, Duration::from_millis(200));
        let other = terminals.connection_id();
        let (tx, _rx) = mpsc::channel(8);
        terminals.attach(other, kept, 80, 24, tx).unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            terminals
                .attach(other, expired, 80, 24, mpsc::channel(1).0)
                .is_err()
        );
        // The reattached terminal outlived the timer of its earlier detach.
        terminals.close(other, kept).unwrap();

        let (now, _rx3) = open(&terminals, connection);
        terminals.detach_connection(connection, Duration::ZERO);
        assert!(
            terminals
                .attach(other, now, 80, 24, mpsc::channel(1).0)
                .is_err()
        );
    }

    #[tokio::test]
    async fn exit_is_reported_to_the_attached_connection() {
        let terminals = Terminals::default();
        let connection = terminals.connection_id();
        let (id, mut rx) = open(&terminals, connection);
        terminals
            .input(connection, id, b"exit 4\n".to_vec())
            .await
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(ServerMessage::TerminalExited { terminal, status }) = rx.recv().await
                    && terminal == id
                {
                    return status;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(status, 4);
    }
}
