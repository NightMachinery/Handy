# The connected CLI

`handy FILE.wav` transcribes a file using the **already-running** Handy — the
one that has your model resident in memory — and prints the transcript to
stdout.

This is a fork addition. It is separate from `handy -f FILE.wav`, which has
always existed and does something different: that one starts a whole second
process and loads its own private copy of the model, so it pays a cold multi
gigabyte load before any work begins.

```
handy recording.wav          connected — uses the running app's loaded model
handy -f recording.wav       local     — loads a private copy in this process
```

## Two binaries

`handy` is a separate, small binary from the app — 1.5 MB against the app's
38 MB. The split is not cosmetic: the app links transcribe-cpp and ONNX Runtime
through build scripts, so a CLI sharing its crate would pull the whole
inference stack in whether it called it or not.

It buys three things:

- **A bare `handy` prints help.** The app binary cannot do that — macOS
  launches a bundle with an empty argv, so for it "no arguments" has to mean
  "start the GUI".
- **No console hack on Windows.** The app is a GUI-subsystem binary with no
  console attached in release builds; the CLI is console-subsystem and simply
  prints.
- **Startup stays cheap** — no chance of dragging in Tauri by accident.

Flags belonging to the app — `--toggle-transcription`, `-f/--transcribe-file`,
`--start-hidden`, `--list-models`, `--debug` — are recognised in argv and the
command line is handed to the app binary verbatim. There is one definition of
each of those flags, in the app, and the CLI deliberately does not model them.
So every documented invocation still works through the single `handy` command.

Both binaries ship inside the bundle, side by side:

```
/Applications/Handy.app/Contents/MacOS/handy       38M   the app
/Applications/Handy.app/Contents/MacOS/handy-cli   1.5M  the CLI
~/bin/handy -> .../Contents/MacOS/handy-cli              what you call
```

Keeping the CLI in the bundle means the PATH entry is a symlink into a stable
location rather than a copy that goes stale after the next rebuild, and moving
or copying the `.app` takes the CLI with it. It does mean the bundle is
re-signed after the CLI is added, since tauri signs before that point —
`scripts/install-macos.sh` does this and verifies the result.

`bun run install:macos` installs both; `--link-only` refreshes just the
symlink. The CLI finds the app via `HANDY_APP_BIN`, then its sibling in the
bundle, then `/Applications/Handy.app`.

## Why a socket, and not the existing plugin

Handy already forwards CLI arguments to a running instance via
`tauri-plugin-single-instance`, which is how `--toggle-transcription` works.
That channel cannot be used here, for a structural reason rather than a
missing feature: the plugin's callback signature returns nothing, and the
second process calls `exit(0)` from inside the plugin's own setup hook, before
any Handy code in that process runs. It is one-way by construction. It can ask
the app to start recording; it can never receive a transcript back.

So the connected CLI adds a real request/response channel alongside it. The
single-instance plugin is untouched and keeps working exactly as before.

## Commands

```
handy FILE.wav                 transcribe a file
handy transcribe FILE.wav      the same thing, spelled explicitly
handy status                   what the running instance is doing
handy ping                     check that it answers
```

Everything else the CLI accepts is unchanged. `--toggle-transcription`,
`--toggle-post-process`, `--cancel`, `--start-hidden`, `--no-tray`, `--debug`,
`-f/--transcribe-file`, `--list-models`, `--list-devices` all parse exactly as
they always have, so existing window-manager bindings and autostart entries
keep working byte for byte.

### Flags for `transcribe`

- `--json` — one JSON object on stdout with timings, model, and both the raw
  and post-processed text. Nothing else is printed to stdout.
- `--ndjson` — every protocol frame as it arrives, one JSON object per line,
  terminal frame last. Useful when a script wants progress.
- `--llm-post-process` — additionally run LLM post-processing, using your
  configured provider. Makes a network call. See "Output pipeline" below.
- `--raw` — print what the engine produced, with no pipeline at all.
- `--paste` — inject the transcript into the focused window. Off by default,
  and worth thinking about before using: the focused window is usually the
  terminal you just typed the command into.
- `--history` — record the result in Handy's transcription history, which also
  writes a WAV into the recordings directory. Off by default; a file you
  transcribed is not a dictation and should not push real dictations out of the
  retention window.
- `--model ID` — load a specific model for this job. Temporary and not
  persisted: the previously loaded model is restored afterwards unless you pass
  `--no-restore-model`. Note that this does swap what the GUI has loaded for
  the duration of the job.
- `--local` — do not connect; transcribe in this process instead. Exactly
  equivalent to `handy -f FILE`.
- `--if-busy wait|fail|local` — what to do when the engine is in use. Defaults
  to `wait`.
- `--fail-on-empty` — exit non-zero when the transcript is empty. By default an
  empty transcript is a successful run that printed nothing.
- `--idle-timeout SECS` — give up if the app sends nothing for this long
  (default 120). This bounds _silence_, not total runtime; a twenty-minute
  transcription is expected and fine.
- `--progress` / `--no-progress` — force progress on stderr on or off. The
  default is on when stderr is a terminal.
- `--no-stream` — use the batch engine even when the model can stream. See
  below for why you might.
- `--partials` — print text on stderr as it is decoded (streaming models only).
- `-q/--quiet` — suppress progress and notes on stderr.

## Progress

On a terminal, progress is drawn in place on one stderr line. What it can show
depends on which engine path runs.

For a **streaming-capable** model the engine reports `audio_committed_ms` on
every feed, and the client already knows the file's exact duration, so the
percentage is measured rather than guessed:

```
handy: [##########..............]  42%  3m18s (~4m32s left)
```

For a **batch** model there is no bar, because there is nothing honest to put
in one. `transcribe-cpp` exposes no progress callback — inference is a single
opaque blocking call — so the client shows a spinner and elapsed time instead:

```
handy: / transcribing (3m18s)
```

Extrapolating a percentage from real-time-factor would be possible, but it
would be a fabricated number that sits near the end for a long time. A bar that
lies is worse than no bar.

When stderr is not a terminal, progress appends plain lines instead of
redrawing, which is what a log or CI transcript wants. `--ndjson` carries every
frame, including `fraction` and `partial`, for scripts that want to render
their own.

### Streaming vs batch

Streaming is used by default whenever the loaded model supports it, because it
is the only path that yields a real percentage, and because its feed loop
belongs to the CLI — so cancellation lands within half a second rather than
after the whole inference.

The trade-off is that streaming families commit text incrementally, so the
transcript can differ slightly from what the batch path — and therefore
`handy -f` — produces for the same audio. Pass `--no-stream` when you want the
batch result specifically. `--json` reports which path ran as `streamed`.

Models without streaming support use batch regardless; there is nothing to
opt into.

## Output pipeline

A dictation does not paste what the engine emitted. It runs the transcript
through Chinese variant conversion and, if you have one configured, your
command filter — and only then, optionally, an LLM. The CLI does the same, so
`handy file.wav` gives you what dictating that audio would have given you.

Three modes:

```
handy file.wav                       conversion + command filter (default)
handy file.wav --llm-post-process    the above, plus your LLM provider
handy file.wav --raw                 exactly what the engine produced
```

The split matters because the command filter and the LLM are independent
things. The filter is a local program of yours with a scope setting that
already distinguishes plain transcription from post-processed; the LLM is a
network call that costs money and latency. Folding them into one flag would
have meant you could not run your own filter without also paying for an LLM
round trip — which is why `--llm-post-process` is separate rather than the
single `--post-process` this originally shipped with. That older spelling still
works as an alias.

`--json` reports all three outcomes separately: `pipeline_ran`,
`post_processed` (LLM requested), and `post_process_applied` (LLM actually
produced text — the pipeline falls back to the pre-LLM text when a provider
fails). `raw_text` always carries the engine's untouched output alongside
whatever `text` ended up being.

Note that a command filter can legitimately return empty to suppress output,
and that it runs with your configured timeout. `--raw` is the way to bypass
both concerns when a script wants the model's output and nothing else.

## Output conventions

stdout carries the result and nothing else. Progress, notes, and errors all go
to stderr, so `handy rec.wav > out.txt` gives a clean transcript and
`handy rec.wav | pbcopy` does what you want.

The client process never builds a Tauri app and never initializes the logger,
so there is no way for a log line to end up in your pipe.

A closed pipe is not an error: `handy rec.wav | head -1` exits 0 quietly rather
than panicking, which is what Rust's default SIGPIPE handling would otherwise
do.

### Exit codes

```
0   success (including an empty transcript, unless --fail-on-empty)
1   runtime failure
2   usage error or unreadable input file
3   no running instance
4   engine busy (with --if-busy fail, or a wait timeout)
5   protocol mismatch between this binary and the running app
6   cancelled
7   model unavailable, not downloaded, or none selected
8   empty transcript with --fail-on-empty
```

Codes 0, 1, and 2 keep the meanings the headless `-f` path already used.

## When Handy is not running

`handy FILE.wav` exits 3 and tells you so. It does **not** silently fall back
to loading a model locally.

That is deliberate. The entire point of the connected CLI is to use a model
that is already in memory; a silent fallback would, on a script's first bad
day, spend thirty seconds and several gigabytes doing the opposite of what was
asked, in a way that is hard to notice inside a pipeline. Opt in explicitly
instead — `--local`, `--if-busy local`, or `HANDY_CLI_FALLBACK=local`.

## Contention with dictation

Only one thing can use the transcription engine at a time. A CLI job takes a
lease on it, and so does every dictation.

If you press your record hotkey while a CLI job is running, the recording
starts normally — audio capture is independent of the engine — but live preview
is skipped and the transcription queues behind the CLI job. Nothing is lost;
it just waits. Dictations take priority over _queued_ CLI jobs, so a dictation
never ends up behind a CLI job that arrived later. A job already running is
never interrupted — see the cancellation note below for why, and what it would
take to change that.

For a long batch job on a machine you are also dictating on, `--local` avoids
the contention entirely by using a separate process and its own model copy.

## Cancellation

Ctrl-C cancels the job and exits. Killing the client works too — the server
notices the disconnect and stops.

On the **streaming** path, cancellation is prompt: the feed loop belongs to the
CLI, so it stops at the next half-second chunk.

On the **batch** path, an inference already in flight is not interrupted.
Cancellation is honoured while queued, while waiting for the engine, between
stages, and after inference completes (the result is discarded), but the engine
call itself runs to completion first.

That last part is a gap in Handy, not in the engine. `transcribe-cpp` already
exposes `Session::set_cancel_token`, and Handy installs one nowhere; wiring it
in would make batch cancellation immediate for GGUF models whose family
advertises `Feature::Cancellation`. The ONNX engines behind `transcribe-rs`
expose no cancellation at all, so for those the current behaviour is the
ceiling.

## Audio formats

Any WAV is accepted: 8, 16, 24, or 32-bit integer, 32-bit float, any channel
count, any sample rate from 4 kHz to 384 kHz. Multi-channel audio is downmixed
by averaging the channels (not by taking the first one — a stereo interview
often has one speaker per channel), and anything not already at 16 kHz is
resampled.

Non-WAV formats are not supported yet. The plan is to shell out to `ffmpeg`
when it is on `PATH` rather than take on a decoder dependency and its codec
matrix; until then, convert first.

The **client** does the decoding, not the app. This matters on macOS: Handy is
a GUI application subject to TCC, so handing it a path under Desktop,
Documents, Downloads, iCloud Drive, or an external volume would either raise a
consent prompt attributed to Handy — possibly unsurfaceable in a tray-only
session — or simply fail. Your terminal already has that access. Decoding
client-side also means relative paths, `~`, and symlinks resolve against your
shell's working directory rather than the app's, which for a launched bundle is
`/`.

It has a security benefit too: because the server never opens a path the caller
supplied, the socket is not a file-read oracle.

## Where the socket lives

A Unix domain socket on macOS and Linux, a named pipe on Windows:

```
$XDG_RUNTIME_DIR/handy-<uid>/ipc.sock     Linux
$TMPDIR/handy-<uid>/ipc.sock              macOS
\\.\pipe\handy-ipc-<user>                 Windows
```

falling back to `/tmp/handy-<uid>/` when neither environment variable points at
a usable directory, and also when the computed path would exceed the ~100 byte
`sun_path` limit.

Alongside the socket is `ipc.json`, a descriptor holding the protocol version,
app version, and pid. The client reads it before connecting so it can report
"that Handy is too old, restart it" instead of an opaque connection failure,
and so a socket left behind by a crash can be recognised as stale.

Set `HANDY_IPC_NAME=something` to suffix the directory. Both sides read it
identically, so it is the way to keep a development build and an installed app
from talking to each other.

### Security model

The directory is created `0700` and the socket `0600`. The directory is the
authorization boundary, which is why there is no token. The server also checks
the peer's uid after accepting, and the client refuses to connect through a
directory that is not owned by it or that is group- or world-accessible.

This is deliberately _not_ the path the single-instance plugin uses
(`/tmp/com_pais_handy_si.sock`), which sits directly in a world-writable sticky
directory where another local user could pre-create the path and intercept
connections.

The capability set here — start a recording, inject keystrokes with `--paste`,
read transcripts — is essentially what the existing `--toggle-transcription`
channel already exposes over that world-writable path, so this is a net
improvement rather than new exposure. Pass `--no-ipc` at startup to disable the
listener entirely. (A persisted setting and a UI toggle would be the natural
follow-up; today it is a launch flag only.)

## Protocol

Newline-delimited JSON, one object per line. A frame carrying audio declares
its byte length inside the frame, and those raw bytes follow immediately after
the newline — so bulk audio and control frames share one connection without
base64 overhead, and there is exactly one authority on how many bytes to read.

The server greets on accept, before reading anything, so version skew surfaces
before any work is done.

Compatibility rules, so this can grow without a flag day:

- Adding an enum variant or an optional field is backward compatible.
- Unknown server frames deserialize to an `Unknown` variant and are skipped, so
  an old client survives a newer server.
- Unknown client requests are answered with `unsupported_request` naming the
  request, never by dropping the connection.
- The protocol version bumps only on a breaking change, and the server
  advertises the oldest version it still accepts.

### Adding a request kind

Four edits, and everything else is inherited:

1. A variant on `ClientFrame` in `src-tauri/src/ipc/protocol.rs`.
2. A variant on `ResultBody` in the same file.
3. A module under `src-tauri/src/ipc/handlers/`, exposing
   `handle(...) -> HandlerResult`.
4. An arm in the dispatch match in `src-tauri/src/ipc/server.rs`.

Framing, the handshake, cancellation, heartbeats, the client cap, and error
to exit code mapping all come for free. Cheap requests should be answered
inline on the connection thread; anything that needs the transcription engine
goes through the job queue so concurrent clients serialize in arrival order.

Natural next verbs, in rough order of value: `watch` (stream transcription
events as NDJSON, which turns Handy into a scriptable dictation primitive),
`record start|stop|toggle --wait`, `last -n N`, and `models list|use|download`.

## Module layout

```
src-tauri/crates/handy-core/      shared by the app and the CLI
├── protocol.rs    frames, codec, LeaseOwner; pure serde, unit-tests standalone
├── endpoint.rs    where the socket lives; used verbatim by both sides
├── transport.rs   the only module that names a socket implementation
├── client.rs      the blocking client
└── audio/         WAV decoding and resampling

src-tauri/crates/handy-cli/       the `handy` binary
├── main.rs        routing: client verb, app passthrough, or help
├── args.rs        client-facing clap surface + app-flag detection
└── client.rs      dispatch, output formatting, exit codes

src-tauri/src/ipc/                app-side only
├── server.rs      accept loop, cancellation, heartbeats
└── handlers/      one module per request kind
```

The rule that keeps this honest: **nothing in `handy-core` may depend on tauri,
transcribe-cpp, transcribe-rs or cpal.** That is why the IPC _server_ lives in
the app while only the client is shared, and why `LeaseOwner` was moved into
the protocol — it crosses the wire, and leaving it in the app's `managers`
module dragged the inference stack into anything that spoke the protocol.

## Notes and gotchas

**Windows consoles.** The app is a GUI-subsystem binary
(`windows_subsystem = "windows"`), so it has no console in release builds and
anything it prints goes to an invalid handle. That is why the CLI is a separate
console-subsystem binary rather than a mode of the app — it prints normally,
`cmd.exe` waits for it, and no `AttachConsole` shim is needed. App flags
forwarded through it are `exec`ed into the app, which still has no console;
that only matters for `-f/--transcribe-file`, whose output is best captured
with `--json` redirected to a file.

**`handy FILE.wav` works through a clap trick.** With an optional subcommand,
clap resolves the first token against subcommand names before positionals, so a
bare path would fail as an unrecognised subcommand. An external-subcommand
catch-all captures those tokens verbatim and they are re-parsed as a
`transcribe` — the same approach `cargo` uses. One consequence: a mistyped verb
is captured as if it were a path, so the CLI checks whether a bare word that is
not a file looks like a verb, and suggests the closest match.

**Model unload timeout.** If yours is set to unload immediately, each CLI call
pays a cold model load, which is reported separately from inference time and
flagged as `cold_load` in the JSON output.

**Post-processing can fail quietly.** The pipeline falls back to the pre-LLM
text when a provider errors, so `--json` reports `post_processed` (requested)
and `post_process_applied` (actually happened) separately.
