# AGENTS.md

This file provides guidance to AI coding assistants working with code in this repository.

## Development Commands

**Prerequisites:**

- [Rust](https://rustup.rs/) (latest stable)
- [Bun](https://bun.sh/) package manager

**Core Development:**

```bash
# Install dependencies
bun install

# Run in development mode (on macOS export SDKROOT first, see below)
bun run tauri dev
# If cmake error on macOS:
CMAKE_POLICY_VERSION_MINIMUM=3.5 bun run tauri dev

# Build for production
bun run tauri build

# Frontend only development
bun run dev        # Start Vite dev server
bun run build      # Build frontend (TypeScript + Vite)
bun run preview    # Preview built frontend
```

**macOS: `SDKROOT` must point at SDK 14.4+.** Every `cargo build`, `cargo
sweep`, `bun run tauri dev` and `bun run tauri build` on macOS needs:

```bash
export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX14.4.sdk
```

Since the v0.9.4 upstream merge, `ort` rc12's prebuilt ONNX Runtime references
the CoreML classes `MLComputePlan` and `MLOptimizationHints`, which exist only
in macOS SDK 14.4+. Xcode 15.1 ships SDK 14.2, so when `xcode-select` points at
that Xcode the build fails — at the *link* step, after everything has already
compiled, with a message that names neither the SDK nor `ort`:

```
ld: Undefined symbols: _OBJC_CLASS_$_MLComputePlan
```

Confirm the active SDK with `xcrun --show-sdk-version`; anything below 14.4
will fail this way. Installing Command Line Tools 15.3+ provides the 14.4 SDK
without touching Xcode. Full rationale and the install step:
[docs/build_mac.md](docs/build_mac.md).

**Linting and Formatting (run before committing):**

```bash
bun run lint              # ESLint for frontend
bun run lint:fix          # ESLint with auto-fix
bun run format            # Prettier + cargo fmt
bun run format:check      # Check formatting without changes
bun run format:frontend   # Prettier only
bun run format:backend    # cargo fmt only
```

**Model Setup (Required for Development):**

```bash
mkdir -p src-tauri/resources/models
curl -o src-tauri/resources/models/silero_vad_v4.onnx https://blob.handy.computer/silero_vad_v4.onnx
```

For detailed platform-specific build setup, see [BUILD.md](BUILD.md).

## Architecture Overview

Handy is a cross-platform desktop speech-to-text application built with Tauri 2.x (Rust backend + React/TypeScript frontend).

### Backend Structure (src-tauri/src/)

- `lib.rs` - Main entry point, Tauri setup, manager initialization
- `managers/` - Core business logic:
  - `audio.rs` - Audio recording and device management
  - `model.rs` - Model downloading and management
  - `transcription.rs` - Speech-to-text processing pipeline
  - `history.rs` - Transcription history storage
- `audio_toolkit/` - Low-level audio processing:
  - `audio/` - Device enumeration, recording, resampling
  - `vad/` - Voice Activity Detection (Silero VAD)
- `commands/` - Tauri command handlers for frontend communication
- `cli.rs` - CLI argument definitions (clap derive)
- `shortcut.rs` - Global keyboard shortcut handling
- `settings.rs` - Application settings management
- `overlay.rs` - Recording overlay window (platform-specific)
- `signal_handle.rs` - `send_transcription_input()` reusable function
- `utils.rs` - Platform detection helpers

### Frontend Structure (src/)

- `App.tsx` - Main component with onboarding flow
- `components/` - React UI components:
  - `settings/` - Settings UI
  - `model-selector/` - Model management interface
  - `onboarding/` - First-run experience
  - `overlay/` - Recording overlay UI
  - `update-checker/` - App update notifications
  - `shared/`, `ui/`, `icons/`, `footer/` - Shared components
- `hooks/useSettings.ts` - Settings state management hook
- `stores/settingsStore.ts` - Zustand store for settings
- `bindings.ts` - Auto-generated Tauri type bindings (via tauri-specta)
- `overlay/` - Recording overlay window entry point
- `lib/types.ts` - Shared TypeScript type definitions

### Key Architecture Patterns

**Manager Pattern:** Core functionality organized into managers (Audio, Model, Transcription) initialized at startup and managed via Tauri state.

**Command-Event Architecture:** Frontend → Backend via Tauri commands; Backend → Frontend via events.

**Pipeline Processing:** Audio → VAD → Whisper/Parakeet → Text output → Clipboard/Paste

**State Flow:** Zustand → Tauri Command → Rust State → Persistence (tauri-plugin-store)

### Technology Stack

**Core Libraries:**

- `transcribe-cpp` - Local Whisper-family inference (GGML/GGUF) with GPU acceleration
- `transcribe-rs` - ONNX speech recognition (Parakeet, Moonshine, SenseVoice, etc.)
- `cpal` - Cross-platform audio I/O
- `vad-rs` - Voice Activity Detection
- `rdev` - Global keyboard shortcuts
- `rubato` - Audio resampling
- `rodio` - Audio playback for feedback sounds

### Application Flow

1. **Initialization:** App starts minimized to tray, loads settings, initializes managers
2. **Model Setup:** First-run downloads preferred Whisper model (Small/Medium/Turbo/Large)
3. **Recording:** Global shortcut triggers audio recording with VAD filtering
4. **Processing:** Audio sent to Whisper model for transcription
5. **Output:** Text pasted to active application via system clipboard

### Settings System

Settings are stored using Tauri's store plugin with reactive updates:

- Keyboard shortcuts (configurable, supports push-to-talk)
- Audio devices (microphone/output selection)
- Model preferences (Small/Medium/Turbo/Large Whisper variants)
- Audio feedback and translation options

### Single Instance Architecture

The app enforces single instance behavior — launching when already running brings the settings window to front rather than creating a new process. Remote control flags (`--toggle-transcription`, etc.) work by launching a second instance that sends args to the running instance via `tauri_plugin_single_instance`, then exits.

That channel is one-way by construction: the plugin's callback returns nothing, and the second process calls `exit(0)` from inside the plugin's own setup hook, before any Handy code in it runs. It can ask the app to do something; it can never receive a result back.

### CLI Control Socket

For requests that need a reply — `handy FILE.wav` returning a transcript, `handy status` — the app also listens on a request/response local socket: a Unix domain socket on macOS/Linux, a named pipe on Windows, via the `interprocess` crate. Newline-delimited JSON, with bulk audio riding the same connection as a length-declared attachment.

`src-tauri` is a cargo workspace with three members:

- `crates/handy-core` — protocol, endpoint, transport, IPC client, WAV decoding. **Must never depend on tauri, transcribe-cpp, transcribe-rs or cpal.** Those link unconditionally through build scripts, so anything importing them makes the CLI 38 MB instead of 1.5 MB. This is why the IPC _server_ stays in the app while only the client is shared, and why `LeaseOwner` lives in the protocol rather than in `managers`.
- `crates/handy-cli` — the `handy` binary. Console subsystem, so no Windows console attachment hack.
- the app itself — `src/ipc/` holds only `server.rs` and `handlers/`, re-exporting the rest from `handy-core`.

The CLI models only client-facing verbs. App flags (`--toggle-transcription`, `-f`, `--start-hidden`, …) are detected in argv by `is_app_invocation` and the command line is `exec`ed into the app binary verbatim, so each flag has exactly one definition and the two cannot drift.

The single-instance plugin is untouched and unaffected; the two mechanisms coexist. See [docs/cli.md](docs/cli.md) for the protocol, the security model, and the recipe for adding a request kind.

Client work never enters the app process at all — that is the point of the separate binary. Were the CLI ever folded back into `src/main.rs`, it would have to run before `tauri::Builder` is touched, because the single-instance plugin forwards argv to the running app and exits from inside its own setup hook, so anything printed afterwards never happens.

### Engine Lease

`TranscriptionManager` hands the engine out of its `Mutex<Option<LoadedEngine>>` for the duration of a call rather than holding the mutex across inference. Every path that needs the engine therefore acquires an `EngineLease` first (`src-tauri/src/managers/engine_lease.rs`) and holds it for the whole operation. Waiters block on a condvar instead of failing, and `Batch` waiters — real dictations, with a human watching — take priority over queued `Cli` jobs.

Do not add a queue in front of the lease. The lease is the thing that serializes engine access; a second scheduler on top re-decides the same question and breaks fail-fast requests, which then wait behind a running job instead of being told the engine is busy.

### Hotkey Bridge

The handy-keys manager thread in `src-tauri/src/shortcut/handy_keys.rs` blocks
on a single channel. Hotkey events and register/unregister commands arrive on
two receivers that `std::sync::mpsc` cannot select across, so a forwarder thread
takes the event receiver (via the `take_event_receiver` addition in the pinned
handy-keys fork), blocks on it, and republishes each event onto the command
channel as `ManagerCommand::Event`.

Do not reintroduce a timeout on that loop. It previously polled both receivers
every 10 ms, which cost 100 wakeups per second forever and measured as two
thirds of the process's total CPU on a long-lived instance. Widening the timeout
does not help either — it bounds hotkey latency rather than command latency, so
it trades idle CPU for lag on every dictation keypress. The loop logs a running
item count at debug level; if it climbs while the machine is idle, something has
started polling again. See [docs/idle_cpu_hotkey_bridge.md](docs/idle_cpu_hotkey_bridge.md).

Registration order is also load-bearing. The cancel binding is registered when
recording starts and unregistered when it stops, and those two calls must arrive
in that order. They reach the manager through `queue_register`/`queue_unregister`,
which send without waiting; do not put them back on `tauri::async_runtime::spawn`
to avoid blocking, because two independent spawns have no order relative to each
other and the unregister can overtake the register it undoes. That left Escape
registered after recording ended and broke the next recording's binding.

## Internationalization (i18n)

All user-facing strings must use i18next translations. ESLint enforces this (no hardcoded strings in JSX).

**Adding new text:**

1. Add key to `src/i18n/locales/en/translation.json`
2. Use in component: `const { t } = useTranslation(); t('key.path')`

**File structure:**

```
src/i18n/
├── index.ts           # i18n setup
├── languages.ts       # Language metadata
└── locales/
    ├── en/translation.json  # English (source)
    ├── de/, es/, fr/, ja/, ru/, zh/, ...
    └── ...
```

For translation contribution guidelines, see [CONTRIBUTING_TRANSLATIONS.md](CONTRIBUTING_TRANSLATIONS.md).

## Code Style

**Rust:**

- Run `cargo fmt` and `cargo clippy` before committing
- Handle errors explicitly (avoid unwrap in production)
- Use descriptive names, add doc comments for public APIs

**TypeScript/React:**

- Strict TypeScript, avoid `any` types
- Functional components with hooks
- Tailwind CSS for styling
- Path aliases: `@/` → `./src/`

## CLI Parameters

Handy supports command-line parameters on all platforms for integration with scripts, window managers, and autostart configurations.

**Implementation:** `cli/mod.rs` (definitions), `cli/client.rs` (client-mode dispatch), `main.rs` (parsing), `lib.rs` (applying), `signal_handle.rs` (shared logic)

| Flag                     | Description                                                |
| ------------------------ | ---------------------------------------------------------- |
| `--toggle-transcription` | Toggle recording on/off on a running instance              |
| `--toggle-post-process`  | Toggle recording with post-processing on/off               |
| `--cancel`               | Cancel the current operation on a running instance         |
| `--start-hidden`         | Launch without showing the main window (tray icon visible) |
| `--no-tray`              | Launch without system tray (closing window quits the app)  |
| `--debug`                | Enable debug mode with verbose (Trace) logging             |
| `--no-ipc`               | Launch without the CLI control socket                      |

Subcommands (`transcribe`, `status`, `ping`) and the bare form `handy FILE.wav` go over the control socket instead; see [docs/cli.md](docs/cli.md).

**Key design decisions:**

- CLI flags are runtime-only overrides — they do NOT modify persisted settings
- Remote control flags work via `tauri_plugin_single_instance`: second instance sends args, then exits
- `send_transcription_input()` in `signal_handle.rs` is shared between signal handlers and CLI
- `handy FILE.wav` works via a clap `external_subcommand` catch-all, re-parsed as `transcribe`. With an optional subcommand, clap resolves the first token against subcommand names before positionals, so a bare path would otherwise fail as an unrecognised subcommand. Every pre-existing flag still parses unchanged — `cli/client.rs` has the regression tests that pin this.
- The client decodes audio and sends samples; the server never opens a caller-supplied path. That avoids macOS TCC prompts against the GUI process, resolves relative paths against the user's shell rather than the app's, and keeps the socket from being a file-read oracle.

## Debug Mode

Access debug features: `Cmd+Shift+D` (macOS) or `Ctrl+Shift+D` (Windows/Linux)

## Platform Notes

- **macOS**: Metal acceleration, accessibility permissions required for keyboard shortcuts, and `SDKROOT` pinned to SDK 14.4+ for every build (see Development Commands)
- **Windows**: Vulkan acceleration, code signing
- **Linux**: OpenBLAS + Vulkan, limited Wayland support, overlay uses GTK layer shell (disable with `HANDY_NO_GTK_LAYER_SHELL=1`)

## Troubleshooting

See the [Troubleshooting](README.md#troubleshooting) section in README.md.

## GitHub workflow for AI coding assistants

**MANDATORY. Before opening any PR, issue, or discussion in this repo: you MUST read the relevant template file and follow it strictly.** That includes sections that look "ceremonial" — checklists, AI Assistance disclosures, "Human Written Description". A generic Summary/Test-plan layout is not acceptable.

- **Opening a PR:** Read [`.github/PULL_REQUEST_TEMPLATE.md`](.github/PULL_REQUEST_TEMPLATE.md). Every section listed there is mandatory. If a section requires a human-written paragraph (e.g. "Human Written Description"), leave a clear TODO placeholder and ask the human contributor to fill it in — do not invent their voice.
- **Opening an issue:** Read [`.github/ISSUE_TEMPLATE/`](.github/ISSUE_TEMPLATE/). Blank issues are disabled; pick the right template (`bug_report.md` for bugs). Feature requests do not belong in issues — they go to [Discussions](https://github.com/cjpais/Handy/discussions) (see `.github/ISSUE_TEMPLATE/config.yml`).
- **Proposing a feature:** Handy is under a feature freeze. New features require community support gathered in [Discussions](https://github.com/cjpais/Handy/discussions) before any PR is opened — see the PR template's "Community Feedback" section.
- **Translations:** Follow [CONTRIBUTING_TRANSLATIONS.md](CONTRIBUTING_TRANSLATIONS.md).
- **Full contributor workflow:** [CONTRIBUTING.md](CONTRIBUTING.md).

**Commits:** Use conventional commit prefixes (`feat:`, `fix:`, `docs:`, `refactor:`, `chore:`). Focus the message on _why_, not _what_.
