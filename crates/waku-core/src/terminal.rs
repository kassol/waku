//! Daemon-owned pseudoterminals for remote clients.
//!
//! The daemon owns the shell, cwd, and PTY. Clients only render the byte
//! stream and send input/resize controls, so a browser can operate against a
//! daemon on another machine without interpreting any daemon-side paths.

#[cfg(not(unix))]
use std::path::Path;

#[cfg(not(unix))]
use anyhow::bail;

#[cfg(not(unix))]
use crate::EventSink;

#[cfg(unix)]
mod platform {
    use std::io::{Read as _, Write as _};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use alacritty_terminal::event::{OnResize as _, WindowSize};
    use alacritty_terminal::tty::{self, EventedPty as _, EventedReadWrite as _, Shell};
    use anyhow::{Context as _, bail};
    use base64::Engine as _;
    use parking_lot::Mutex;
    use serde_json::json;

    use crate::{EventSink, WireDriverEvent};

    const CELL_WIDTH: u16 = 8;
    const CELL_HEIGHT: u16 = 16;
    const MIN_COLUMNS: u16 = 2;
    const MIN_ROWS: u16 = 1;

    pub struct DaemonTerminal {
        pty: Arc<Mutex<tty::Pty>>,
        stopped: Arc<AtomicBool>,
        reader: Option<JoinHandle<()>>,
    }

    impl DaemonTerminal {
        pub fn open(
            cwd: &std::path::Path,
            cols: u16,
            rows: u16,
            events: EventSink,
        ) -> anyhow::Result<Self> {
            if !cwd.is_dir() {
                bail!(
                    "terminal working directory does not exist: {}",
                    cwd.display()
                );
            }

            let shell = crate::command_env::default_terminal_shell();
            let shell_args = crate::command_env::default_terminal_shell_args(&shell);
            let mut options = tty::Options {
                shell: Some(Shell::new(shell.to_string_lossy().into_owned(), shell_args)),
                working_directory: Some(cwd.to_owned()),
                drain_on_exit: false,
                ..Default::default()
            };
            for (name, value) in crate::command_env::shell_environment() {
                options.env.insert(
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                );
            }
            options.env.insert("TERM".into(), "xterm-256color".into());
            options.env.insert("COLORTERM".into(), "truecolor".into());

            let size = window_size(cols, rows);
            let pty = tty::new(&options, size, 0)
                .with_context(|| format!("spawn terminal in {}", cwd.display()))?;
            let mut output = pty.file().try_clone().context("clone terminal output")?;
            let pty = Arc::new(Mutex::new(pty));
            let stopped = Arc::new(AtomicBool::new(false));
            let reader_pty = pty.clone();
            let reader_stopped = stopped.clone();
            let reader = std::thread::Builder::new()
                .name("waku-daemon-terminal-output".into())
                .spawn(move || {
                    let mut buffer = [0_u8; 32 * 1024];
                    while !reader_stopped.load(Ordering::Acquire) {
                        match output.read(&mut buffer) {
                            Ok(0) => {
                                let _ = events.send_ephemeral(WireDriverEvent::new(
                                    "terminalExited",
                                    serde_json::Value::Null,
                                ));
                                break;
                            }
                            Ok(read) => {
                                let data = base64::engine::general_purpose::STANDARD
                                    .encode(&buffer[..read]);
                                let _ = events.send_ephemeral(WireDriverEvent::new(
                                    "terminalOutput",
                                    json!({ "data": data }),
                                ));
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::TimedOut
                                        | std::io::ErrorKind::Interrupted
                                ) =>
                            {
                                std::thread::sleep(Duration::from_millis(4));
                            }
                            Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                                // A PTY master may report EIO briefly before the
                                // freshly spawned child has attached its slave.
                                // Only treat it as EOF after Alacritty's SIGCHLD
                                // channel confirms the child actually exited.
                                if reader_pty.lock().next_child_event().is_some() {
                                    let _ = events.send_ephemeral(WireDriverEvent::new(
                                        "terminalExited",
                                        serde_json::Value::Null,
                                    ));
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(4));
                            }
                            Err(error) => {
                                let _ = events.send_ephemeral(WireDriverEvent::new(
                                    "terminalError",
                                    serde_json::Value::String(error.to_string()),
                                ));
                                break;
                            }
                        }
                    }
                })
                .context("start terminal output thread")?;

            Ok(Self {
                pty,
                stopped,
                reader: Some(reader),
            })
        }

        pub fn write(&self, data: Vec<u8>) -> anyhow::Result<()> {
            if data.is_empty() {
                return Ok(());
            }
            let mut pty = self.pty.lock();
            pty.writer()
                .write_all(&data)
                .context("write terminal input")?;
            pty.writer().flush().context("flush terminal input")
        }

        pub fn resize(&self, cols: u16, rows: u16) {
            self.pty.lock().on_resize(window_size(cols, rows));
        }
    }

    impl Drop for DaemonTerminal {
        fn drop(&mut self) {
            // Keep the owned child waitable until signaling is finished. A
            // shell may ignore HUP; Alacritty's Drop would then wait forever.
            // Keep reading output during shutdown: macOS can wait for PTY
            // output to drain while the shell exits.
            {
                let pty = self.pty.lock();
                let child = pty.child();
                if matches!(child_is_running(child), Ok(true)) {
                    unsafe {
                        libc::kill(child.id() as libc::pid_t, libc::SIGHUP);
                    }
                    let deadline = std::time::Instant::now() + Duration::from_secs(2);
                    while matches!(child_is_running(child), Ok(true)) {
                        if std::time::Instant::now() >= deadline {
                            unsafe {
                                libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
                            }
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
            self.stopped.store(true, Ordering::Release);
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    fn child_is_running(child: &std::process::Child) -> std::io::Result<bool> {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        let info = unsafe { info.assume_init() };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info.si_signo == 0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn terminal_close_bounds_a_shell_that_ignores_hangup() {
            let root =
                std::env::temp_dir().join(format!("waku-terminal-close-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let options = tty::Options {
                shell: Some(Shell::new(
                    "/bin/sh".into(),
                    vec![
                        "-c".into(),
                        "trap '' HUP; printf ready > ready; while [ ! -e release ]; do :; done"
                            .into(),
                    ],
                )),
                working_directory: Some(root.clone()),
                ..Default::default()
            };
            let pty = tty::new(&options, window_size(80, 24), 0).unwrap();
            let terminal = DaemonTerminal {
                pty: Arc::new(Mutex::new(pty)),
                stopped: Arc::new(AtomicBool::new(false)),
                reader: None,
            };
            // Also release the old unbounded implementation so its red test
            // finishes without leaving a fixture process behind.
            let release_root = root.clone();
            let release = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(4));
                std::fs::write(release_root.join("release"), "").unwrap();
            });
            let ready_deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !root.join("ready").exists() && std::time::Instant::now() < ready_deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            let ready = root.join("ready").exists();
            let started = std::time::Instant::now();
            drop(terminal);
            let elapsed = started.elapsed();
            release.join().unwrap();
            std::fs::remove_dir_all(root).unwrap();
            assert!(ready, "shell must install its HUP handler before close");
            assert!(
                elapsed < Duration::from_secs(3),
                "terminal close took {elapsed:?}"
            );
        }
    }

    fn window_size(cols: u16, rows: u16) -> WindowSize {
        WindowSize {
            num_lines: rows.max(MIN_ROWS),
            num_cols: cols.max(MIN_COLUMNS),
            cell_width: CELL_WIDTH,
            cell_height: CELL_HEIGHT,
        }
    }
}

#[cfg(unix)]
pub use platform::DaemonTerminal;

#[cfg(not(unix))]
pub struct DaemonTerminal;

#[cfg(not(unix))]
impl DaemonTerminal {
    pub fn open(_cwd: &Path, _cols: u16, _rows: u16, _events: EventSink) -> anyhow::Result<Self> {
        bail!("daemon terminals are not supported on this platform")
    }

    pub fn write(&self, _data: Vec<u8>) -> anyhow::Result<()> {
        bail!("daemon terminals are not supported on this platform")
    }

    pub fn resize(&self, _cols: u16, _rows: u16) {}
}
