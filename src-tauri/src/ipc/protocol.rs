//! Wire format for the CLI control socket.
//!
//! Newline-delimited JSON: one UTF-8 object per line. A frame that carries bulk
//! audio declares its byte length inside the frame itself
//! ([`AudioSource::Inline::byte_len`]); those raw bytes follow immediately after
//! the frame's newline, before the next JSON line. Keeping the length in the
//! payload rather than in a codec-level envelope means there is exactly one
//! authority on how many bytes to read.
//!
//! Transcripts containing newlines are safe — JSON escapes them, so framing can
//! never be broken by transcript content.
//!
//! Compatibility rules, so this can grow without a flag day:
//!
//! - Adding an enum variant or an `Option` field is backward compatible.
//! - Unknown [`ServerFrame`]s deserialize to [`ServerFrame::Unknown`], so an old
//!   client skips frames a newer server invented instead of dying.
//! - Unknown [`ClientFrame`]s are answered with [`ErrorCode::UnsupportedRequest`]
//!   naming the request, never by dropping the connection.
//! - [`PROTOCOL_VERSION`] bumps only on a breaking change. The server advertises
//!   `min_protocol` so it can serve older clients.

use crate::managers::engine_lease::LeaseOwner;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Read, Write};

/// Current wire version. Bump only for a breaking change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Oldest client this server still speaks to.
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// Largest inline audio payload accepted, ~2.3 hours of 16 kHz mono f32.
/// Beyond this the client is told to use `--server-read` or `--local`.
pub const MAX_ATTACHMENT_BYTES: u64 = 512 * 1024 * 1024;

/// Largest JSON control line accepted. Guards against a peer that opens a
/// connection and streams bytes without ever sending a newline.
pub const MAX_FRAME_BYTES: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Liveness check.
    Ping,
    /// What is the app doing right now.
    Status,
    /// Transcribe a buffer of audio.
    Transcribe(Box<TranscribeRequest>),
    /// Abandon the in-flight job on this connection.
    Cancel,
    /// Graceful close.
    Bye,
    /// A request kind this build does not know. Answered, not dropped.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// Sent immediately on accept, before the server reads anything, so version
    /// skew surfaces before any work is done.
    Hello {
        protocol: u32,
        min_protocol: u32,
        app_version: String,
        pid: u32,
    },
    /// The request was understood and queued.
    Accepted { job_id: String },
    /// Liveness heartbeat and stage transitions for a running job.
    Progress {
        job_id: String,
        stage: Stage,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        elapsed_ms: u64,
    },
    /// Terminal success frame.
    Result { job_id: String, body: ResultBody },
    /// Terminal failure frame.
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        job_id: Option<String>,
        code: ErrorCode,
        message: String,
    },
    /// A frame kind this build does not know. Skipped by the client.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscribeRequest {
    pub audio: AudioSource,
    /// Where the audio came from. Used for logs, history titles, and error
    /// messages — the server never opens it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_label: Option<String>,
    /// Load this model for the job instead of the one currently loaded. Not
    /// persisted; the previous model is restored afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Restore the previously loaded model after a `model` override.
    #[serde(default = "default_true")]
    pub restore_model: bool,
    /// Run the app's post-processing pipeline (Chinese conversion, command
    /// filter, LLM) over the transcript.
    #[serde(default)]
    pub post_process: bool,
    /// Record the result in the app's transcription history.
    #[serde(default)]
    pub save_history: bool,
    /// Paste the result into the focused window.
    #[serde(default)]
    pub paste: bool,
    /// What to do when the engine is busy.
    #[serde(default)]
    pub wait_policy: WaitPolicy,
    /// Send `Progress` frames while the job runs.
    #[serde(default = "default_true")]
    pub want_progress: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AudioSource {
    /// Audio bytes follow this frame. The client decodes the file, so the
    /// server never opens a caller-supplied path — that keeps the socket from
    /// being a file-read oracle, and sidesteps macOS TCC prompts against the
    /// GUI process for files the user's terminal can already read.
    Inline {
        encoding: AudioEncoding,
        sample_rate: u32,
        channels: u16,
        byte_len: u64,
    },
    /// Server-side read. Reserved for files the app itself owns (history
    /// recordings) and for an explicit opt-in.
    Path { path: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioEncoding {
    /// Little-endian 32-bit float, exactly what the engines consume.
    PcmF32Le,
    /// Little-endian signed 16-bit. Half the bytes, at 16-bit fidelity.
    PcmS16Le,
}

impl AudioEncoding {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            AudioEncoding::PcmF32Le => 4,
            AudioEncoding::PcmS16Le => 2,
        }
    }

    /// Decode a raw little-endian buffer into normalized samples.
    pub fn decode(self, bytes: &[u8]) -> Vec<f32> {
        match self {
            AudioEncoding::PcmF32Le => bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
            AudioEncoding::PcmS16Le => bytes
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / i16::MAX as f32)
                .collect(),
        }
    }

    /// Encode normalized samples into a raw little-endian buffer.
    pub fn encode(self, samples: &[f32]) -> Vec<u8> {
        match self {
            AudioEncoding::PcmF32Le => {
                let mut out = Vec::with_capacity(samples.len() * 4);
                for s in samples {
                    out.extend_from_slice(&s.to_le_bytes());
                }
                out
            }
            AudioEncoding::PcmS16Le => {
                let mut out = Vec::with_capacity(samples.len() * 2);
                for s in samples {
                    let clamped = (s * i16::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32);
                    out.extend_from_slice(&(clamped as i16).to_le_bytes());
                }
                out
            }
        }
    }
}

/// What a client wants done when the engine is already in use.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitPolicy {
    /// Fail immediately with [`ErrorCode::Busy`].
    FailFast,
    /// Queue for the engine, optionally giving up after `timeout_ms`.
    Wait {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
}

impl Default for WaitPolicy {
    /// Waiting is the kind default: the engine is usually free, and when it is
    /// not the wait is bounded by one in-flight job.
    fn default() -> Self {
        WaitPolicy::Wait { timeout_ms: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResultBody {
    Pong { app_version: String },
    Status(Box<StatusBody>),
    Transcript(Box<TranscriptBody>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptBody {
    /// Final text, post-processed if that was requested.
    pub text: String,
    /// Text as the engine produced it, before post-processing.
    pub raw_text: String,
    /// Whether post-processing was requested.
    pub post_processed: bool,
    /// Whether post-processing actually changed anything. The pipeline falls
    /// back to the raw text when a provider fails, so "requested" and "applied"
    /// are genuinely different answers.
    pub post_process_applied: bool,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub audio_secs: f64,
    /// Time spent queued for the engine.
    pub queue_ms: u64,
    /// Time spent loading a model, if one had to be loaded.
    pub load_ms: u64,
    pub transcribe_ms: u64,
    /// Audio seconds per wall-clock second.
    pub rtf: f64,
    /// True when the model was not resident and had to be loaded for this job,
    /// which usually means the unload timeout is set to Immediately.
    pub cold_load: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_id: Option<i64>,
    pub pasted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusBody {
    pub app_version: String,
    pub protocol: u32,
    pub pid: u32,
    pub uptime_secs: u64,
    pub selected_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loaded_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub model_loaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_busy: Option<LeaseOwner>,
    pub recording: bool,
    pub streaming: bool,
    pub post_process_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Queued,
    WaitingForEngine,
    LoadingModel,
    Transcribing,
    PostProcessing,
    SavingHistory,
    Pasting,
}

impl Stage {
    /// Short label for a progress line on stderr.
    pub fn label(self) -> &'static str {
        match self {
            Stage::Queued => "queued",
            Stage::WaitingForEngine => "waiting for engine",
            Stage::LoadingModel => "loading model",
            Stage::Transcribing => "transcribing",
            Stage::PostProcessing => "post-processing",
            Stage::SavingHistory => "saving to history",
            Stage::Pasting => "pasting",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtocolMismatch,
    UnsupportedRequest,
    BadRequest,
    PayloadTooLarge,
    Busy,
    Cancelled,
    NoModelSelected,
    ModelNotAvailable,
    ModelLoadFailed,
    TranscriptionFailed,
    IoError,
    Internal,
    TooManyClients,
    Forbidden,
}

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

/// Serialize `frame` as one JSON line.
pub fn write_frame<W: Write, F: Serialize>(w: &mut W, frame: &F) -> io::Result<()> {
    let line = serde_json::to_vec(frame)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    debug_assert!(
        !line.contains(&b'\n'),
        "serde_json must not emit raw newlines"
    );
    w.write_all(&line)?;
    w.write_all(b"\n")?;
    w.flush()
}

/// Serialize `frame`, then write `attachment` immediately after its newline.
/// The frame must itself declare `attachment.len()` so the peer knows to read it.
pub fn write_frame_with_attachment<W: Write, F: Serialize>(
    w: &mut W,
    frame: &F,
    attachment: &[u8],
) -> io::Result<()> {
    let line = serde_json::to_vec(frame)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    w.write_all(&line)?;
    w.write_all(b"\n")?;
    w.write_all(attachment)?;
    w.flush()
}

/// Read one JSON line. `Ok(None)` means the peer closed cleanly.
pub fn read_frame<R: BufRead, F: DeserializeOwned>(r: &mut R) -> io::Result<Option<F>> {
    let mut line = Vec::new();
    // Bound the line so a peer that never sends a newline cannot exhaust memory.
    let read = (&mut *r)
        .take(MAX_FRAME_BYTES)
        .read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if !line.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("control frame exceeded {} bytes", MAX_FRAME_BYTES),
        ));
    }
    let frame = serde_json::from_slice(&line)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad frame: {}", e)))?;
    Ok(Some(frame))
}

/// Read exactly `len` attachment bytes following a frame.
pub fn read_attachment<R: Read>(r: &mut R, len: u64) -> io::Result<Vec<u8>> {
    if len > MAX_ATTACHMENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "attachment of {} bytes exceeds the {} byte limit",
                len, MAX_ATTACHMENT_BYTES
            ),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn roundtrip<F: Serialize + DeserializeOwned>(frame: &F) -> F {
        let mut buf = Vec::new();
        write_frame(&mut buf, frame).unwrap();
        let mut reader = BufReader::new(Cursor::new(buf));
        read_frame(&mut reader).unwrap().unwrap()
    }

    #[test]
    fn transcript_with_newlines_and_cjk_survives_framing() {
        let text = "line one\nline two\r\n日本語のテキスト\ttab";
        let frame = ServerFrame::Result {
            job_id: "j1".into(),
            body: ResultBody::Transcript(Box::new(TranscriptBody {
                text: text.into(),
                raw_text: text.into(),
                post_processed: false,
                post_process_applied: false,
                model: "whisper-large".into(),
                backend: Some("metal".into()),
                audio_secs: 1196.03,
                queue_ms: 0,
                load_ms: 0,
                transcribe_ms: 1234,
                rtf: 4.2,
                cold_load: false,
                history_id: None,
                pasted: false,
            })),
        };
        match roundtrip(&frame) {
            ServerFrame::Result { body, .. } => match body {
                ResultBody::Transcript(t) => assert_eq!(t.text, text),
                other => panic!("wrong body: {other:?}"),
            },
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn attachment_rides_the_same_stream_as_the_next_frame() {
        let audio = vec![7u8; 5 * 1024 * 1024];
        let request = ClientFrame::Transcribe(Box::new(TranscribeRequest {
            audio: AudioSource::Inline {
                encoding: AudioEncoding::PcmF32Le,
                sample_rate: 16_000,
                channels: 1,
                byte_len: audio.len() as u64,
            },
            source_label: Some("rec.wav".into()),
            model: None,
            restore_model: true,
            post_process: false,
            save_history: false,
            paste: false,
            wait_policy: WaitPolicy::default(),
            want_progress: true,
        }));

        let mut buf = Vec::new();
        write_frame_with_attachment(&mut buf, &request, &audio).unwrap();
        // A control frame right behind the attachment must still parse.
        write_frame(&mut buf, &ClientFrame::Cancel).unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let first: ClientFrame = read_frame(&mut reader).unwrap().unwrap();
        let len = match &first {
            ClientFrame::Transcribe(req) => match &req.audio {
                AudioSource::Inline { byte_len, .. } => *byte_len,
                other => panic!("wrong source: {other:?}"),
            },
            other => panic!("wrong frame: {other:?}"),
        };
        assert_eq!(read_attachment(&mut reader, len).unwrap(), audio);
        assert!(matches!(
            read_frame::<_, ClientFrame>(&mut reader).unwrap().unwrap(),
            ClientFrame::Cancel
        ));
        assert!(read_frame::<_, ClientFrame>(&mut reader).unwrap().is_none());
    }

    #[test]
    fn unknown_server_frame_is_skipped_not_fatal() {
        let line = br#"{"type":"future_thing","whatever":[1,2,3]}"#;
        let mut buf = line.to_vec();
        buf.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(buf));
        let frame: ServerFrame = read_frame(&mut reader).unwrap().unwrap();
        assert!(matches!(frame, ServerFrame::Unknown));
    }

    #[test]
    fn unknown_client_frame_is_answerable_not_fatal() {
        let mut buf = br#"{"type":"teleport"}"#.to_vec();
        buf.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(buf));
        let frame: ClientFrame = read_frame(&mut reader).unwrap().unwrap();
        assert!(matches!(frame, ClientFrame::Unknown));
    }

    #[test]
    fn optional_fields_may_be_absent_from_an_older_peer() {
        let mut buf =
            br#"{"type":"transcribe","audio":{"kind":"path","path":"/tmp/a.wav"}}"#.to_vec();
        buf.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(buf));
        let frame: ClientFrame = read_frame(&mut reader).unwrap().unwrap();
        match frame {
            ClientFrame::Transcribe(req) => {
                assert!(req.want_progress, "want_progress should default to true");
                assert!(req.restore_model);
                assert!(!req.paste);
                assert!(matches!(req.wait_policy, WaitPolicy::Wait { .. }));
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn an_unterminated_flood_is_rejected_rather_than_buffered() {
        let flood = vec![b'x'; (MAX_FRAME_BYTES + 10) as usize];
        let mut reader = BufReader::new(Cursor::new(flood));
        let err = read_frame::<_, ClientFrame>(&mut reader).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversized_attachment_is_refused_before_allocating() {
        let mut reader = BufReader::new(Cursor::new(Vec::new()));
        let err = read_attachment(&mut reader, MAX_ATTACHMENT_BYTES + 1).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[test]
    fn pcm_encodings_round_trip() {
        let samples = vec![0.0f32, 0.5, -0.5, 1.0, -1.0];
        let f32_bytes = AudioEncoding::PcmF32Le.encode(&samples);
        assert_eq!(AudioEncoding::PcmF32Le.decode(&f32_bytes), samples);

        let s16_bytes = AudioEncoding::PcmS16Le.encode(&samples);
        let back = AudioEncoding::PcmS16Le.decode(&s16_bytes);
        for (got, want) in back.iter().zip(&samples) {
            assert!((got - want).abs() < 1e-4, "got {got}, want {want}");
        }
    }

    #[test]
    fn s16_encoding_clamps_instead_of_wrapping() {
        // A float slightly over full scale must saturate, not wrap to -32768.
        let bytes = AudioEncoding::PcmS16Le.encode(&[1.5, -1.5]);
        let back = AudioEncoding::PcmS16Le.decode(&bytes);
        assert!(back[0] > 0.99, "got {}", back[0]);
        assert!(back[1] < -0.99, "got {}", back[1]);
    }
}
