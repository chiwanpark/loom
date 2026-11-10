use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use loom_proto::{ApiError, ErrorCode, ServerMessage, TerminalId};
use loom_tools::pty::{PtyExit, PtyProcess, PtySpawn};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const INPUT_QUEUE: usize = 256;
const MAX_DIMENSION: u16 = 1000;

struct Terminal {
    process: Arc<PtyProcess>,
    input: mpsc::Sender<Vec<u8>>,
    reader: Option<JoinHandle<()>>,
    writer: JoinHandle<()>,
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
pub struct Terminals {
    next_id: TerminalId,
    open: Arc<Mutex<HashMap<TerminalId, Terminal>>>,
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
    pub fn open(
        &mut self,
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
        self.next_id += 1;
        let id = self.next_id;
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

        self.open.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id,
            Terminal {
                process,
                input,
                reader: None,
                writer,
            },
        );
        let open = self.open.clone();
        let reader = tokio::spawn(async move {
            while let Some(data) = output.data.recv().await {
                if out
                    .send(ServerMessage::TerminalOutput { terminal: id, data })
                    .await
                    .is_err()
                {
                    return;
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
            let _ = out
                .send(ServerMessage::TerminalExited {
                    terminal: id,
                    status,
                })
                .await;
            let removed = open.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            if let Some(mut terminal) = removed {
                terminal.reader = None;
            }
        });

        if let Some(terminal) = self
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&id)
        {
            terminal.reader = Some(reader);
        }
        Ok(id)
    }

    fn with<T>(
        &self,
        id: TerminalId,
        f: impl FnOnce(&Terminal) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        f(open.get(&id).ok_or_else(|| not_found(id))?)
    }

    pub async fn input(&self, id: TerminalId, data: Vec<u8>) -> Result<(), ApiError> {
        let input = self.with(id, |t| Ok(t.input.clone()))?;
        input
            .send(data)
            .await
            .map_err(|_| ApiError::new(ErrorCode::NotFound, format!("terminal {id} is closed")))
    }

    pub fn resize(&self, id: TerminalId, cols: u16, rows: u16) -> Result<(), ApiError> {
        check_size(cols, rows)?;
        self.with(id, |t| {
            t.process
                .resize(cols, rows)
                .map_err(|err| ApiError::new(ErrorCode::Internal, err.to_string()))
        })
    }

    pub fn close(&self, id: TerminalId) -> Result<(), ApiError> {
        let removed = self
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        removed.map(drop).ok_or_else(|| not_found(id))
    }

    pub fn close_all(&self) {
        let all: Vec<Terminal> = self
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, t)| t)
            .collect();
        drop(all);
    }
}
