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

The second is in handy-keys itself: `event_loop` calls
`listener.recv_timeout(100ms)` purely so it can re-check a `running` flag ten
times a second. That thread held about four minutes. It is still there; see
"Still open" below.

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

Measured on the same machine, idle, with the same `log_level = debug`:

Before, the process drew 0.10 s of CPU per 50 s — 0.20% of a core doing
nothing. After, 0.04 s per 60 s, or 0.067%. That is a 66% reduction, which
matches the share the polling thread had been measured to hold.

Per thread, the loop that had accumulated 25 minutes 12 seconds over the old
instance's lifetime now reports 0:00.00 in `ps -M`. The largest remaining
consumer is the handy-keys `event_loop` thread, still sitting in
`semaphore_timedwait_trap` on its 100 ms poll — see "Still open".

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

## Still open

`event_loop`'s 100 ms poll in handy-keys is unchanged. Making it block would
deadlock the manager's `Drop`, which sets the `running` flag and then joins that
thread — a blocked `recv()` would never return. Fixing it properly means letting
the listener be dropped from outside the loop so the channel disconnects, which
is a larger change to shutdown ordering than the win justifies on its own.
