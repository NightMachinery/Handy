# The cancel binding must not reconfigure shortcuts inline

Pressing Escape to cancel a recording froze Handy completely: no overlay
teardown, no tray change, no response to any hotkey, and the window server
eventually killed the webview. The only way out was to kill the process and
relaunch. It reproduced on every attempt.

The log ends mid-cancel, which is what pins it down:

```
17:37:36 tauri global-shortcut event: binding=cancel, shortcut=Escape, state=Pressed
17:37:36 Initiating operation cancellation...
17:38:03 web content process terminated
```

`Initiating operation cancellation...` is printed by
`utils::cancel_current_operation`, and the next statement in that function is
`shortcut::unregister_cancel_shortcut(app)`. Nothing after it ever ran.

## Why it deadlocked

`tauri-plugin-global-shortcut` dispatches to our handler like this:

```rust
GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
    if let Some(shortcut) = shortcuts_.lock().unwrap().get(&e.id) {
        handler(&app_handle, &shortcut.shortcut, e);
    }
}));
```

The `MutexGuard` produced by `shortcuts_.lock()` is a temporary of the `if let`
scrutinee, so it stays alive for the whole body. Our handler therefore runs
while the plugin holds the lock on its own shortcut map.

`reconcile_cancel_shortcut` opened with
`app.global_shortcut().is_registered(shortcut)`, which locks that same map.
`std::sync::Mutex` is not reentrant, so the thread blocked on a lock it was
already holding, forever.

On macOS the hotkey callback is a Carbon handler installed on
`GetApplicationEventTarget()`, so the thread that died is the **main thread**.
That is why the whole GUI went with it rather than just the shortcut system.

Cancel was the only binding that could trigger this. `shortcut::handler` routes
every transcribe binding through `TranscriptionCoordinator::send_input`, an
mpsc send, so those actions run on the coordinator thread — outside the guard,
and free to call back into the plugin. Cancel alone is executed inline in the
callback.

The tray's Cancel item and `handy --cancel` reach the same code but are safe:
they are not inside the plugin's guard. `run_on_main_thread` is also not a
hazard here, because `tauri-runtime-wry` runs the task directly when the caller
is already the main thread rather than queueing it and waiting.

## The rule

Do not call into `global_shortcut()` — `register`, `unregister` or even the
read-only `is_registered` — from inside a global-shortcut handler. Hand the work
to another thread.

`request_cancel_reconcile` in `src-tauri/src/shortcut/tauri_impl.rs` does that
with one dedicated thread fed by a channel. A thread rather than
`tauri::async_runtime::spawn` for two reasons: reconciles have to stay ordered
relative to each other, and a spawned task was measured taking seven seconds
under load, by which point the next recording had already set the intent back,
the reconcile correctly did nothing, and Escape stayed registered globally while
Handy sat idle. A thread that does nothing else cannot be queued behind other
work.

The `CANCEL_WANTED` intent flag is read when a request runs, not when it is
queued, so a request that waited behind a newer one converges on the newer
intent instead of undoing it.

## History

The inline call arrived with `819e789`, which replaced a
`tauri::async_runtime::spawn` with a direct call to fix the leaked-Escape
problem described above. The spawn had been accidentally protecting against
this: it got the work off the callback thread. Fixing the leak without keeping
that property is what introduced the freeze.
