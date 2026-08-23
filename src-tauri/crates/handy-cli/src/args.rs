//! Argument surface for the standalone CLI.
//!
//! Only the client-facing verbs are defined here. Flags that belong to the
//! application — `--toggle-transcription`, `-f`, `--start-hidden` and friends —
//! are deliberately *not* modelled: [`is_app_invocation`] spots them in argv
//! and the whole command line is handed to the app binary untouched. That way
//! there is exactly one definition of those flags (the app's), and no risk of
//! the two drifting apart.

use clap::{Parser, Subcommand};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// Flags that mean "this is the app, not the CLI".
///
/// `--model` and `--json` are absent on purpose: both sides accept them, so
/// they carry no signal about which binary should handle the invocation.
const APP_FLAGS: &[&str] = &[
    "--toggle-transcription",
    "--toggle-post-process",
    "--cancel",
    "--start-hidden",
    "--no-tray",
    "--debug",
    "--no-ipc",
    "-f",
    "--transcribe-file",
    "--list-devices",
    "--list-models",
    "--device-index",
    "--repeat",
];

/// Whether this command line belongs to the app binary rather than the CLI.
pub fn is_app_invocation<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter().any(|arg| {
        let arg = arg.as_ref().to_string_lossy().into_owned();
        // Match `--flag` and `--flag=value` alike.
        let name = arg.split('=').next().unwrap_or(&arg).to_string();
        APP_FLAGS.contains(&name.as_str())
    })
}

#[derive(Parser, Debug, Clone)]
#[command(
    name = "handy",
    about = "Talk to a running Handy instance",
    after_help = "Run `handy FILE.wav` to transcribe a file.\n\
                  Flags belonging to the app itself (--start-hidden, --toggle-transcription,\n\
                  -f/--transcribe-file, --list-models, ...) are passed straight through to it."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
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
    /// conversion, no LLM.
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

    /// Use this model instead of the loaded one. Temporary and not persisted.
    #[arg(long)]
    pub model: Option<String>,

    /// Leave the --model override loaded instead of restoring the previous one.
    #[arg(long)]
    pub no_restore_model: bool,

    /// Print every protocol frame as JSON, one per line, terminal frame last.
    #[arg(long, conflicts_with = "json")]
    pub ndjson: bool,

    /// Transcribe in a separate process with its own copy of the model,
    /// instead of using the running instance. Runs the app binary's `-f` path.
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
    /// percentage, but it can differ slightly from the batch transcript.
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
    /// Fall back to transcribing in a separate process.
    Local,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_flags_are_recognised() {
        for flag in [
            "--toggle-transcription",
            "--start-hidden",
            "--no-tray",
            "-f",
            "--list-models",
            "--debug",
        ] {
            assert!(is_app_invocation([flag]), "{flag} should route to the app");
        }
        assert!(is_app_invocation(["--transcribe-file=a.wav"]));
        assert!(is_app_invocation(["--start-hidden", "--no-tray"]));
    }

    #[test]
    fn client_invocations_are_not_forwarded() {
        for argv in [
            vec!["status"],
            vec!["ping"],
            vec!["recording.wav"],
            vec!["recording.wav", "--json"],
            vec!["transcribe", "a.wav", "--llm-post-process"],
        ] {
            assert!(
                !is_app_invocation(&argv),
                "{argv:?} should be handled by the CLI"
            );
        }
    }

    /// Both binaries accept these, so they must not decide the routing.
    #[test]
    fn shared_flags_do_not_force_the_app() {
        assert!(!is_app_invocation(["--json"]));
        assert!(!is_app_invocation(["--model", "whisper"]));
    }
}
