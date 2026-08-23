//! Client-mode dispatch: everything `handy` does *without* starting the app.
//!
//! Runs before `tauri::Builder` is touched. That ordering is load-bearing:
//! once the single-instance plugin's setup hook runs, it forwards argv to the
//! running app and exits the process, so anything printed after that never
//! happens.
//!
//! stdout carries the result and nothing else. Diagnostics go to stderr via
//! plain `eprintln!` — this process never initializes `tauri-plugin-log`, so
//! nothing can leak into a pipe.

use super::{CliArgs, Command, CommonArgs, IfBusy, TranscribeArgs};
use crate::audio_toolkit::decode_to_16k_mono;
use crate::ipc::client::{ConnectError, IdleWatchdog, Session};
use crate::ipc::protocol::*;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

// Exit codes. 0/1/2 keep the meanings the headless path already had.
pub const EXIT_OK: i32 = 0;
pub const EXIT_RUNTIME: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NOT_RUNNING: i32 = 3;
pub const EXIT_BUSY: i32 = 4;
pub const EXIT_PROTOCOL: i32 = 5;
pub const EXIT_CANCELLED: i32 = 6;
pub const EXIT_MODEL: i32 = 7;
pub const EXIT_EMPTY: i32 = 8;

/// Environment escape hatch for people who want the local fallback globally.
const FALLBACK_ENV: &str = "HANDY_CLI_FALLBACK";

/// Verbs a bare first token could plausibly be a typo of.
const KNOWN_VERBS: &[&str] = &["transcribe", "status", "ping"];

/// Outcome of client dispatch.
pub enum Dispatch {
    /// The work is done; exit with this code.
    Exit(i32),
    /// Not a client invocation — carry on and start the app. Carries the args
    /// to start it with, which `--local` rewrites.
    RunApp(Box<CliArgs>),
}

/// Handle a client invocation, if this is one.
pub fn dispatch(args: CliArgs) -> Dispatch {
    let Some(command) = args.command.clone() else {
        return Dispatch::RunApp(Box::new(args));
    };

    let command = match resolve_bare(command) {
        Ok(command) => command,
        Err(code) => return Dispatch::Exit(code),
    };

    match command {
        Command::Ping(common) => Dispatch::Exit(run_ping(&common)),
        Command::Status(common) => Dispatch::Exit(run_status(&common)),
        Command::Transcribe(t) => run_transcribe(*Box::new(t), args),
        // resolve_bare has already turned this into a Transcribe or exited.
        Command::Bare(_) => unreachable!("bare tokens are resolved before dispatch"),
    }
}

/// Re-parse a bare `handy FILE.wav [flags]` invocation as `transcribe`.
fn resolve_bare(command: Command) -> Result<Command, i32> {
    let Command::Bare(raw) = command else {
        return Ok(command);
    };

    let first = raw.first().cloned().unwrap_or_default();
    let first_str = first.to_string_lossy().to_string();

    // An external subcommand swallows a mistyped verb as if it were a path.
    // Tell the difference so `handy stauts` says something useful.
    if looks_like_a_verb(&first_str) && !Path::new(&first).exists() {
        eprintln!("handy: unknown subcommand '{first_str}'");
        if let Some(suggestion) = closest_verb(&first_str) {
            eprintln!("  did you mean `handy {suggestion}`?");
        }
        eprintln!("  run `handy --help` for the full list");
        return Err(EXIT_USAGE);
    }

    let argv = std::iter::once(OsString::from("handy")).chain(raw);
    match parse_transcribe_args(argv) {
        Ok(parsed) => Ok(Command::Transcribe(parsed)),
        Err(e) => {
            let _ = e.print();
            Err(EXIT_USAGE)
        }
    }
}

/// Parse captured tokens as a standalone `transcribe` invocation.
///
/// `TranscribeArgs` derives `Args`, not `Parser`, because it is also a
/// subcommand payload — so build a one-off command from it rather than calling
/// `try_parse_from`.
fn parse_transcribe_args(
    argv: impl IntoIterator<Item = OsString>,
) -> Result<TranscribeArgs, clap::Error> {
    use clap::{Args as _, FromArgMatches as _};
    let command = TranscribeArgs::augment_args(clap::Command::new("handy"));
    let matches = command.try_get_matches_from(argv)?;
    TranscribeArgs::from_arg_matches(&matches)
}

fn looks_like_a_verb(token: &str) -> bool {
    !token.is_empty()
        && !token.starts_with('-')
        && !token.contains('/')
        && !token.contains('\\')
        && !token.contains('.')
}

fn closest_verb(token: &str) -> Option<&'static str> {
    KNOWN_VERBS
        .iter()
        .map(|verb| (*verb, strsim::levenshtein(verb, token)))
        // Two edits is close enough to be a typo, further is a different word.
        .filter(|(_, distance)| *distance <= 2)
        .min_by_key(|(_, distance)| *distance)
        .map(|(verb, _)| verb)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn run_ping(common: &CommonArgs) -> i32 {
    let mut session = match connect(common) {
        Ok(session) => session,
        Err(code) => return code,
    };
    if let Err(e) = session.send(&ClientFrame::Ping) {
        eprintln!("handy: {e:#}");
        return EXIT_RUNTIME;
    }
    finish_simple(&mut session, common)
}

fn run_status(common: &CommonArgs) -> i32 {
    let mut session = match connect(common) {
        Ok(session) => session,
        Err(code) => return code,
    };
    if let Err(e) = session.send(&ClientFrame::Status) {
        eprintln!("handy: {e:#}");
        return EXIT_RUNTIME;
    }
    finish_simple(&mut session, common)
}

fn finish_simple(session: &mut Session, common: &CommonArgs) -> i32 {
    let watchdog = idle_watchdog(common);
    let frame = match session.wait_for_result(|_| watchdog.tick()) {
        Ok(frame) => frame,
        Err(e) => {
            eprintln!("handy: {e:#}");
            return EXIT_RUNTIME;
        }
    };
    match frame {
        ServerFrame::Result { body, .. } => {
            print_simple_result(&body, common.json);
            EXIT_OK
        }
        ServerFrame::Error { code, message, .. } => {
            eprintln!("handy: {message}");
            exit_code_for(code)
        }
        _ => EXIT_RUNTIME,
    }
}

fn print_simple_result(body: &ResultBody, json: bool) {
    match body {
        ResultBody::Pong { app_version } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "schema": 1, "ok": true, "app_version": app_version })
                );
            } else {
                println!("handy {app_version} is running");
            }
        }
        ResultBody::Status(status) => {
            if json {
                let mut value = serde_json::to_value(status).unwrap_or_default();
                if let Some(map) = value.as_object_mut() {
                    map.insert("schema".into(), serde_json::json!(1));
                }
                println!("{}", value);
            } else {
                println!("version:    {}", status.app_version);
                println!("pid:        {}", status.pid);
                println!("uptime:     {}s", status.uptime_secs);
                println!("selected:   {}", status.selected_model);
                println!(
                    "loaded:     {}",
                    status.loaded_model.as_deref().unwrap_or("(none)")
                );
                println!(
                    "backend:    {}",
                    status.backend.as_deref().unwrap_or("(unknown)")
                );
                println!(
                    "engine:     {}",
                    match status.engine_busy {
                        Some(owner) => format!("busy ({owner})"),
                        None => "idle".to_string(),
                    }
                );
                println!("recording:  {}", status.recording);
                println!("streaming:  {}", status.streaming);
                println!("post-proc:  {}", status.post_process_enabled);
            }
        }
        ResultBody::Transcript(_) => {}
    }
}

fn run_transcribe(args: TranscribeArgs, original: CliArgs) -> Dispatch {
    // --local never touches the socket: rewrite into the existing headless
    // invocation and let the normal app entry point take over. No new path.
    if args.local {
        return Dispatch::RunApp(Box::new(local_args(&args, original)));
    }

    let path = match resolve_path(&args.file) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("handy: {message}");
            return Dispatch::Exit(EXIT_USAGE);
        }
    };

    // Decode here, in the process that already has the user's file access.
    // Handing the path to the GUI app instead would hit macOS TCC prompts for
    // Desktop/Documents/Downloads and resolve relative paths against the wrong
    // working directory.
    let progress = show_progress(&args);
    if progress {
        eprintln!("handy: reading {}", path.display());
    }
    let decoded = match decode_to_16k_mono(&path) {
        Ok(decoded) => decoded,
        Err(e) => {
            eprintln!("handy: {e:#}");
            return Dispatch::Exit(EXIT_USAGE);
        }
    };
    if progress && !decoded.is_native() {
        eprintln!(
            "handy: converted {} to 16000 Hz / 1 ch",
            decoded.source_description()
        );
    }

    let mut session = match connect(&args.common) {
        Ok(session) => session,
        Err(code) => {
            // Falling back means loading gigabytes of weights this process
            // does not have, so it is opt-in rather than automatic.
            if code == EXIT_NOT_RUNNING && wants_local_fallback(&args) {
                eprintln!("handy: no running instance; transcribing locally");
                return Dispatch::RunApp(Box::new(local_args(&args, original)));
            }
            return Dispatch::Exit(code);
        }
    };

    let encoding = AudioEncoding::PcmF32Le;
    let payload = encoding.encode(&decoded.samples);
    let request = ClientFrame::Transcribe(Box::new(TranscribeRequest {
        audio: AudioSource::Inline {
            encoding,
            sample_rate: 16_000,
            channels: 1,
            byte_len: payload.len() as u64,
        },
        source_label: path.file_name().map(|n| n.to_string_lossy().into_owned()),
        model: args.model.clone(),
        restore_model: !args.no_restore_model,
        post_process: args.post_process,
        save_history: args.history && !args.no_history,
        paste: args.paste,
        wait_policy: match args.if_busy {
            IfBusy::Fail => WaitPolicy::FailFast,
            IfBusy::Wait | IfBusy::Local => WaitPolicy::Wait { timeout_ms: None },
        },
        want_progress: true,
    }));

    if let Err(e) = session.send_with_audio(&request, &payload) {
        eprintln!("handy: {e:#}");
        return Dispatch::Exit(EXIT_RUNTIME);
    }

    Dispatch::Exit(stream_transcription(&mut session, &args, progress))
}

fn stream_transcription(session: &mut Session, args: &TranscribeArgs, progress: bool) -> i32 {
    let watchdog = idle_watchdog(&args.common);
    let interrupts = crate::ipc::client::spawn_interrupt_watcher();

    let frame = {
        // Ctrl-C cancels the job rather than orphaning it. Even without this,
        // the server notices the disconnect; this just makes it graceful.
        let cancel_check = |session: &mut Session| {
            if interrupts.try_recv().is_ok() {
                if progress {
                    eprintln!("handy: cancelling…");
                }
                session.cancel();
            }
        };
        let mut result = None;
        loop {
            match session.read_frame() {
                Ok(Some(frame)) => {
                    watchdog.tick();
                    if args.ndjson {
                        print_ndjson(&frame);
                    } else if progress {
                        print_progress(&frame);
                    }
                    match frame {
                        ServerFrame::Result { .. } | ServerFrame::Error { .. } => {
                            result = Some(frame);
                            break;
                        }
                        _ => {}
                    }
                    cancel_check(session);
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("handy: {e:#}");
                    return EXIT_RUNTIME;
                }
            }
        }
        result
    };

    let Some(frame) = frame else {
        eprintln!("handy: the connection closed before a result arrived");
        return EXIT_RUNTIME;
    };

    match frame {
        ServerFrame::Error { code, message, .. } => {
            if !args.ndjson {
                eprintln!("handy: {message}");
            }
            if code == ErrorCode::Busy && args.if_busy == IfBusy::Local {
                eprintln!("handy: run again with --local to transcribe in this process");
            }
            exit_code_for(code)
        }
        ServerFrame::Result { body, .. } => match body {
            ResultBody::Transcript(t) => print_transcript(&t, args),
            other => {
                print_simple_result(&other, args.common.json);
                EXIT_OK
            }
        },
        _ => EXIT_RUNTIME,
    }
}

fn print_transcript(t: &TranscriptBody, args: &TranscribeArgs) -> i32 {
    if args.ndjson {
        // Already printed frame by frame.
    } else if args.common.json {
        let mut value = serde_json::to_value(t).unwrap_or_default();
        if let Some(map) = value.as_object_mut() {
            map.insert("schema".into(), serde_json::json!(1));
        }
        if !write_stdout(&format!("{value}\n")) {
            return EXIT_OK;
        }
    } else {
        // Bare text, no prefix — this is what gets piped.
        if !write_stdout(&format!("{}\n", t.text)) {
            return EXIT_OK;
        }
    }

    if t.text.is_empty() {
        if !args.common.quiet {
            eprintln!("handy: no speech detected");
        }
        if args.fail_on_empty {
            return EXIT_EMPTY;
        }
    }
    if t.post_processed && !t.post_process_applied && !args.common.quiet {
        eprintln!("handy: post-processing was requested but did not apply (provider failed?)");
    }
    EXIT_OK
}

/// Write to stdout, treating a closed pipe as success.
///
/// Rust ignores SIGPIPE, so `handy x.wav | head -1` would otherwise panic with
/// "Broken pipe" instead of exiting quietly.
fn write_stdout(text: &str) -> bool {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match lock.write_all(text.as_bytes()).and_then(|()| lock.flush()) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => false,
        Err(e) => {
            eprintln!("handy: writing to stdout: {e}");
            false
        }
    }
}

fn print_ndjson(frame: &ServerFrame) {
    if let Ok(text) = serde_json::to_string(frame) {
        let _ = write_stdout(&format!("{text}\n"));
    }
}

fn print_progress(frame: &ServerFrame) {
    if let ServerFrame::Progress {
        stage,
        message,
        elapsed_ms,
        ..
    } = frame
    {
        let elapsed = Duration::from_millis(*elapsed_ms);
        match message {
            Some(message) => eprintln!(
                "handy: {} ({}) — {message}",
                stage.label(),
                format_duration(elapsed)
            ),
            None => eprintln!("handy: {} ({})", stage.label(), format_duration(elapsed)),
        }
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn connect(common: &CommonArgs) -> Result<Session, i32> {
    match Session::connect() {
        Ok(session) => Ok(session),
        Err(e) => {
            eprintln!("handy: {e}");
            Err(match e {
                ConnectError::NotRunning { .. } => EXIT_NOT_RUNNING,
                ConnectError::ProtocolMismatch { .. } => EXIT_PROTOCOL,
                ConnectError::Forbidden(_) => EXIT_USAGE,
                ConnectError::Other(_) => {
                    let _ = common;
                    EXIT_RUNTIME
                }
            })
        }
    }
}

fn idle_watchdog(common: &CommonArgs) -> IdleWatchdog {
    let timeout = Duration::from_secs(common.idle_timeout.max(1));
    let quiet = common.quiet;
    IdleWatchdog::spawn(timeout, move || {
        if !quiet {
            eprintln!("handy: no response for {}s; giving up", timeout.as_secs());
        }
        // The read is blocked in the kernel, so unwinding is not an option;
        // exiting also tells the server (via disconnect) to stop the job.
        std::process::exit(EXIT_RUNTIME);
    })
}

fn show_progress(args: &TranscribeArgs) -> bool {
    if args.no_progress || args.common.quiet || args.ndjson {
        return false;
    }
    if args.progress {
        return true;
    }
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

fn wants_local_fallback(args: &TranscribeArgs) -> bool {
    args.if_busy == IfBusy::Local
        || std::env::var(FALLBACK_ENV).is_ok_and(|v| v.eq_ignore_ascii_case("local"))
}

/// Rewrite a client transcribe into the existing headless invocation.
fn local_args(args: &TranscribeArgs, original: CliArgs) -> CliArgs {
    CliArgs {
        command: None,
        transcribe_file: Some(args.file.clone()),
        model: args.model.clone(),
        json: args.common.json,
        debug: original.debug,
        ..CliArgs::default()
    }
}

/// Canonicalize, expanding a leading `~/` that a quoted argument kept from the
/// shell. The server never sees a path, so this only has to be right here.
fn resolve_path(path: &Path) -> Result<PathBuf, String> {
    let expanded = expand_tilde(path);
    std::fs::canonicalize(&expanded)
        .map_err(|e| format!("cannot read {}: {}", expanded.display(), e))
}

fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let Some(rest) = text.strip_prefix("~/") else {
        return path.to_path_buf();
    };
    match home_dir() {
        Some(home) => home.join(rest),
        None => path.to_path_buf(),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn exit_code_for(code: ErrorCode) -> i32 {
    match code {
        ErrorCode::Busy => EXIT_BUSY,
        ErrorCode::Cancelled => EXIT_CANCELLED,
        ErrorCode::ProtocolMismatch | ErrorCode::UnsupportedRequest => EXIT_PROTOCOL,
        ErrorCode::BadRequest | ErrorCode::PayloadTooLarge | ErrorCode::Forbidden => EXIT_USAGE,
        ErrorCode::NoModelSelected | ErrorCode::ModelNotAvailable | ErrorCode::ModelLoadFailed => {
            EXIT_MODEL
        }
        ErrorCode::TranscriptionFailed
        | ErrorCode::IoError
        | ErrorCode::Internal
        | ErrorCode::TooManyClients => EXIT_RUNTIME,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(argv: &[&str]) -> CliArgs {
        CliArgs::try_parse_from(argv).unwrap_or_else(|e| panic!("failed to parse {argv:?}: {e}"))
    }

    // ---- the regression net: every documented flag must keep working -------

    #[test]
    fn legacy_flags_still_parse_without_a_subcommand() {
        let args = parse(&["handy", "--toggle-transcription"]);
        assert!(args.toggle_transcription);
        assert!(args.command.is_none());

        let args = parse(&["handy", "--toggle-post-process"]);
        assert!(args.toggle_post_process);
        assert!(args.command.is_none());

        let args = parse(&["handy", "--cancel"]);
        assert!(args.cancel);

        let args = parse(&["handy", "--start-hidden", "--no-tray"]);
        assert!(args.start_hidden);
        assert!(args.no_tray);
        assert!(args.command.is_none());

        let args = parse(&["handy", "-f", "x.wav", "--model", "m", "--json"]);
        assert_eq!(args.transcribe_file, Some(PathBuf::from("x.wav")));
        assert_eq!(args.model.as_deref(), Some("m"));
        assert!(args.json);
        assert!(args.command.is_none());

        let args = parse(&["handy", "--list-models", "--json"]);
        assert!(args.list_models);

        let args = parse(&["handy", "--list-devices"]);
        assert!(args.list_devices);

        let args = parse(&["handy", "--debug"]);
        assert!(args.debug);
    }

    #[test]
    fn no_arguments_starts_the_app() {
        let args = parse(&["handy"]);
        assert!(args.command.is_none());
        assert!(matches!(dispatch(args), Dispatch::RunApp(_)));
    }

    // ---- the new surface ---------------------------------------------------

    #[test]
    fn a_bare_path_is_captured_and_reparsed_as_transcribe() {
        let args = parse(&["handy", "recording.wav"]);
        let command = resolve_bare(args.command.unwrap()).expect("should re-parse");
        match command {
            Command::Transcribe(t) => assert_eq!(t.file, PathBuf::from("recording.wav")),
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn a_bare_path_keeps_its_trailing_flags() {
        let args = parse(&["handy", "rec.wav", "--json", "--post-process", "--paste"]);
        let command = resolve_bare(args.command.unwrap()).expect("should re-parse");
        match command {
            Command::Transcribe(t) => {
                assert_eq!(t.file, PathBuf::from("rec.wav"));
                assert!(t.common.json);
                assert!(t.post_process);
                assert!(t.paste);
                assert!(!t.history, "history must be opt-in");
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn absolute_and_relative_paths_both_work() {
        for path in ["/tmp/a.wav", "./a.wav", "../a.wav", "sub/dir/a.wav"] {
            let args = parse(&["handy", path]);
            let command = resolve_bare(args.command.unwrap()).expect("should re-parse");
            assert!(
                matches!(command, Command::Transcribe(_)),
                "failed for {path}"
            );
        }
    }

    #[test]
    fn explicit_subcommands_parse() {
        assert!(matches!(
            parse(&["handy", "status"]).command,
            Some(Command::Status(_))
        ));
        assert!(matches!(
            parse(&["handy", "ping", "--json"]).command,
            Some(Command::Ping(_))
        ));
        match parse(&["handy", "transcribe", "a.wav", "--local"]).command {
            Some(Command::Transcribe(t)) => assert!(t.local),
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn a_mistyped_verb_is_reported_as_a_verb_not_a_missing_file() {
        let args = parse(&["handy", "stauts"]);
        assert_eq!(resolve_bare(args.command.unwrap()).unwrap_err(), EXIT_USAGE);
        assert_eq!(closest_verb("stauts"), Some("status"));
        assert_eq!(closest_verb("pign"), Some("ping"));
        // A real word that is not a near-miss gets no suggestion.
        assert_eq!(closest_verb("frobnicate"), None);
    }

    #[test]
    fn a_path_shaped_token_is_not_mistaken_for_a_verb() {
        assert!(!looks_like_a_verb("recording.wav"));
        assert!(!looks_like_a_verb("/tmp/a"));
        assert!(!looks_like_a_verb("./a"));
        assert!(!looks_like_a_verb("--json"));
        assert!(looks_like_a_verb("stauts"));
    }

    #[test]
    fn history_flags_override_each_other_in_order() {
        let args = parse(&["handy", "transcribe", "a.wav", "--history", "--no-history"]);
        match args.command {
            Some(Command::Transcribe(t)) => {
                assert!(t.no_history);
                assert!(
                    !(t.history && !t.no_history),
                    "should resolve to no history"
                );
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn local_rewrites_into_the_headless_invocation() {
        let args = parse(&["handy", "transcribe", "a.wav", "--local", "--model", "m"]);
        let Some(Command::Transcribe(t)) = args.command.clone() else {
            panic!("expected transcribe");
        };
        let rewritten = local_args(&t, args);
        assert_eq!(rewritten.transcribe_file, Some(PathBuf::from("a.wav")));
        assert_eq!(rewritten.model.as_deref(), Some("m"));
        assert!(rewritten.command.is_none(), "must not re-enter client mode");
    }

    #[test]
    fn if_busy_accepts_its_modes() {
        for (flag, want) in [
            ("wait", IfBusy::Wait),
            ("fail", IfBusy::Fail),
            ("local", IfBusy::Local),
        ] {
            let args = parse(&["handy", "transcribe", "a.wav", "--if-busy", flag]);
            match args.command {
                Some(Command::Transcribe(t)) => assert_eq!(t.if_busy, want),
                other => panic!("wrong command: {other:?}"),
            }
        }
    }

    #[test]
    fn tilde_is_expanded_because_a_quoted_path_never_reaches_the_shell() {
        std::env::set_var("HOME", "/home/example");
        assert_eq!(
            expand_tilde(Path::new("~/audio/a.wav")),
            PathBuf::from("/home/example/audio/a.wav")
        );
        // A bare "~" or an embedded tilde is left alone.
        assert_eq!(expand_tilde(Path::new("a~b.wav")), PathBuf::from("a~b.wav"));
    }

    #[test]
    fn error_codes_map_to_distinct_exit_statuses() {
        assert_eq!(exit_code_for(ErrorCode::Busy), EXIT_BUSY);
        assert_eq!(exit_code_for(ErrorCode::Cancelled), EXIT_CANCELLED);
        assert_eq!(exit_code_for(ErrorCode::ModelNotAvailable), EXIT_MODEL);
        assert_eq!(exit_code_for(ErrorCode::UnsupportedRequest), EXIT_PROTOCOL);
        assert_eq!(exit_code_for(ErrorCode::BadRequest), EXIT_USAGE);
        assert_eq!(exit_code_for(ErrorCode::Internal), EXIT_RUNTIME);
    }
}
