//! Command-line surface.
//!
//! Two shapes coexist:
//!
//! - The original flat flags (`--toggle-transcription`, `-f FILE`, …), which
//!   are documented and live in people's window-manager configs, so they parse
//!   exactly as they always have.
//! - Client subcommands (`handy status`) and the bare form `handy FILE.wav`,
//!   which talk to a running instance over the control socket.
//!
//! The two collide in clap: with an optional subcommand, `handy foo.wav` fails
//! with `InvalidSubcommand`, because clap resolves the first token against
//! subcommand names before positionals. The way out — the one `cargo` uses — is
//! an external-subcommand catch-all that captures unrecognised leading tokens
//! verbatim, which [`client::dispatch`] then re-parses as a `transcribe`.

pub mod client;
#[cfg(windows)]
pub mod console_win;

use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone, Default)]
#[command(
    name = "handy",
    about = "Handy - Speech to Text",
    after_help = "Run `handy FILE.wav` to transcribe a file with the running instance,\n\
                  or `handy -f FILE.wav` to transcribe it in this process instead."
)]
pub struct CliArgs {
    /// Client subcommand, or a bare file path to transcribe.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Start with the main window hidden
    #[arg(long)]
    pub start_hidden: bool,

    /// Disable the system tray icon
    #[arg(long)]
    pub no_tray: bool,

    /// Toggle transcription on/off (sent to running instance)
    #[arg(long)]
    pub toggle_transcription: bool,

    /// Toggle transcription with post-processing on/off (sent to running instance)
    #[arg(long)]
    pub toggle_post_process: bool,

    /// Cancel the current operation (sent to running instance)
    #[arg(long)]
    pub cancel: bool,

    /// Enable debug mode with verbose logging
    #[arg(long)]
    pub debug: bool,

    /// Transcribe this WAV headlessly and exit. Any bit depth, channel count,
    /// and sample rate are converted to 16 kHz mono. Runs the same batch
    /// transcription path as the app — no mic, no VAD, no download (the model
    /// must already be installed).
    #[arg(short = 'f', long, value_name = "WAV")]
    pub transcribe_file: Option<PathBuf>,

    /// Model id to load for --transcribe-file (default: the selected model).
    #[arg(long)]
    pub model: Option<String>,

    /// Hard-select the compute device for --transcribe-file by its registry
    /// index (see --list-devices). Omit to use the persisted accelerator
    /// setting. transcribe-cpp (whisper-family) models only.
    #[arg(long, value_name = "N")]
    pub device_index: Option<usize>,

    /// List the transcribe-cpp compute devices (with indices) and exit.
    #[arg(long)]
    pub list_devices: bool,

    /// List the available models (with ids) and exit. Pass an id to --model.
    /// Honors --json for machine-readable output.
    #[arg(long)]
    pub list_models: bool,

    /// Repeat the transcription N times (best_ms reports the fastest run).
    #[arg(long, value_name = "N")]
    pub repeat: Option<usize>,

    /// Emit --transcribe-file results as JSON.
    #[arg(long)]
    pub json: bool,

    /// Do not listen on the CLI control socket (disables `handy FILE.wav`
    /// and `handy status` against this instance).
    #[arg(long)]
    pub no_ipc: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Transcribe an audio file using the running instance's loaded model.
    Transcribe(TranscribeArgs),
    /// Report what the running instance is doing.
    Status(CommonArgs),
    /// Check that the running instance answers.
    Ping(CommonArgs),
    /// `handy FILE.wav [flags]` — captured verbatim and re-parsed as `transcribe`.
    #[command(external_subcommand)]
    Bare(Vec<OsString>),
}

/// Flags every client subcommand accepts.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct CommonArgs {
    /// Emit the result as JSON.
    #[arg(long)]
    pub json: bool,

    /// Give up if the running instance sends nothing for this many seconds.
    /// Bounds silence, not total runtime — a long transcription is fine.
    #[arg(long, value_name = "SECS", default_value_t = 120)]
    pub idle_timeout: u64,

    /// Suppress progress and notes on stderr.
    #[arg(long, short = 'q')]
    pub quiet: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct TranscribeArgs {
    /// Audio file to transcribe.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    #[command(flatten)]
    pub common: CommonArgs,

    /// Also run LLM post-processing, using your configured provider. Makes a
    /// network call. The command filter and variant conversion run either way.
    #[arg(long, alias = "post-process")]
    pub llm_post_process: bool,

    /// Print the engine's output untouched: no command filter, no variant
    /// conversion, no LLM. Useful when a script wants exactly what the model
    /// produced rather than what a dictation would have pasted.
    #[arg(long, conflicts_with = "llm_post_process")]
    pub raw: bool,

    /// Paste the transcript into the focused window. Note that the focused
    /// window is usually the terminal you ran this from.
    #[arg(long)]
    pub paste: bool,

    /// Record the result in Handy's transcription history.
    #[arg(long, overrides_with = "no_history")]
    pub history: bool,

    /// Keep the result out of history (the default).
    #[arg(long, overrides_with = "history")]
    pub no_history: bool,

    /// Use this model instead of the loaded one. Temporary and not persisted;
    /// the previous model is restored afterwards.
    #[arg(long)]
    pub model: Option<String>,

    /// Leave the --model override loaded instead of restoring the previous one.
    #[arg(long)]
    pub no_restore_model: bool,

    /// Print every protocol frame as JSON, one per line, terminal frame last.
    #[arg(long, conflicts_with = "json")]
    pub ndjson: bool,

    /// Transcribe in this process instead of connecting, loading a private copy
    /// of the model. Equivalent to `handy -f FILE`.
    #[arg(long)]
    pub local: bool,

    /// What to do when the running instance's engine is busy.
    #[arg(long, value_name = "MODE", default_value = "wait")]
    pub if_busy: IfBusy,

    /// Exit non-zero when the transcript is empty.
    #[arg(long)]
    pub fail_on_empty: bool,

    /// Show progress on stderr even when it is not a terminal.
    #[arg(long, overrides_with = "no_progress")]
    pub progress: bool,

    /// Never show progress on stderr.
    #[arg(long, overrides_with = "progress")]
    pub no_progress: bool,

    /// Use the batch engine even when the model can stream. Streaming is the
    /// default because it is the only path that reports a real completion
    /// percentage, but it commits text incrementally, so its transcript can
    /// differ slightly from `handy -f` for the same audio.
    #[arg(long)]
    pub no_stream: bool,

    /// Print text on stderr as it is decoded (streaming models only).
    #[arg(long)]
    pub partials: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "kebab-case")]
pub enum IfBusy {
    /// Queue behind the current job. Dictations still take priority.
    Wait,
    /// Exit immediately with the busy status.
    Fail,
    /// Fall back to transcribing in this process.
    Local,
}
