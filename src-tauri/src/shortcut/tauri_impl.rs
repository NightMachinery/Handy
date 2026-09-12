//! Tauri global-shortcut implementation
//!
//! This module provides shortcut functionality using Tauri's built-in
//! global-shortcut plugin.

use log::{debug, error, warn};
use tauri::AppHandle;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

#[cfg(not(target_os = "linux"))]
use crate::settings::get_settings;
use crate::settings::{self, ShortcutBinding};

use super::handler::handle_shortcut_event;

/// Initialize shortcuts using Tauri's global-shortcut plugin
pub fn init_shortcuts(app: &AppHandle) {
    let default_bindings = settings::get_default_settings().bindings;
    let user_settings = settings::load_or_create_app_settings(app);

    // Register all default shortcuts, applying user customizations
    for (id, default_binding) in default_bindings {
        if id == "cancel" {
            continue; // Skip cancel shortcut, it will be registered dynamically
        }
        // Skip secondary processing shortcut when no feature needs it
        if id == "transcribe_with_post_process"
            && !user_settings.should_register_secondary_shortcut()
        {
            continue;
        }
        let binding = user_settings
            .bindings
            .get(&id)
            .cloned()
            .unwrap_or(default_binding);

        if let Err(e) = register_shortcut(app, binding) {
            error!("Failed to register shortcut {} during init: {}", id, e);
        }
    }
}

/// Validate a shortcut string for the Tauri global-shortcut implementation.
/// Tauri requires at least one non-modifier key and doesn't support the fn key.
pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err("Shortcut cannot be empty".into());
    }

    let modifiers = [
        "ctrl", "control", "shift", "alt", "option", "meta", "command", "cmd", "super", "win",
        "windows",
    ];

    // Check for fn key which Tauri doesn't support
    let parts: Vec<String> = raw.split('+').map(|p| p.trim().to_lowercase()).collect();
    for part in &parts {
        if part == "fn" || part == "function" {
            return Err("The 'fn' key is not supported by Tauri global shortcuts".into());
        }
    }

    // Check for at least one non-modifier key
    let has_non_modifier = parts.iter().any(|part| !modifiers.contains(&part.as_str()));

    if has_non_modifier {
        Ok(())
    } else {
        Err("Tauri shortcuts must include a main key (letter, number, F-key, etc.) in addition to modifiers".into())
    }
}

/// Register a shortcut using Tauri's global-shortcut plugin
pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    // Validate for Tauri requirements
    if let Err(e) = validate_shortcut(&binding.current_binding) {
        warn!(
            "register_tauri_shortcut validation error for binding '{}': {}",
            binding.current_binding, e
        );
        return Err(e);
    }

    // Parse shortcut and return error if it fails
    let shortcut = match binding.current_binding.parse::<Shortcut>() {
        Ok(s) => s,
        Err(e) => {
            let error_msg = format!(
                "Failed to parse shortcut '{}': {}",
                binding.current_binding, e
            );
            error!("register_tauri_shortcut parse error: {}", error_msg);
            return Err(error_msg);
        }
    };

    // Prevent duplicate registrations that would silently shadow one another
    if app.global_shortcut().is_registered(shortcut) {
        let error_msg = format!("Shortcut '{}' is already in use", binding.current_binding);
        warn!("register_tauri_shortcut duplicate error: {}", error_msg);
        return Err(error_msg);
    }

    // Clone binding.id for use in the closure
    let binding_id_for_closure = binding.id.clone();

    app.global_shortcut()
        .on_shortcut(shortcut, move |app_handle, scut, event| {
            if scut == &shortcut {
                let shortcut_string = scut.into_string();
                let is_pressed = event.state == ShortcutState::Pressed;
                // Mirrors the handy-keys event log line; the distinct prefix
                // makes it possible to tell which backend fired a shortcut
                // (e.g. when diagnosing the Secure Input fallback)
                debug!(
                    "tauri global-shortcut event: binding={}, shortcut={}, state={:?}",
                    binding_id_for_closure, shortcut_string, event.state
                );
                handle_shortcut_event(
                    app_handle,
                    &binding_id_for_closure,
                    &shortcut_string,
                    is_pressed,
                );
            }
        })
        .map_err(|e| {
            let error_msg = format!(
                "Couldn't register shortcut '{}': {}",
                binding.current_binding, e
            );
            error!("register_tauri_shortcut registration error: {}", error_msg);
            error_msg
        })?;

    Ok(())
}

/// Unregister a shortcut from Tauri's global-shortcut plugin
pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let shortcut = match binding.current_binding.parse::<Shortcut>() {
        Ok(s) => s,
        Err(e) => {
            let error_msg = format!(
                "Failed to parse shortcut '{}' for unregistration: {}",
                binding.current_binding, e
            );
            error!("unregister_tauri_shortcut parse error: {}", error_msg);
            return Err(error_msg);
        }
    };

    app.global_shortcut().unregister(shortcut).map_err(|e| {
        let error_msg = format!(
            "Failed to unregister shortcut '{}': {}",
            binding.current_binding, e
        );
        error!("unregister_tauri_shortcut error: {}", error_msg);
        error_msg
    })?;

    Ok(())
}

/// Register the cancel shortcut (called when recording starts)
/// Whether the cancel binding *should* be registered right now.
///
/// Registration and unregistration are requested from different threads and
/// serviced asynchronously, so the two requests have no order relative to each
/// other. Rather than try to impose one, both sides record the intent here and
/// ask for a reconcile; whichever reconcile runs last reads the latest intent
/// and converges on it. Ordering then cannot matter.
#[cfg(not(target_os = "linux"))]
static CANCEL_WANTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Bring the cancel binding in line with `CANCEL_WANTED`. Idempotent: it checks
/// what is actually registered first, so a duplicate reconcile is a no-op
/// rather than an "already in use" error.
///
/// Called inline, deliberately. Dispatching this onto the async runtime was
/// measured taking seven seconds to run under load, by which time the next
/// recording had set the intent back and the reconcile became a no-op — so
/// Escape stayed registered globally while Handy sat idle. The work here is a
/// settings read, a parse and one plugin call, and registration is known to
/// work off the main thread, so there is nothing to gain by deferring it.
#[cfg(not(target_os = "linux"))]
fn reconcile_cancel_shortcut(app: &AppHandle) {
    use std::sync::atomic::Ordering;

    let wanted = CANCEL_WANTED.load(Ordering::SeqCst);
    let Some(binding) = get_settings(app).bindings.get("cancel").cloned() else {
        return;
    };
    let Ok(shortcut) = binding.current_binding.parse::<Shortcut>() else {
        // register_shortcut logs the parse failure in detail; nothing to do.
        return;
    };
    let shortcut_str = binding.current_binding.clone();

    let registered = app.global_shortcut().is_registered(shortcut);
    // Logged because the Tauri path was previously silent on success, which is
    // why a cancel binding that failed to register went unnoticed. Mirrors the
    // handy-keys backend's registration lines.
    if wanted && !registered {
        match register_shortcut(app, binding) {
            Ok(()) => debug!("Registered tauri cancel shortcut: {}", shortcut_str),
            Err(e) => error!("Failed to register cancel shortcut: {}", e),
        }
    } else if !wanted && registered {
        match unregister_shortcut(app, binding) {
            Ok(()) => debug!("Unregistered tauri cancel shortcut: {}", shortcut_str),
            Err(e) => error!("Failed to unregister cancel shortcut: {}", e),
        }
    }
}

/// Register the cancel shortcut (called when recording starts)
pub fn register_cancel_shortcut(app: &AppHandle) {
    // Cancel shortcut is disabled on Linux due to instability with dynamic shortcut registration
    #[cfg(target_os = "linux")]
    {
        let _ = app;
        return;
    }

    #[cfg(not(target_os = "linux"))]
    {
        CANCEL_WANTED.store(true, std::sync::atomic::Ordering::SeqCst);
        reconcile_cancel_shortcut(app);
    }
}

/// Unregister the cancel shortcut (called when recording stops)
pub fn unregister_cancel_shortcut(app: &AppHandle) {
    #[cfg(target_os = "linux")]
    {
        let _ = app;
        return;
    }

    #[cfg(not(target_os = "linux"))]
    {
        CANCEL_WANTED.store(false, std::sync::atomic::Ordering::SeqCst);
        reconcile_cancel_shortcut(app);
    }
}
