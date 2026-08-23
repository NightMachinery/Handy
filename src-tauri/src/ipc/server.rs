//! The control socket server.
//!
//! One accept thread, two threads per connection (a reader and the
//! dispatcher), and a thread per engine-bound job. Cheap requests (`Ping`,
//! `Status`) are answered inline on the dispatcher, so they stay responsive
//! while a long transcription runs.
//!
//! There is deliberately no job queue in front of the engine. The engine lease
//! is already the thing that serializes access, and layering a queue on top
//! put the "who goes next" decision in two places: a fail-fast request would
//! sit in the queue behind a running job and time out, never reaching the
//! lease that would have told it immediately that the engine was busy.
//!
//! The reader runs separately from the dispatcher so cancellation is prompt: a
//! `Cancel` frame, or the client simply dying, is noticed while the job is
//! still running rather than after it. Reader events and job completion arrive
//! on the same channel, so the dispatcher waits on one blocking `recv` instead
//! of polling.

use super::endpoint::{self, Descriptor, DESCRIPTOR_SCHEMA};
use super::handlers;
use super::protocol::*;
use super::transport::{self, Connection};
use crate::managers::history::HistoryManager;
use crate::managers::model::ModelManager;
use crate::managers::transcription::TranscriptionManager;
use log::{error, info, warn};
use std::io::{BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

/// Simultaneous connections. Well above any real use; a backstop against a
/// runaway script opening sockets in a loop.
const MAX_CLIENTS: usize = 8;

/// How often a running job emits a liveness heartbeat. Long transcriptions
/// would otherwise look hung, and the client uses these to drive an *idle*
/// timeout rather than a total one.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Shared handle to the connection's write half.
type Out = Arc<Mutex<Box<dyn Write + Send>>>;

/// Shared state a handler needs.
pub struct ServerContext {
    pub app: AppHandle,
    pub transcription: Arc<TranscriptionManager>,
    pub models: Arc<ModelManager>,
    pub history: Arc<HistoryManager>,
    pub started_at: Instant,
}

/// Everything a running job needs to report progress and notice cancellation.
pub struct JobContext {
    pub ctx: Arc<ServerContext>,
    pub job_id: String,
    pub cancel: Arc<AtomicBool>,
    started: Instant,
    want_progress: bool,
    out: Out,
}

impl JobContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Bail out of a handler if the client has gone away.
    pub fn check_cancelled(&self) -> Result<(), HandlerError> {
        if self.is_cancelled() {
            Err(HandlerError::new(
                ErrorCode::Cancelled,
                "job cancelled by the client",
            ))
        } else {
            Ok(())
        }
    }

    pub fn progress(&self, stage: Stage, message: Option<String>) {
        self.progress_with_fraction(stage, message, None);
    }

    /// Report progress with a measured completion fraction, where one exists.
    pub fn progress_with_fraction(
        &self,
        stage: Stage,
        message: Option<String>,
        fraction: Option<f64>,
    ) {
        if !self.want_progress {
            return;
        }
        send(
            &self.out,
            &ServerFrame::Progress {
                job_id: self.job_id.clone(),
                stage,
                message,
                elapsed_ms: self.started.elapsed().as_millis() as u64,
                fraction,
            },
        );
    }

    /// Emit text decoded so far. Only sent when the client asked for partials.
    pub fn partial(&self, committed: &str, tentative: &str) {
        send(
            &self.out,
            &ServerFrame::Partial {
                job_id: self.job_id.clone(),
                committed: committed.to_string(),
                tentative: tentative.to_string(),
            },
        );
    }
}

/// A handler failure, carrying the code the client should see.
#[derive(Debug)]
pub struct HandlerError {
    pub code: ErrorCode,
    pub message: String,
}

impl HandlerError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type HandlerResult = Result<ResultBody, HandlerError>;

/// What the dispatcher waits on. Reader events and job completion share a
/// channel so the dispatcher never has to poll.
enum Event {
    /// A parsed request, with its inline audio already consumed from the stream.
    Frame(Box<ClientFrame>, Option<Vec<f32>>),
    /// The reader hit EOF, a parse error, or a protocol violation.
    Closed(Option<HandlerError>),
    /// The running job finished (successfully or not).
    JobDone,
}

/// Handle kept alive for the life of the app.
pub struct IpcServer {
    _accept: thread::JoinHandle<()>,
}

/// Start the control socket.
pub fn start(app: &AppHandle) -> anyhow::Result<IpcServer> {
    let listener = transport::bind()?;

    let ctx = Arc::new(ServerContext {
        app: app.clone(),
        transcription: app.state::<Arc<TranscriptionManager>>().inner().clone(),
        models: app.state::<Arc<ModelManager>>().inner().clone(),
        history: app.state::<Arc<HistoryManager>>().inner().clone(),
        started_at: Instant::now(),
    });

    endpoint::write_descriptor(&Descriptor {
        schema: DESCRIPTOR_SCHEMA,
        protocol: PROTOCOL_VERSION,
        min_protocol: MIN_PROTOCOL_VERSION,
        app_version: app.package_info().version.to_string(),
        pid: std::process::id(),
        endpoint: endpoint::transport_name()?,
    })?;

    let clients = Arc::new(AtomicU64::new(0));
    let accept = thread::Builder::new()
        .name("handy-ipc-accept".into())
        .spawn(move || {
            let uid = current_uid();
            loop {
                let conn = match listener.accept() {
                    Ok(conn) => conn,
                    Err(e) => {
                        error!("CLI control socket accept failed: {:#}", e);
                        // A persistent accept error would otherwise spin hot.
                        thread::sleep(Duration::from_millis(250));
                        continue;
                    }
                };

                // Second line of defence behind the 0700 directory.
                if let (Some(peer), Some(uid)) = (conn.peer_uid(), uid) {
                    if peer != uid {
                        warn!(
                            "Rejecting CLI connection from uid {} (expected {})",
                            peer, uid
                        );
                        continue;
                    }
                }

                if clients.load(Ordering::Relaxed) as usize >= MAX_CLIENTS {
                    warn!("Refusing CLI connection: {} already connected", MAX_CLIENTS);
                    refuse(conn, ErrorCode::TooManyClients, "too many CLI connections");
                    continue;
                }

                clients.fetch_add(1, Ordering::Relaxed);
                let ctx = Arc::clone(&ctx);
                let clients_for_thread = Arc::clone(&clients);
                let spawned =
                    thread::Builder::new()
                        .name("handy-ipc-conn".into())
                        .spawn(move || {
                            serve_connection(conn, ctx);
                            clients_for_thread.fetch_sub(1, Ordering::Relaxed);
                        });
                if spawned.is_err() {
                    clients.fetch_sub(1, Ordering::Relaxed);
                    error!("Failed to spawn a CLI connection thread");
                }
            }
        })?;

    Ok(IpcServer { _accept: accept })
}

/// Tell a client why it is being dropped, then drop it.
fn refuse(conn: Connection, code: ErrorCode, message: &str) {
    let (_rx, mut tx) = conn.split();
    let _ = write_frame(
        &mut tx,
        &ServerFrame::Error {
            job_id: None,
            code,
            message: message.to_string(),
        },
    );
}

fn serve_connection(conn: Connection, ctx: Arc<ServerContext>) {
    let (rx, tx) = conn.split();
    let out: Out = Arc::new(Mutex::new(Box::new(tx)));

    // Greet before reading anything, so version skew is visible to the client
    // without it having to send a request first.
    let hello = ServerFrame::Hello {
        protocol: PROTOCOL_VERSION,
        min_protocol: MIN_PROTOCOL_VERSION,
        app_version: ctx.app.package_info().version.to_string(),
        pid: std::process::id(),
    };
    if !send(&out, &hello) {
        return;
    }

    let (ev_tx, ev_rx) = mpsc::channel::<Event>();
    let reader_tx = ev_tx.clone();
    let reader = thread::Builder::new()
        .name("handy-ipc-read".into())
        .spawn(move || run_reader(rx, reader_tx));
    if reader.is_err() {
        error!("Failed to spawn a CLI reader thread");
        return;
    }

    while let Ok(event) = ev_rx.recv() {
        match event {
            Event::Closed(err) => {
                if let Some(err) = err {
                    send(
                        &out,
                        &ServerFrame::Error {
                            job_id: None,
                            code: err.code,
                            message: err.message,
                        },
                    );
                }
                return;
            }
            // No job is awaiting; a completion from a job we already stopped
            // waiting on is not interesting.
            Event::JobDone => continue,
            Event::Frame(frame, audio) => {
                if dispatch(*frame, audio, &ctx, &out, &ev_rx, &ev_tx).is_break() {
                    return;
                }
            }
        }
    }
}

/// Handle one request. Returns `Break` when the connection should close.
fn dispatch(
    frame: ClientFrame,
    audio: Option<Vec<f32>>,
    ctx: &Arc<ServerContext>,
    out: &Out,
    ev_rx: &mpsc::Receiver<Event>,
    ev_tx: &mpsc::Sender<Event>,
) -> std::ops::ControlFlow<()> {
    use std::ops::ControlFlow::{Break, Continue};

    match frame {
        ClientFrame::Bye => Break(()),
        // Nothing is running, so there is nothing to cancel.
        ClientFrame::Cancel => Continue(()),
        ClientFrame::Ping => {
            send(
                out,
                &ServerFrame::Result {
                    job_id: next_job_id(),
                    body: ResultBody::Pong {
                        app_version: ctx.app.package_info().version.to_string(),
                    },
                },
            );
            Continue(())
        }
        ClientFrame::Status => {
            let job_id = next_job_id();
            let frame = match handlers::status::handle(ctx) {
                Ok(body) => ServerFrame::Result { job_id, body },
                Err(e) => ServerFrame::Error {
                    job_id: Some(job_id),
                    code: e.code,
                    message: e.message,
                },
            };
            send(out, &frame);
            Continue(())
        }
        ClientFrame::Unknown => {
            send(
                out,
                &ServerFrame::Error {
                    job_id: None,
                    code: ErrorCode::UnsupportedRequest,
                    message: "this Handy build does not support that request; \
                              update and restart Handy"
                        .into(),
                },
            );
            Continue(())
        }
        ClientFrame::Transcribe(request) => {
            let job_id = next_job_id();
            send(
                out,
                &ServerFrame::Accepted {
                    job_id: job_id.clone(),
                },
            );

            let cancel = Arc::new(AtomicBool::new(false));
            let job = JobContext {
                ctx: Arc::clone(ctx),
                job_id: job_id.clone(),
                cancel: Arc::clone(&cancel),
                started: Instant::now(),
                want_progress: request.want_progress,
                out: Arc::clone(out),
            };
            job.progress(Stage::Queued, None);

            let out_for_job = Arc::clone(out);
            let done_tx = ev_tx.clone();
            // A thread per job, contending on the engine lease. The lease is
            // what enforces exclusion and dictation priority; a queue in front
            // of it would only re-decide the same question, and would stop a
            // fail-fast request from ever reaching it.
            let spawned = thread::Builder::new()
                .name("handy-ipc-job".into())
                .spawn(move || {
                    let stop_heartbeat = spawn_heartbeat(&job);
                    let outcome = handlers::transcribe::handle(&job, *request, audio);
                    stop_heartbeat.store(true, Ordering::Relaxed);
                    let frame = match outcome {
                        Ok(body) => ServerFrame::Result {
                            job_id: job.job_id.clone(),
                            body,
                        },
                        Err(e) => ServerFrame::Error {
                            job_id: Some(job.job_id.clone()),
                            code: e.code,
                            message: e.message,
                        },
                    };
                    send(&out_for_job, &frame);
                    let _ = done_tx.send(Event::JobDone);
                });

            if spawned.is_err() {
                send(
                    out,
                    &ServerFrame::Error {
                        job_id: Some(job_id),
                        code: ErrorCode::Internal,
                        message: "could not start a transcription worker".into(),
                    },
                );
                return Break(());
            }

            await_job(&cancel, ev_rx)
        }
    }
}

/// Block until the running job completes, honouring cancellation meanwhile.
///
/// On client disconnect the job is cancelled but still awaited, so a stream of
/// abandoned clients cannot pile jobs up behind each other.
fn await_job(cancel: &Arc<AtomicBool>, ev_rx: &mpsc::Receiver<Event>) -> std::ops::ControlFlow<()> {
    use std::ops::ControlFlow::{Break, Continue};
    let mut client_gone = false;
    loop {
        match ev_rx.recv() {
            Ok(Event::JobDone) => {
                return if client_gone { Break(()) } else { Continue(()) };
            }
            Ok(Event::Closed(_)) => {
                cancel.store(true, Ordering::Relaxed);
                client_gone = true;
            }
            Ok(Event::Frame(frame, _)) => match *frame {
                ClientFrame::Cancel | ClientFrame::Bye => {
                    cancel.store(true, Ordering::Relaxed);
                }
                // A second request while one is in flight: ignore it rather
                // than interleaving. The client protocol is one job per
                // connection.
                _ => {}
            },
            // Reader gone and job channel dropped: nothing more can arrive.
            Err(_) => return Break(()),
        }
    }
}

/// Read frames (and their audio payloads) until the peer goes away.
fn run_reader<R: std::io::Read>(rx: R, ev_tx: mpsc::Sender<Event>) {
    let mut reader = BufReader::new(rx);
    loop {
        let frame: ClientFrame = match read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                let _ = ev_tx.send(Event::Closed(None));
                return;
            }
            Err(e) => {
                let _ = ev_tx.send(Event::Closed(Some(HandlerError::new(
                    ErrorCode::BadRequest,
                    format!("could not parse request: {}", e),
                ))));
                return;
            }
        };

        // Inline audio must be consumed here, in stream order, before the next
        // control frame can be read — regardless of how long the job queue is.
        let audio = match &frame {
            ClientFrame::Transcribe(request) => {
                match read_inline_audio(&mut reader, &request.audio) {
                    Ok(audio) => audio,
                    Err(e) => {
                        let _ = ev_tx.send(Event::Closed(Some(e)));
                        return;
                    }
                }
            }
            _ => None,
        };

        if ev_tx.send(Event::Frame(Box::new(frame), audio)).is_err() {
            return;
        }
    }
}

/// Emit a heartbeat every few seconds until the returned flag is set.
fn spawn_heartbeat(job: &JobContext) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    if !job.want_progress {
        return stop;
    }
    let stop_for_thread = Arc::clone(&stop);
    let out = Arc::clone(&job.out);
    let job_id = job.job_id.clone();
    let started = job.started;
    let transcription = Arc::clone(&job.ctx.transcription);
    thread::spawn(move || {
        while !stop_for_thread.load(Ordering::Relaxed) {
            thread::sleep(HEARTBEAT_INTERVAL);
            if stop_for_thread.load(Ordering::Relaxed) {
                break;
            }
            // Tell the client when its job is holding up a dictation, so a
            // script can see why the app feels stuck.
            let message = transcription
                .engine_has_interactive_waiters()
                .then(|| "a dictation is waiting behind this job".to_string());
            send(
                &out,
                &ServerFrame::Progress {
                    job_id: job_id.clone(),
                    stage: Stage::Transcribing,
                    message,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    // The heartbeat knows no fraction. A streaming job sends its
                    // own measured ones alongside; the client keeps the last it
                    // saw so the two do not fight over the display.
                    fraction: None,
                },
            );
        }
    });
    stop
}

fn read_inline_audio<R: std::io::BufRead>(
    reader: &mut R,
    source: &AudioSource,
) -> Result<Option<Vec<f32>>, HandlerError> {
    let AudioSource::Inline {
        encoding,
        sample_rate,
        channels,
        byte_len,
    } = source
    else {
        return Ok(None);
    };

    if *byte_len > MAX_ATTACHMENT_BYTES {
        return Err(HandlerError::new(
            ErrorCode::PayloadTooLarge,
            format!(
                "audio payload of {} bytes exceeds the {} byte limit; \
                 use --local for very long files",
                byte_len, MAX_ATTACHMENT_BYTES
            ),
        ));
    }
    // The client converts; anything else is a bug on its side, and quietly
    // accepting it here would mis-time the audio instead of failing.
    if *sample_rate != crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE || *channels != 1 {
        return Err(HandlerError::new(
            ErrorCode::BadRequest,
            format!(
                "inline audio must be 16000 Hz mono, got {} Hz / {} ch",
                sample_rate, channels
            ),
        ));
    }
    let bytes = read_attachment(reader, *byte_len).map_err(|e| {
        HandlerError::new(ErrorCode::IoError, format!("reading audio payload: {}", e))
    })?;
    Ok(Some(encoding.decode(&bytes)))
}

/// Write a frame. Returns false if the peer is gone — expected, not exceptional.
fn send(out: &Out, frame: &ServerFrame) -> bool {
    let mut guard = match out.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    write_frame(&mut *guard, frame).is_ok()
}

fn next_job_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn current_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Remove the socket and descriptor. Called from the app's exit hook.
pub fn shutdown() {
    info!("Removing the CLI control socket");
    endpoint::cleanup();
}
