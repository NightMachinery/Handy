# Merge decisions: upstream v0.7.2 → v0.9.4+ into `codex/night`

Date: 2026-08-07.
Merge base: `3eb370d` (v0.7.2). Our side: 3 commits (configurable stdin/stdout
command filter, `~` expansion in filter args, macOS build guide). Upstream side:
285 commits, 212 files, +41.5k/−6k lines, releases 0.7.3 through 0.9.4 plus
unreleased 0.9.5 work.

This document explains every merge decision behaviorally: what each side was
doing, what we chose, what the alternatives were, and the trade-offs. It is the
companion to the merge commit itself.

## What upstream did (summary of the 285 commits)

- Transcription engine replaced with transcribe.cpp (0.9.0): streaming models
  with live partial text, a new streaming overlay (`OverlayStyle =
  none|minimal|live`), a model catalog system, many new model families
  (Voxtral, Granite, Qwen3 ASR, SenseVoice, Canary, ...).
- GPU acceleration settings (CPU/GPU/CUDA/DirectML/ROCm), per-model GPU device
  selection.
- Paste subsystem rewrite: `paste_tx` module, reliable-paste mode, Linux typing
  tool selection, auto-submit after paste, external-script paste method.
- Audio pipeline: device-default sample rates with resampling, VAD toggle,
  channel selection, mic recovery, Windows mic permissions.
- Shortcut/input hardening: new `transcription_coordinator` (X11 auto-repeat,
  push-to-talk), secure-input detection with fallback re-registration,
  keyboard diagnostics.
- LLM post-processing: structured outputs, more providers, `<think>` stripping,
  prompt-injection defense, skip-on-blank.
- History: pagination, push updates, re-transcribe stored recordings.
- Cancellation infrastructure: `cancel_generation` tokens polled at every
  pipeline stage; dropped futures instead of run-to-completion.
- App shell: theme selector, What's New viewer, live log viewer, tray overhaul,
  portable mode, CLI args (`--toggle-post-process`), 8 new locales (24 total),
  new CI (prettier, translation completeness, cargo test).

Upstream has no feature overlapping our command filter. Their
`external_script_path` is a Linux *paste backend* (types text via a user
script), not a transcript transform; no settings or behavior collide. Our
feature remains purely additive.

## The central decision: port, don't merge

Our command filter was spliced inline into `TranscribeAction::stop()`. Upstream
rewrote that function completely and extracted the whole transcript pipeline
(Chinese conversion → LLM post-processing → prompt lookup) into a reusable
`process_transcription_output()` in `src-tauri/src/actions.rs`, now called from
two places: the live dictation path and the new history re-transcribe command
(`src-tauri/src/commands/history.rs`).

Decision: take upstream's `actions.rs` wholesale and re-implement the filter
inside `process_transcription_output()`, rather than resolving the ~190-line
conflict hunk line by line.

- Alternative considered: keep our inline structure and graft upstream's
  cancellation/error handling around it. Rejected: upstream's `stop()` now
  weaves cancellation checks, streaming overlay phases, error events, and a
  finish-guard through the whole function; grafting our inline blocks into that
  would re-derive upstream's logic by hand and would still leave the history
  retry path without the filter.
- Trade-off accepted: our filter code moved from the hotkey handler into the
  shared pipeline function, so its behavior is now defined in one place for
  all callers. This is strictly better for maintenance, but it changed two
  user-visible behaviors (see "Behavioral changes" below).

## Behavioral changes decided during the merge

### 1. The filter now runs on history re-transcribe (decided: yes)

Because the filter lives inside `process_transcription_output()`, upstream's
new "retry transcription" button in History also runs it (when the entry
requests post-processing and the filter scope matches).

- Chosen: run the filter on retries. Retrying produces the same output a live
  dictation would; one hook point, no signature diff against upstream.
- Alternative: add a context flag to `process_transcription_output()` so
  retries skip the filter. Rejected: retried entries would silently differ
  from live results, and the extra parameter is a permanent diff against
  upstream at every future merge.
- Trade-off accepted: a filter with side effects (logging, notifications) now
  also fires from a UI button. A filter returning empty on the retry path is
  benign: the retry stores only the post-processed text, which stays unset on
  cancellation, so the entry keeps its raw transcription.

### 2. Empty filter output is now encoded as empty text, not a flag

Our old code tracked `cancelled_by_filter_empty` and had a dedicated
"suppress the paste, hide the overlay" branch. Upstream already suppresses the
paste when `final_text` is empty. Decision: on `CancelledEmpty`, the pipeline
returns an empty `final_text` and drops the prompt metadata; the flag and the
duplicated paste branch are gone.

- Trade-off accepted: we lose the distinct "cancelled by filter" state — an
  empty filter output and an empty transcription are handled identically
  downstream (an info-level log line still records the filter cancellation).
  In exchange, every current and future caller of the pipeline gets correct
  suppression for free, with zero extra merge surface.

### 3. Processing overlay now shows for filter-only runs (decided: yes)

Upstream shows the "processing" overlay state only when the post-process
hotkey fired. Our filter can be scoped to the plain transcribe hotkey; a slow
filter there would give no visual feedback until the paste lands.

- Chosen: widen the condition — the overlay also enters the processing state
  when the command filter applies to the fired hotkey (in `stop()`,
  `src-tauri/src/actions.rs`).
- Trade-off accepted: a fast filter briefly flashes the processing state where
  stock Handy would not, and the widened condition is a small permanent diff
  in upstream's overlay logic. The alternative (keep upstream's condition)
  meant silent dead air for slow filters.

### 4. LLM post-processing now additionally requires `post_process_enabled`

Kept from our fork, now inside `process_transcription_output()`: the LLM step
runs only when the post-process hotkey fired *and* AI post-processing is
enabled. Upstream gates only on the hotkey, because in stock Handy the hotkey
is registered only when the feature is on. In our fork the secondary hotkey
can be registered for filter-only reasons, so without this guard a filter-only
user would get surprise LLM calls. This also changes one edge case
deliberately: with the filter changing text but no LLM run, the history entry
records the filter output as the post-processed text.

### 5. Dropped our `post_process` → `is_post_process_hotkey` field rename

Our fork had renamed `TranscribeAction.post_process` for clarity ("which
hotkey fired" vs "is AI post-processing on"). Upstream kept `post_process` and
now passes it through the coordinator, the history retry path, and the action
map. Decision: revert to upstream's name. The rename was cosmetic, and keeping
it would have inflated this and every future merge. The clearer name survives
in parameter names (`is_post_process_hotkey`) inside `settings.rs` helpers.

## Mechanical conflict resolutions

### src-tauri/src/settings.rs

Both sides appended a `#[cfg(test)] mod tests` at the same location; the
modules were concatenated (our 3 command-filter tests + upstream's suite). Our
6 `command_filter_*` fields, the two enums, and the helper methods auto-merged
cleanly. `CURRENT_SETTINGS_SCHEMA_VERSION` stays at 1: all our fields carry
`#[serde(default)]`, so existing settings stores parse without migration, and
upstream's new per-field salvage even degrades corrupt values to defaults
instead of resetting the store.

### src-tauri/src/lib.rs

The specta builder was reshaped upstream (`.commands([...])` plus a new
`.events([...])`, fully reindented), so git could not align our 6 added
command registrations. Resolution: upstream's builder verbatim, with our 6
`change_command_filter_*_setting` commands re-inserted next to
`change_post_process_enabled_setting`. `mod command_filter;` auto-merged.

### src-tauri/src/shortcut/mod.rs — a genuine merge trap

Two conflicts. The import-list conflict was a trivial union. The second was a
misaligned splice: git matched the tail of our
`change_command_filter_timeout_setting` against the tail of upstream's
`change_post_process_enabled_setting`, whose new
`crate::secure_input::reconcile_fallback(&app)` call landed *inside our
function*. Taking either side naively would have either deleted upstream's
secure-input reconciliation from the post-process toggle or deleted our
setter. Resolution: keep our setter body, and move the reconciliation into
`sync_secondary_shortcut_registration()` — so the post-process toggle *and*
the filter enable/scope setters all reconcile the secure-input fallback after
changing hotkey registration. This is slightly broader than upstream (they
reconcile only in the post-process toggle), which is the correct behavior for
a fork where filter settings also affect hotkey registration.

### Silent regressions in files git never flagged

Upstream duplicated the "should the secondary hotkey be registered" predicate
in two new places that did not conflict, both still reading the stock
`!post_process_enabled`:

- `resume_all_shortcuts()` in `src-tauri/src/shortcut/mod.rs` — runs after the
  user records a shortcut in the UI. Unpatched, a filter-only user would lose
  their secondary hotkey after every shortcut-recording session.
- `reconcile_fallback()` in `src-tauri/src/secure_input.rs` — unpatched, the
  secondary hotkey would not be covered by the secure-input fallback for
  filter-only users.

Both now use `should_register_secondary_shortcut()`, matching the three sites
our fork already patched (`shortcut/mod.rs` registration,
`shortcut/handy_keys.rs`, `shortcut/tauri_impl.rs` — those auto-merged). This
is the classic fork hazard: the merge compiles and the conflict list looks
complete, but behavior regresses in code you never touched. It was found by
grepping for the predicate we override, which is the recommended check at
every future merge.

### src/bindings.ts

Generated by tauri-specta at debug-build startup ("Do not edit this file
manually"). Resolution: took upstream's version, then re-applied our six
command wrappers (copied verbatim from the previously *generated* file), the
two enum aliases, and the six `AppSettings` fields in upstream's new
multi-line format, mirroring the Rust struct's field order. Regenerating via a
debug run was the preferred route but is currently impossible on this machine:
upstream's new ONNX Runtime (`ort` rc12) prebuilt binary references CoreML
symbols (`MLComputePlan`, `MLOptimizationHints`) that require macOS SDK 14.4+,
and the installed Xcode ships SDK 14.2, so the app fails to link locally. The
hand-applied entries are validated by the frontend typecheck; the next debug
build on a capable machine will canonicalize any cosmetic ordering drift.

### src/components/settings/advanced/AdvancedSettings.tsx

Our fork removed the `PostProcessingToggle` from Advanced → Experimental
(relocated to the Post Process page, which our Sidebar change makes always
reachable). Upstream inserted a new `AutoSubmit` import directly above the
import we deleted, producing the conflict. Resolution: keep upstream's new
import, keep our removal. Consequence: `PostProcessingToggle.tsx` is now dead
code in our fork (upstream still uses it); left in place to minimize diff.

### i18n: en/es/ru conflicts, and 8 new locales

All three conflicts were adjacent-edit collisions at our insertion anchors,
not semantic disagreements:

- `en`: upstream deleted `settings.postProcessing.title` (the sidebar refactor
  supplies the page label from `sidebar.postProcessing`) exactly where our
  `modes` block was inserted. Kept our block, dropped the deleted key.
- `es`/`ru`: upstream retranslation passes touched `prompts.createFirst` /
  `selectToEdit` at our insertion point. Took upstream's improved
  translations, kept our key blocks.

Upstream added 8 locales (`bg da he hi ne nl sv zh-TW`) and a CI check that
every locale has every key of `en`. Our 45 keys were copied into the new
locales as English placeholders — the same approach our branch already used
for the original 15 non-English locales, and what upstream tolerates for new
features. Alternative (machine-translating them) was rejected as out of scope
for a merge; the strings remain grep-able for a future translation pass.

Our reworded `transcribe_with_post_process.description` (explaining that the
hotkey drives AI post-processing and/or the command filter) auto-merged and
survives; the stock wording would be wrong under our fork's semantics.

## Follow-up fix made alongside the merge

`run_command_filter()` spawns the user's filter process and awaits it.
Upstream's new cancellation model *drops* in-flight pipeline futures when the
user cancels. Without `kill_on_drop(true)`, a drop mid-filter would leak the
child process (it would keep running with no reader on its pipes). Added
`.kill_on_drop(true)` to the spawn in `src-tauri/src/command_filter.rs` as a
separate commit, since it is a behavior fix rather than a conflict resolution.
The explicit kill-on-timeout path is unchanged.

## What was verified

- `cargo check` / `cargo test` in `src-tauri` (includes upstream's new suites,
  our 3 settings tests, and the command-filter tests).
- `bun run check:translations`: all 23 non-English locales complete.
- Frontend `tsc && vite build` clean; `settingsStore.ts` compiles against the
  updated `bindings.ts`, which exercises the 6 commands, 2 enum types, and 6
  `AppSettings` fields.
- Not verified: launching the app (blocked by the SDK 14.2 / ort CoreML link
  failure above — pre-existing on this machine for upstream's current tree,
  unrelated to the merge). First run on a machine with Xcode SDK 14.4+ should
  smoke-test dictation with the filter enabled and regenerate `bindings.ts`.
- Grep audits: no `PostProcessingToggle` references in Advanced settings; no
  `postProcessing.title` lookups; `Sidebar.tsx` keeps `enabled: () => true`
  for the Post Process page; no residual `is_post_process_hotkey` field
  references.

## Checklist for the next upstream merge

- Grep for `post_process_enabled` in `src-tauri/` — every new gate on the
  secondary hotkey must use `should_register_secondary_shortcut()` instead.
- If `process_transcription_output()` gains callers or moves, the filter hook
  moves with it; re-read its call sites.
- Regenerate `src/bindings.ts` (debug build) instead of merging it.
- Re-run `bun run check:translations` — new upstream locales need our keys.
