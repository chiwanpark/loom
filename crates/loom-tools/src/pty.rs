use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{mpsc, oneshot};

const READ_CHUNK: usize = 16 * 1024;
const OUTPUT_QUEUE: usize = 64;

#[derive(Debug, Clone)]
pub struct PtySpawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl PtyExit {
    pub fn status(self) -> i32 {
        match (self.code, self.signal) {
            (Some(code), _) => code,
            (None, Some(signal)) => 128 + signal,
            (None, None) => -1,
        }
    }
}

pub struct PtyOutput {
    pub data: mpsc::Receiver<Vec<u8>>,
    pub exit: oneshot::Receiver<PtyExit>,
}

pub struct PtyProcess {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    pid: Pid,
    exited: Arc<AtomicBool>,
}

fn other(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(err.to_string())
}

impl PtyProcess {
    pub fn spawn(spec: PtySpawn) -> io::Result<(Self, PtyOutput)> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: spec.rows.max(1),
                cols: spec.cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(other)?;
        let mut command = CommandBuilder::new(&spec.program);
        command.args(&spec.args);
        command.cwd(&spec.cwd);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        let child = pair.slave.spawn_command(command).map_err(other)?;
        drop(pair.slave);
        let pid = child
            .process_id()
            .and_then(|pid| i32::try_from(pid).ok())
            .map(Pid::from_raw)
            .ok_or_else(|| other("the PTY child has no process ID"))?;
        let mut reader = pair.master.try_clone_reader().map_err(other)?;
        let writer = pair.master.take_writer().map_err(other)?;

        let (data_tx, data) = mpsc::channel(OUTPUT_QUEUE);
        std::thread::Builder::new()
            .name("loom-pty-read".into())
            .spawn(move || {
                let mut buf = vec![0u8; READ_CHUNK];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if data_tx.blocking_send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })?;

        let (exit_tx, exit) = oneshot::channel();
        let exited = Arc::new(AtomicBool::new(false));
        let reaped = exited.clone();
        std::thread::Builder::new()
            .name("loom-pty-wait".into())
            .spawn(move || {
                let _child = child;
                let status = loop {
                    match waitpid(pid, None) {
                        Ok(WaitStatus::Exited(_, code)) => {
                            break PtyExit {
                                code: Some(code),
                                signal: None,
                            };
                        }
                        Ok(WaitStatus::Signaled(_, signal, _)) => {
                            break PtyExit {
                                code: None,
                                signal: Some(signal as i32),
                            };
                        }
                        Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                        Err(_) => {
                            break PtyExit {
                                code: None,
                                signal: None,
                            };
                        }
                    }
                };
                reaped.store(true, Ordering::SeqCst);
                let _ = exit_tx.send(status);
            })?;

        Ok((
            Self {
                master: Mutex::new(pair.master),
                writer: Mutex::new(writer),
                pid,
                exited,
            },
            PtyOutput { data, exit },
        ))
    }

    pub fn pid(&self) -> u32 {
        self.pid.as_raw() as u32
    }

    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(data)?;
        writer.flush()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(other)
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    pub fn signal(&self, signal: Signal) {
        if !self.has_exited() {
            let _ = killpg(self.pid, signal);
        }
    }

    pub fn kill(&self) {
        self.signal(Signal::SIGKILL);
    }

    /// Asks the program in the foreground of the terminal to redraw, by sending
    /// `SIGWINCH` to the terminal's foreground process group (or the shell's group).
    pub fn redraw(&self) {
        if self.has_exited() {
            return;
        }
        let foreground = self
            .master
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .process_group_leader()
            .filter(|pgid| *pgid > 0)
            .map(Pid::from_raw)
            .unwrap_or(self.pid);
        let _ = killpg(foreground, Signal::SIGWINCH);
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        self.signal(Signal::SIGHUP);
        self.kill();
    }
}
