//! Everything the Handy CLI and the Handy app must agree on.
//!
//! This crate exists so `handy-cli` can be a small, fast, console-subsystem
//! binary. The app links transcribe-cpp, ONNX Runtime and Tauri; a CLI that
//! shares its crate links all of that too, whether it calls into it or not,
//! because the native libraries are pulled in unconditionally by build
//! scripts. Splitting the wire protocol and the file decoder out here is what
//! breaks that.
//!
//! The hard rule: **nothing in this crate may depend on tauri, transcribe-cpp,
//! transcribe-rs, or cpal.** If something here needs one of those, it belongs
//! in the app instead — which is why the IPC *server* lives in the app and only
//! the client lives here.

pub mod audio;
pub mod client;
pub mod endpoint;
pub mod protocol;
pub mod transport;

pub use audio::{decode_to_16k_mono, decode_wav_reader, DecodedAudio, FrameResampler};
pub use protocol::{LeaseOwner, PROTOCOL_VERSION};

/// Sample rate every transcription engine expects.
pub const WHISPER_SAMPLE_RATE: u32 = 16000;
