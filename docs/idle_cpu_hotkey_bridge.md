# Idle CPU in the hotkey bridge

Handy used to draw measurable CPU while completely idle — no dictation, nobody
touching the keyboard. This records where it went, how it was measured, and how
to check that it has not come back.

## The symptom

On a long-lived macOS instance the process had accumulated 38 minutes of CPU
over 19 days of uptime, and drew about 0.2% of a core continuously with the
machine untouched.

## What it was not

The obvious suspect on macOS is the Accessibility permission check —
`AXIsProcessTrusted`, reached through `tauri-plugin-macos-permissions`, enigo's
`Enigo::new`, and handy-keys. It was not that. `tccd`, the daemon that answers
those requests, had used two seconds of CPU in its entire lifetime and barely
moved during observation. A permission-check storm would have shown up there.

It was also not the CGEventTap itself. That thread accounted for well under a
tenth of the total.

## What it was

Two stacked polling loops, both in the hotkey path.

The dominant one was Handy's own manager thread in
`src-tauri/src/shortcut/handy_keys.rs`. It waited on two receivers that
`std::sync::mpsc` cannot select across — hotkey events arrive on
`HotkeyManager`'s channel, register and unregister commands arrive on the
manager's own channel — so it drained `try_recv` and then called
`cmd_rx.recv_timeout(10ms)` in a loop. That is 100 wakeups per second forever.
Over the uptime above it came to roughly 165 million wakeups, and the thread
held 25 minutes 12 seconds of the 38 minute total: two thirds of everything the
process had ever spent, essentially all of it wakeup overhead.

The second is in handy-keys itself: `event_loop` called
`listener.recv_timeout(100ms)` purely so it could re-check a `running` flag ten
times a second. That thread held about four minutes. See "The second poll"
below.

Raising the 10 ms timeout was not a fix. It bounds *hotkey* latency, not command
latency — `recv_timeout` already returns immediately when a command arrives, and
events are only picked up on the next pass. A longer wait would have added that
much lag to every dictation keypress.

## The fix

Give the hotkey event receiver a thread of its own that blocks on it and
republishes each event onto the command channel as `ManagerCommand::Event`. The
manager loop then waits on a single channel with a plain `recv()` and costs
nothing at all while idle.

This needs `HotkeyManager::take_event_receiver`, which upstream handy-keys did
not have — the manager owns an `mpsc::Receiver`, which is `!Sync`, so the
manager as a whole cannot be shared between threads. `register` and `unregister`
only touch `state` and `blocking_hotkeys`, never the receiver, so moving the
receiver out is safe. The method is added in the fork pinned under
`[patch.crates-io]` in `src-tauri/Cargo.toml`.

Shutdown still terminates on its own: the loop drops the `HotkeyManager`, which
stops the listener, which drops the sending half, which ends the forwarder's
blocking `recv()`.

## The result

Measured on the same machine, idle, with the same `log_level = debug`.

Before, the process drew 0.10 s of CPU per 50 s — 0.20% of a core doing
nothing. Removing the 10 ms poll took that to 0.04 s per 60 s, or 0.067%, a 66%
cut that matches the share the polling thread had been measured to hold.
Removing the 100 ms poll as well took it to 0.05 s per 300 s, or 0.017%.

That is a 92% reduction overall, and the remainder is close enough to the 10 ms
accounting granularity that a longer window is needed to say much more about it.

Per thread, the loop that had accumulated 25 minutes 12 seconds over the old
instance's lifetime now reports 0:00.00 in `ps -M`, and no thread in the process
shows more than a single tick over a 90-second idle window.

Keep the absolute size in view: this is worth having for battery and for
long-uptime instances, but it was never a responsiveness problem.

## How to re-measure

Find the running process and watch its cumulative CPU while leaving the machine
alone. Two readings a minute apart are enough:

    ps -o time= -p "$(pgrep -x handy)"

Per-thread attribution needs a sampling profiler. Both of these work, and agree
with each other and with `ps -M` on thread ordering:

    sample "$(pgrep -x handy)" 5 1 -f /dev/stdout
    spindump "$(pgrep -x handy)" 5 10 -noBinary -o /tmp/spin.txt

A thread that sits in `semaphore_timedwait_trap` in every sample while still
accumulating millions of cycles is a wakeup loop, not work. That is the shape to
look for.

Note that `dtrace` cannot attach to the signed release bundle under SIP, so
counting calls to a specific function needs a locally built, unsigned binary.

## The invariant

The manager loop in `shortcut/handy_keys.rs` must block. Every iteration should
correspond to real work. It logs a running count at debug level for exactly this
reason: if that count climbs while nobody is using the machine, a poll has been
reintroduced.

## The second poll

`event_loop` in handy-keys had the same shape for a different reason: it woke
every 100 ms only to re-read a `running` flag, because shutdown was something it
had to notice rather than something that reached it. It owned the listener, so
nothing outside could drop it, and joining a thread parked in `recv` would have
hung.

Moving ownership fixes it. `HotkeyManager` keeps the listener and the loop gets
only the receiver, so shutdown becomes a channel disconnect: `Drop` drops the
listener, the backend thread stops, the sending half goes away, and the loop's
`recv` ends. Shutdown also stopped being bounded below by the poll interval.

The cost is that shutdown now depends on that teardown actually disconnecting
the channel. The old flag-and-timeout would exit regardless; this will hang if a
future backend change stops dropping the sender. A synthetic-input test guards
it by asserting the drop returns.

## Still open

The CGEventTap itself is created with `CGEventTapOptions::Default` rather than
`ListenOnly`, at the head of the session tap chain, with mouse-button events in
the mask. That makes every keystroke and click on the machine wait for Handy's
callback before reaching the focused app. It is a small share of Handy's own CPU
and so was not what this investigation was chasing, but it is the more
interesting number: the cost lands on everything else running, not here.
`TapDisabledByTimeout` is silently re-enabled with no logging, so overruns are
currently invisible.
