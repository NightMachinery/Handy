//! Blocking client for the control socket.
//!
//! Deliberately free of Tauri: the CLI process never builds an app, never loads
//! a model, and never initializes the logger, so nothing can leak onto stdout
//! except the result the user asked for.

use crate::endpoint;
use crate::protocol::*;
use crate::transport;
use anyhow::{bail, Context, Result};
use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Why a connection attempt failed, so the CLI can pick an exit code and give
/// advice specific to the cause.
#[derive(Debug)]
pub enum ConnectError {
    /// Nothing is listening.
    NotRunning {
        endpoint: String,
        detail: String,
    },
    /// A running app speaks a protocol this build cannot.
    ProtocolMismatch {
        app_version: String,
        server_protocol: u32,
        server_min: u32,
        client_protocol: u32,
    },
    /// The endpoint directory is not safely ours.
    Forbidden(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NotRunning { endpoint, detail } => write!(
                f,
                "no running Handy instance (nothing listening at {endpoint}: {detail})\n  \
                 start Handy, or pass --local to transcribe in this process"
            ),
            ConnectError::ProtocolMismatch {
                app_version,
                server_protocol,
                server_min,
                client_protocol,
            } => write!(
                f,
                "the running Handy ({app_version}) speaks CLI protocol \
                 {server_protocol} (accepts >= {server_min}), this build speaks \
                 {client_protocol}\n  restart Handy so both sides match"
            ),
            ConnectError::Forbidden(detail) => write!(
                f,
                "refusing to use the control socket: {detail}\n  \
                 fix the directory's ownership and permissions, then retry"
            ),
            ConnectError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

/// A connected, handshaken session.
pub struct Session {
    reader: BufReader<Box<dyn Read + Send>>,
    writer: Box<dyn Write + Send>,
    pub app_version: String,
    pub server_pid: u32,
}

impl Session {
    /// Connect and complete the handshake.
    pub fn connect() -> Result<Self, ConnectError> {
        let endpoint_name = endpoint::transport_name().unwrap_or_else(|_| "<unknown>".to_string());

        // A descriptor from a dead pid means the app crashed; say so plainly
        // rather than reporting a confusing connection refusal.
        if let Ok(Some(descriptor)) = endpoint::read_descriptor() {
            if !endpoint::process_is_alive(descriptor.pid) {
                return Err(ConnectError::NotRunning {
                    endpoint: endpoint_name,
                    detail: format!("Handy (pid {}) is no longer running", descriptor.pid),
                });
            }
            if PROTOCOL_VERSION < descriptor.min_protocol {
                return Err(ConnectError::ProtocolMismatch {
                    app_version: descriptor.app_version,
                    server_protocol: descriptor.protocol,
                    server_min: descriptor.min_protocol,
                    client_protocol: PROTOCOL_VERSION,
                });
            }
        }

        let conn = transport::connect().map_err(|e| classify_connect_error(&endpoint_name, e))?;
        let (rx, writer) = conn.split();
        let mut session = Session {
            reader: BufReader::new(Box::new(rx) as Box<dyn Read + Send>),
            writer: Box::new(writer),
            app_version: String::new(),
            server_pid: 0,
        };

        match session.read_frame() {
            Ok(Some(ServerFrame::Hello {
                protocol,
                min_protocol,
                app_version,
                pid,
            })) => {
                // Only a client below the server's floor is fatal. A *newer*
                // server is fine: unknown frames deserialize to
                // ServerFrame::Unknown and are skipped. An *older* server is
                // fine too: it answers UnsupportedRequest for anything it does
                // not know, which the CLI reports with advice.
                if PROTOCOL_VERSION < min_protocol {
                    return Err(ConnectError::ProtocolMismatch {
                        app_version,
                        server_protocol: protocol,
                        server_min: min_protocol,
                        client_protocol: PROTOCOL_VERSION,
                    });
                }
                session.app_version = app_version;
                session.server_pid = pid;
                Ok(session)
            }
            Ok(Some(ServerFrame::Error { code, message, .. })) => {
                Err(ConnectError::Other(anyhow::anyhow!("{code:?}: {message}")))
            }
            Ok(_) => Err(ConnectError::Other(anyhow::anyhow!(
                "the running Handy did not greet us; it may be an older build"
            ))),
            Err(e) => Err(ConnectError::Other(e)),
        }
    }

    pub fn send(&mut self, frame: &ClientFrame) -> Result<()> {
        write_frame(&mut self.writer, frame).context("sending a request")
    }

    pub fn send_with_audio(&mut self, frame: &ClientFrame, audio: &[u8]) -> Result<()> {
        write_frame_with_attachment(&mut self.writer, frame, audio).context("sending audio")
    }

    pub fn read_frame(&mut self) -> Result<Option<ServerFrame>> {
        read_frame(&mut self.reader).context("reading a reply")
    }

    /// Ask the server to abandon the running job. Best effort.
    pub fn cancel(&mut self) {
        let _ = self.send(&ClientFrame::Cancel);
    }

    /// Read frames until a terminal one arrives, handing each to `on_frame`.
    ///
    /// There is deliberately no total timeout: a 20-minute transcription is
    /// normal. The deadline that matters is the gap *between* frames, and since
    /// socket reads block, that is enforced by [`IdleWatchdog`] on its own
    /// thread — call [`IdleWatchdog::tick`] from `on_frame`.
    pub fn wait_for_result(
        &mut self,
        mut on_frame: impl FnMut(&ServerFrame),
    ) -> Result<ServerFrame> {
        loop {
            let Some(frame) = self.read_frame()? else {
                bail!("the connection closed before a result arrived");
            };
            on_frame(&frame);
            match frame {
                ServerFrame::Result { .. } | ServerFrame::Error { .. } => return Ok(frame),
                // Unknown frames come from a newer server; skipping them is the
                // whole point of the forward-compat rule.
                _ => continue,
            }
        }
    }
}

/// Trip `cancel` if no frame arrives within `idle_timeout`.
///
/// Returns a handle that must be `tick()`ed on every received frame. Runs the
/// clock on its own thread because socket reads block.
pub struct IdleWatchdog {
    last_seen: Arc<std::sync::Mutex<std::time::Instant>>,
    stop: Arc<AtomicBool>,
    pub tripped: Arc<AtomicBool>,
}

impl IdleWatchdog {
    pub fn spawn(idle_timeout: Duration, on_trip: impl FnOnce() + Send + 'static) -> Self {
        let last_seen = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        let stop = Arc::new(AtomicBool::new(false));
        let tripped = Arc::new(AtomicBool::new(false));

        let last_for_thread = Arc::clone(&last_seen);
        let stop_for_thread = Arc::clone(&stop);
        let tripped_for_thread = Arc::clone(&tripped);
        thread::spawn(move || {
            let mut on_trip = Some(on_trip);
            while !stop_for_thread.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(500));
                let elapsed = last_for_thread
                    .lock()
                    .map(|t| t.elapsed())
                    .unwrap_or_default();
                if elapsed > idle_timeout && !stop_for_thread.load(Ordering::Relaxed) {
                    tripped_for_thread.store(true, Ordering::Relaxed);
                    if let Some(f) = on_trip.take() {
                        f();
                    }
                    return;
                }
            }
        });

        Self {
            last_seen,
            stop,
            tripped,
        }
    }

    pub fn tick(&self) {
        if let Ok(mut t) = self.last_seen.lock() {
            *t = std::time::Instant::now();
        }
    }
}

impl Drop for IdleWatchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Install a Ctrl-C handler that cancels the running job once, then lets a
/// second press kill the process.
///
/// Even without this, the server notices the disconnect and stops — this just
/// makes the common case graceful and lets the client print a clean message.
pub fn spawn_interrupt_watcher() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    #[cfg(unix)]
    {
        use signal_hook::consts::SIGINT;
        use signal_hook::iterator::Signals;
        if let Ok(mut signals) = Signals::new([SIGINT]) {
            thread::spawn(move || {
                let mut first = true;
                for _ in signals.forever() {
                    if first {
                        first = false;
                        let _ = tx.send(());
                    } else {
                        std::process::exit(130);
                    }
                }
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = &tx;
    }
    rx
}

fn classify_connect_error(endpoint_name: &str, err: anyhow::Error) -> ConnectError {
    let text = format!("{err:#}");
    if text.contains("owned by uid") || text.contains("accessible to other users") {
        return ConnectError::Forbidden(text);
    }
    if let Some(io) = err.downcast_ref::<std::io::Error>() {
        use std::io::ErrorKind::*;
        if matches!(io.kind(), NotFound | ConnectionRefused | AddrNotAvailable) {
            return ConnectError::NotRunning {
                endpoint: endpoint_name.to_string(),
                detail: io.to_string(),
            };
        }
    }
    // interprocess wraps the io::Error, so fall back to matching the message.
    if text.contains("No such file") || text.contains("Connection refused") {
        return ConnectError::NotRunning {
            endpoint: endpoint_name.to_string(),
            detail: text,
        };
    }
    ConnectError::Other(err)
}
