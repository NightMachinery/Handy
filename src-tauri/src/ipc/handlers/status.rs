//! `Status` — what the running app is doing right now.
//!
//! Answered inline on the connection thread: it takes no locks that a running
//! job holds, so it stays responsive while a long transcription is in flight.
//! That makes it the natural smoke test for the whole transport.

use crate::ipc::protocol::{ResultBody, StatusBody, PROTOCOL_VERSION};
use crate::ipc::server::{HandlerResult, ServerContext};
use crate::managers::audio::AudioRecordingManager;
use crate::settings::get_settings;
use std::sync::Arc;
use tauri::Manager;

pub fn handle(ctx: &Arc<ServerContext>) -> HandlerResult {
    let settings = get_settings(&ctx.app);
    let transcription = &ctx.transcription;

    let recording = ctx
        .app
        .try_state::<Arc<AudioRecordingManager>>()
        .is_some_and(|a| a.is_recording());

    Ok(ResultBody::Status(Box::new(StatusBody {
        app_version: ctx.app.package_info().version.to_string(),
        protocol: PROTOCOL_VERSION,
        pid: std::process::id(),
        uptime_secs: ctx.started_at.elapsed().as_secs(),
        selected_model: settings.selected_model,
        loaded_model: transcription.get_current_model(),
        backend: transcription.current_backend(),
        model_loaded: transcription.is_model_loaded(),
        engine_busy: transcription.engine_busy(),
        recording,
        streaming: transcription.is_streaming(),
        post_process_enabled: settings.post_process_enabled,
    })))
}
