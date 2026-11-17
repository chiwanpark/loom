//! Interactive terminals (PTYs) hosted by the agent.
//!
//! Terminals belong to the agent, not to a connection. Each terminal is attached
//! to at most one connection, which receives its output and may send input,
//! resize, or close it. A client can move a terminal to its connection with
//! `AttachTerminal`, which replays the terminal's recent output, so a terminal
//! opened on one device can be taken over on another. When the connection a
//! terminal is attached to goes away the terminal is detached: its output is
//! kept only as recent output, and it is closed unless a client attaches it
//! within `terminal.detached_timeout`. Attaching resizes the PTY and sends
//! `SIGWINCH` so full-screen programs redraw. Every connection hears of
//! terminals opening, closing, and moving through a watch channel.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use loom_proto::{
    ApiError, ErrorCode, ServerMessage, TERMINAL_REPLAY, TerminalClient, TerminalHolder,
    TerminalId, TerminalInfo, TerminalOwner,
};
use loom_tools::pty::{PtyExit, PtyProcess, PtySpawn};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

const INPUT_QUEUE: usize = 256;
const MAX_DIMENSION: u16 = 1000;

/// Identifies a connection to the terminal manager.
pub type ConnectionId = u64;

/// A connection that a terminal's output goes to.
#[derive(Clone)]
pub struct Viewer {
    pub connection: ConnectionId,
    /// The token the client sent, which lets it take a terminal back from
    /// an earlier connection of its own.
    pub owner: TerminalOwner,
    pub client: TerminalClient,
    pub out: mpsc::Sender<ServerMessage>,
}

/// A terminal's most recent output, which attaching replays.
#[derive(Default)]
struct Recent {
    bytes: VecDeque<u8>,
    /// Whether older output was dropped, so the bytes may start in the
    /// middle of a line or an escape sequence.
    cut: bool,
}

impl Recent {
    fn push(&mut self, data: &[u8]) {
        let data = &data[data.len().saturating_sub(TERMINAL_REPLAY)..];
        let over = (self.bytes.len() + data.len()).saturating_sub(TERMINAL_REPLAY);
        if over > 0 {
            self.bytes.drain(..over);
            self.cut = true;
        }
        self.bytes.extend(data);
    }

    /// The output to replay. Once older output was dropped, it starts after
    /// the first line break, so it does not start inside an escape sequence.
    fn replay(&self) -> Vec<u8> {
        let mut bytes: Vec<u8> = self.bytes.iter().copied().collect();
        if self.cut
            && let Some(newline) = bytes.iter().position(|&b| b == b'\n')
        {
            bytes.drain(..=newline);
        }
        bytes
    }
}

#[derive(Default)]
struct Output {
    viewer: Option<Viewer>,
    recent: Recent,
}

/// What a terminal's reader task shares with the manager.
#[derive(Default)]
struct Shared {
    output: Mutex<Output>,
    /// Held while output goes out, so that the replay of an attach reaches
    /// the new connection before any newer output.
    gate: tokio::sync::Mutex<()>,
}

impl Shared {
    fn viewer(&self) -> Option<Viewer> {
        lock(&self.output).viewer.clone()
    }
}

struct Terminal {
    process: Arc<PtyProcess>,
    input: mpsc::Sender<Vec<u8>>,
    shared: Arc<Shared>,
    /// Bumped on every attach and detach, so a stale detach timer does nothing.
    generation: u64,
    reader: Option<JoinHandle<()>>,
    writer: JoinHandle<()>,
}

impl Terminal {
    fn attached_to(&self) -> Option<ConnectionId> {
        lock(&self.shared.output)
            .viewer
            .as_ref()
            .map(|v| v.connection)
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

struct State {
    next_id: TerminalId,
    open: HashMap<TerminalId, Terminal>,
    /// Told whenever a terminal opens, closes, or moves to another connection.
    changes: watch::Sender<()>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            next_id: 0,
            open: HashMap::new(),
            changes: watch::Sender::new(()),
        }
    }
}

impl State {
    fn changed(&self) {
        self.changes.send_replace(());
    }
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

    /// Changes whenever a terminal opens, closes, or moves to another
    /// connection; [`Self::list`] then tells how things are.
    pub fn subscribe(&self) -> watch::Receiver<()> {
        lock(&self.state).changes.subscribe()
    }

    /// The open terminals, oldest first, as `connection` sees them.
    pub fn list(&self, connection: ConnectionId) -> Vec<TerminalInfo> {
        let state = lock(&self.state);
        let mut list: Vec<TerminalInfo> = state
            .open
            .iter()
            .map(|(&id, terminal)| TerminalInfo {
                id,
                holder: match terminal.shared.viewer() {
                    Some(v) if v.connection == connection => TerminalHolder::You,
                    Some(v) => TerminalHolder::Other(v.client),
                    None => TerminalHolder::Nobody,
                },
            })
            .collect();
        list.sort_by_key(|t| t.id);
        list
    }

    /// Opens a terminal that runs `shell` in `cwd` with only the variables
    /// of `env`.
    pub fn open(
        &self,
        viewer: Viewer,
        shell: PathBuf,
        cwd: &Path,
        mut env: Vec<(OsString, OsString)>,
        cols: u16,
        rows: u16,
    ) -> Result<TerminalId, ApiError> {
        check_size(cols, rows)?;
        env.push(("TERM".into(), "xterm-256color".into()));
        env.push(("COLORTERM".into(), "truecolor".into()));
        let (process, mut output) = PtyProcess::spawn(PtySpawn {
            program: shell,
            args: Vec::new(),
            cwd: cwd.to_path_buf(),
            clear_env: true,
            env,
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

        let shared = Arc::new(Shared::default());
        lock(&shared.output).viewer = Some(viewer);
        let mut state = lock(&self.state);
        state.next_id += 1;
        let id = state.next_id;
        let reader_shared = shared.clone();
        let weak = Arc::downgrade(&self.state);
        let reader = tokio::spawn(async move {
            while let Some(data) = output.data.recv().await {
                // Output while detached (or to a connection that is closing) is
                // only kept as recent output, so the program never blocks. The
                // connection detaches its terminals itself when it is dropped,
                // which also starts the close timer.
                let _gate = reader_shared.gate.lock().await;
                let target = {
                    let mut output = lock(&reader_shared.output);
                    output.recent.push(&data);
                    output.viewer.clone()
                };
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
            let target = reader_shared.viewer();
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
                let mut state = lock(&state);
                let removed = state.open.remove(&id);
                state.changed();
                drop(state);
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
                shared,
                generation: 0,
                reader: Some(reader),
                writer,
            },
        );
        state.changed();
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
        state.changed();
        drop(state);
        drop(removed);
        Ok(())
    }

    /// Moves a terminal to `viewer`'s connection and replays its recent
    /// output there. A terminal that another connection holds moves only
    /// with `take_over`, or when that connection has the same owner: an
    /// earlier connection of the same client that has not been noticed as
    /// gone yet.
    pub async fn attach(
        &self,
        viewer: Viewer,
        id: TerminalId,
        cols: u16,
        rows: u16,
        take_over: bool,
    ) -> Result<(), ApiError> {
        check_size(cols, rows)?;
        let (shared, process) = {
            let state = lock(&self.state);
            let terminal = state.open.get(&id).ok_or_else(|| not_found(id))?;
            if !take_over
                && let Some(holder) = terminal.shared.viewer()
                && holder.connection != viewer.connection
                && holder.owner != viewer.owner
            {
                return Err(ApiError::new(
                    ErrorCode::Conflict,
                    format!("terminal {id} is open on {}", holder.client.describe()),
                ));
            }
            (terminal.shared.clone(), terminal.process.clone())
        };
        let out = viewer.out.clone();
        let gate = shared.gate.lock().await;
        let replay = {
            let mut state = lock(&self.state);
            let terminal = state.open.get_mut(&id).ok_or_else(|| not_found(id))?;
            terminal.generation += 1;
            let mut output = lock(&shared.output);
            output.viewer = Some(viewer);
            let replay = output.recent.replay();
            drop(output);
            state.changed();
            replay
        };
        if !replay.is_empty() {
            let _ = out
                .send(ServerMessage::TerminalOutput {
                    terminal: id,
                    data: replay,
                })
                .await;
        }
        drop(gate);
        process
            .resize(cols, rows)
            .map_err(|err| ApiError::new(ErrorCode::Internal, err.to_string()))?;
        process.redraw();
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
        state.changed();
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
                lock(&terminal.shared.output).viewer = None;
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
                if !expired.is_empty() {
                    state.changed();
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

    fn viewer(
        connection: ConnectionId,
        owner: TerminalOwner,
    ) -> (Viewer, mpsc::Receiver<ServerMessage>) {
        let (out, rx) = mpsc::channel(64);
        let client = TerminalClient {
            program: format!("client-{owner}"),
            label: None,
        };
        let viewer = Viewer {
            connection,
            owner,
            client,
            out,
        };
        (viewer, rx)
    }

    fn open(
        terminals: &Terminals,
        connection: ConnectionId,
    ) -> (TerminalId, mpsc::Receiver<ServerMessage>) {
        let (viewer, rx) = viewer(connection, connection);
        let cwd = std::env::temp_dir();
        let id = terminals
            .open(
                viewer,
                "/bin/sh".into(),
                &cwd,
                std::env::vars_os().collect(),
                80,
                24,
            )
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
        assert_eq!(
            terminals.list(second),
            [TerminalInfo {
                id,
                holder: TerminalHolder::Nobody
            }]
        );

        // A detached terminal needs no taking over, and it replays its output.
        let (attached, mut rx) = viewer(second, second);
        terminals
            .attach(attached, id, 100, 30, false)
            .await
            .unwrap();
        output_until(&mut rx, "one=1").await;
        terminals
            .input(second, id, b"stty size\n".to_vec())
            .await
            .unwrap();
        output_until(&mut rx, "30 100").await;
        // The old connection lost the terminal.
        assert!(terminals.resize(first, id, 80, 24).is_err());
        terminals.close(second, id).unwrap();
        assert!(terminals.list(second).is_empty());
        assert_eq!(
            terminals
                .attach(viewer(second, second).0, id, 80, 24, true)
                .await
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }

    #[tokio::test]
    async fn taking_over_needs_consent_or_the_same_owner() {
        let terminals = Terminals::default();
        let mut changes = terminals.subscribe();
        let desktop = terminals.connection_id();
        let (id, mut desktop_rx) = open(&terminals, desktop);
        assert!(changes.has_changed().unwrap());
        changes.borrow_and_update();
        terminals
            .input(desktop, id, b"echo desk=$((2))\n".to_vec())
            .await
            .unwrap();
        output_until(&mut desktop_rx, "desk=2").await;

        let phone = terminals.connection_id();
        let (other, mut phone_rx) = viewer(phone, 77);
        let err = terminals
            .attach(other.clone(), id, 40, 10, false)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert!(err.message.contains(&format!("client-{desktop}")), "{err}");
        assert!(!changes.has_changed().unwrap());
        assert!(
            matches!(&terminals.list(phone)[0].holder, TerminalHolder::Other(c) if c.program == format!("client-{desktop}"))
        );

        // A later connection of the same owner takes it back without asking.
        let again = terminals.connection_id();
        let (same_owner, mut again_rx) = viewer(again, desktop);
        terminals
            .attach(same_owner, id, 80, 24, false)
            .await
            .unwrap();
        assert!(changes.has_changed().unwrap());
        output_until(&mut again_rx, "desk=2").await;
        assert_eq!(terminals.list(again)[0].holder, TerminalHolder::You);

        terminals.attach(other, id, 40, 10, true).await.unwrap();
        output_until(&mut phone_rx, "desk=2").await;
        assert!(terminals.input(again, id, b"x".to_vec()).await.is_err());
        terminals.close(phone, id).unwrap();
    }

    #[test]
    fn recent_output_is_bounded_and_starts_on_a_line() {
        let mut recent = Recent::default();
        recent.push(b"\x1b[31mfirst\r\nsecond\r\n");
        assert_eq!(recent.replay(), b"\x1b[31mfirst\r\nsecond\r\n");
        let filler = vec![b'x'; TERMINAL_REPLAY - 4];
        recent.push(&filler);
        recent.push(b"\r\nend");
        assert_eq!(recent.bytes.len(), TERMINAL_REPLAY);
        assert_eq!(recent.replay(), b"end");
        recent.push(&vec![b'y'; TERMINAL_REPLAY * 2]);
        assert_eq!(recent.replay().len(), TERMINAL_REPLAY);
    }

    #[tokio::test]
    async fn detached_terminals_close_after_the_timeout() {
        let terminals = Terminals::default();
        let connection = terminals.connection_id();
        let (kept, _rx1) = open(&terminals, connection);
        let (expired, _rx2) = open(&terminals, connection);
        terminals.detach_connection(connection, Duration::from_millis(200));
        let other = terminals.connection_id();
        let (attached, _rx) = viewer(other, other);
        terminals
            .attach(attached, kept, 80, 24, false)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            terminals
                .attach(viewer(other, other).0, expired, 80, 24, false)
                .await
                .is_err()
        );
        // The reattached terminal outlived the timer of its earlier detach.
        terminals.close(other, kept).unwrap();

        let (now, _rx3) = open(&terminals, connection);
        terminals.detach_connection(connection, Duration::ZERO);
        assert!(
            terminals
                .attach(viewer(other, other).0, now, 80, 24, false)
                .await
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
