# Built-in microphone with the lid closed

Apple laptops cut the built-in microphone off in hardware while the lid is
closed (Apple's "hardware microphone disconnect", on every T2 and Apple
silicon Mac). CoreAudio still lists the device and opens it happily; it just
delivers silence. A recording from it transcribes to nothing, and the user
only finds out afterwards.

So `AudioRecordingManager::try_start_recording` refuses that recording up
front with `BUILTIN_MIC_LID_CLOSED_ERROR`, and the user sees an error card in
the overlay asking them to connect a working microphone.

## When the refusal fires

The check runs right after `start_microphone_stream()` succeeds, against the
device the stream actually opened on (`AudioRecorder::device_name`). Checking
the opened device instead of the settings covers every way of ending up on
the built-in microphone:

- it is the selected microphone;
- nothing is selected and it is the system default;
- the selected or clamshell microphone is missing, and resolution fell back
  to the system default.

Two probes decide it, cheapest first:

1. **Lid closed.** `helpers::clamshell::is_clamshell` reads
   `AppleClamshellState` off `IOPMrootDomain` through IOKit, ~10 µs. It used
   to spawn `ioreg`, ~10 ms, which would have landed on keypress-to-capture
   latency for every laptop user once this check ran on every start.
2. **Built-in device.** `audio_toolkit::is_builtin_input_device` looks the
   device up by cpal's name with coreaudio-rs (the same helper cpal names
   devices with) and compares its `kAudioDevicePropertyTransportType` with
   `kAudioDeviceTransportTypeBuiltIn`, ~0.3 ms. The name itself is useless for
   this: macOS localizes it ("MacBook Pro Microphone", "MacBook
   Pro-Mikrofon").

With the lid open, only the first probe runs.

## A stream that was already open

In always-on mode, and for 30 s after a recording in on-demand mode with lazy
close, the stream is already open when recording starts. It may have been
opened before the lid closed, or before the user plugged in a microphone that
is now the system default. Refusing then would blame the user for a stale
stream, so the manager first closes it, clears the device cache, reopens on
whatever the settings resolve to now, and checks again.

On refusal the stream is closed in both modes. Holding a dead microphone open
only lights the privacy indicator, and closing restores the system mute if
the always-on start sound had already applied it. The next start reopens it.

## Showing the error

The main window's toast is not enough: with the lid closed, Handy is usually
in the tray and the main window is hidden. `overlay::show_error_overlay` swaps
the recording pill for an enlarged error card (`--ov-error-w`, window
`OVERLAY_ERROR_WIDTH` x `OVERLAY_ERROR_HEIGHT`) for six seconds. It is shown
even when the overlay style is None, because that setting is about the
recording indicator, not about errors that stop a recording from starting.
The card's text comes from the `overlay-error` event, keyed by the same
`error_type` the `recording-error` toast uses.

Every current translation was measured in headless Chrome at 382 x 80-97
(one-line title, two- or three-line body). The 128-point window leaves room
for one more wrapped line; re-measure if a translation grows.

## Overlay show/hide ordering

The card's timed hide made a latent race likely: the user sees the error,
plugs in a microphone and presses the hotkey within those six seconds, and
the stale hide fires under the new recording. Two rules now prevent it:

- `hide_recording_overlay` runs on the main thread, where every show already
  runs, so a hide and a show land in the order they were requested.
- `OVERLAY_SHOW_GENERATION` is bumped by every show. A delayed hide (the
  error card's six seconds, and the 300 ms wait for the fade-out before the
  window itself is hidden) records it when scheduled and only acts on the main
  thread if it is unchanged.

Do not hide the overlay window from a background thread, or behind a timer
that skips the generation check.
