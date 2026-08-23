//! `Transcribe` — run a buffer of audio through the app's loaded engine.
//!
//! Runs on the shared job worker, so only one of these is ever in flight. The
//! engine lease is what actually excludes a concurrent dictation; the queue
//! just keeps several CLI clients from thundering on it.

use crate::actions::process_transcription_output;
use crate::audio_toolkit::{save_wav_file, verify_wav_file};
use crate::ipc::protocol::{
    AudioSource, ErrorCode, ResultBody, Stage, TranscribeRequest, TranscriptBody, WaitPolicy,
};
use crate::ipc::server::{HandlerError, HandlerResult, JobContext};
use crate::managers::engine_lease::{LeaseError, LeaseOwner};
use crate::settings::get_settings;
use crate::utils;
use log::{error, info, warn};
use std::time::{Duration, Instant};
use tauri::Emitter;

pub fn handle(
    job: &JobContext,
    request: TranscribeRequest,
    audio: Option<Vec<f32>>,
) -> HandlerResult {
    let samples = resolve_audio(&request, audio)?;
    if samples.is_empty() {
        return Err(HandlerError::new(
            ErrorCode::BadRequest,
            "the audio payload contained no samples",
        ));
    }
    let audio_secs = samples.len() as f64 / 16_000.0;

    job.check_cancelled()?;

    // ----- claim the engine -------------------------------------------------
    job.progress(Stage::WaitingForEngine, None);
    let queue_start = Instant::now();
    let transcription = &job.ctx.transcription;
    let lease = match request.wait_policy {
        WaitPolicy::FailFast => transcription
            .try_lease_engine(LeaseOwner::Cli)
            .map_err(lease_error)?,
        WaitPolicy::Wait { timeout_ms } => transcription
            .lease_engine_wait(
                LeaseOwner::Cli,
                timeout_ms.map(Duration::from_millis),
                Some(&job.cancel),
            )
            .map_err(lease_error)?,
    };
    let queue_ms = queue_start.elapsed().as_millis() as u64;
    job.check_cancelled()?;

    // ----- make sure the right model is loaded ------------------------------
    let settings = get_settings(&job.ctx.app);
    let previous_model = transcription.get_current_model();
    let wanted = match request.model.clone() {
        Some(id) => id,
        None => previous_model
            .clone()
            .unwrap_or_else(|| settings.selected_model.clone()),
    };
    if wanted.is_empty() {
        return Err(HandlerError::new(
            ErrorCode::NoModelSelected,
            "no model is selected; pick one in Handy or pass --model",
        ));
    }
    if job.ctx.models.get_model_info(&wanted).is_none() {
        return Err(HandlerError::new(
            ErrorCode::ModelNotAvailable,
            format!("unknown model '{}'; run `handy --list-models`", wanted),
        ));
    }

    let needs_load =
        previous_model.as_deref() != Some(wanted.as_str()) || !transcription.is_model_loaded();
    let mut load_ms = 0;
    if needs_load {
        job.progress(
            Stage::LoadingModel,
            Some(format!("loading model '{}'", wanted)),
        );
        let start = Instant::now();
        // load_model_with_device, not switch_active_model: a CLI job must not
        // rewrite the user's persisted model selection or fire the
        // selection-changed events the settings UI listens for.
        transcription
            .load_model_with_device(&wanted, None)
            .map_err(|e| {
                HandlerError::new(
                    ErrorCode::ModelLoadFailed,
                    format!("could not load model '{}': {}", wanted, e),
                )
            })?;
        load_ms = start.elapsed().as_millis() as u64;
    }
    // Restore whatever the app had loaded, so a one-off --model does not leave
    // the GUI pointing at a different engine than its own settings say.
    let restore = RestoreModel::new(job, &previous_model, &wanted, request.restore_model);

    job.check_cancelled()?;

    // ----- transcribe -------------------------------------------------------
    job.progress(Stage::Transcribing, None);
    let start = Instant::now();

    // Streaming first when the model supports it: it is the only path that can
    // report a measured completion fraction, and its feed loop is ours, so
    // cancellation lands within a chunk instead of after the whole inference.
    let mut streamed = false;
    let mut raw_text = None;
    if request.allow_streaming {
        let want_partials = request.want_partials;
        let streaming = transcription
            .transcribe_streaming_with_lease(&lease, &samples, &job.cancel, |progress| {
                job.progress_with_fraction(Stage::Transcribing, None, progress.fraction());
                if want_partials {
                    job.partial(&progress.committed_text, &progress.tentative_text);
                }
            })
            .map_err(|e| {
                let code = if job.is_cancelled() {
                    ErrorCode::Cancelled
                } else {
                    ErrorCode::TranscriptionFailed
                };
                HandlerError::new(code, e.to_string())
            })?;
        if let Some(text) = streaming {
            streamed = true;
            raw_text = Some(text);
        }
    }

    // Batch fallback: the model cannot stream, or streaming declined to start.
    let raw_text = match raw_text {
        Some(text) => text,
        None => {
            job.check_cancelled()?;
            transcription
                .transcribe_with_lease(&lease, samples.clone())
                .map_err(|e| HandlerError::new(ErrorCode::TranscriptionFailed, e.to_string()))?
        }
    };
    let transcribe_ms = start.elapsed().as_millis() as u64;
    let backend = transcription.current_backend();

    // Release the engine before post-processing: the LLM call can take seconds
    // and there is no reason to make a waiting dictation sit through it.
    drop(restore);
    drop(lease);

    job.check_cancelled()?;

    // ----- post-process, save, paste ----------------------------------------
    let mut post_process_applied = false;
    let (final_text, post_processed_text, post_process_prompt) = if request.post_process {
        job.progress(Stage::PostProcessing, None);
        let processed = tauri::async_runtime::block_on(process_transcription_output(
            &job.ctx.app,
            &raw_text,
            true,
        ));
        post_process_applied = processed.post_processed_text.is_some();
        (
            processed.final_text,
            processed.post_processed_text,
            processed.post_process_prompt,
        )
    } else {
        (raw_text.clone(), None, None)
    };

    let history_id = if request.save_history {
        job.progress(Stage::SavingHistory, None);
        save_history(
            job,
            &samples,
            &raw_text,
            &request,
            &post_processed_text,
            &post_process_prompt,
        )
    } else {
        None
    };

    let pasted = if request.paste && !final_text.is_empty() {
        job.progress(Stage::Pasting, None);
        paste_text(job, final_text.clone())
    } else {
        false
    };

    let rtf = if transcribe_ms > 0 {
        audio_secs / (transcribe_ms as f64 / 1000.0)
    } else {
        0.0
    };
    info!(
        "CLI transcription: {:.2}s of audio in {}ms ({:.2}x real-time) using '{}'",
        audio_secs, transcribe_ms, rtf, wanted
    );

    Ok(ResultBody::Transcript(Box::new(TranscriptBody {
        text: final_text,
        raw_text,
        post_processed: request.post_process,
        post_process_applied,
        model: wanted,
        backend,
        audio_secs,
        queue_ms,
        load_ms,
        transcribe_ms,
        rtf,
        cold_load: needs_load,
        streamed,
        history_id,
        pasted,
    })))
}

/// Inline audio arrives decoded; a `Path` source is read here, and is only
/// reachable when a caller explicitly opted into server-side reads.
fn resolve_audio(
    request: &TranscribeRequest,
    audio: Option<Vec<f32>>,
) -> Result<Vec<f32>, HandlerError> {
    match (&request.audio, audio) {
        (AudioSource::Inline { .. }, Some(samples)) => Ok(samples),
        (AudioSource::Inline { .. }, None) => Err(HandlerError::new(
            ErrorCode::BadRequest,
            "inline audio was declared but no payload followed",
        )),
        (AudioSource::Path { path }, _) => crate::audio_toolkit::decode_to_16k_mono(path)
            .map(|decoded| decoded.samples)
            .map_err(|e| HandlerError::new(ErrorCode::IoError, e.to_string())),
    }
}

fn lease_error(err: LeaseError) -> HandlerError {
    let code = match err {
        LeaseError::Busy(_) | LeaseError::Timeout(_) => ErrorCode::Busy,
        LeaseError::Cancelled => ErrorCode::Cancelled,
    };
    HandlerError::new(code, err.to_string())
}

/// Restores the previously loaded model when the job used a `--model` override.
struct RestoreModel<'a> {
    job: &'a JobContext,
    target: Option<String>,
}

impl<'a> RestoreModel<'a> {
    fn new(
        job: &'a JobContext,
        previous: &Option<String>,
        used: &str,
        enabled: bool,
    ) -> Option<Self> {
        let previous = previous.clone()?;
        if !enabled || previous == used {
            return None;
        }
        warn!(
            "CLI job temporarily loaded '{}' (was '{}'); will restore",
            used, previous
        );
        Some(Self {
            job,
            target: Some(previous),
        })
    }
}

impl Drop for RestoreModel<'_> {
    fn drop(&mut self) {
        let Some(target) = self.target.take() else {
            return;
        };
        // Off-thread and after the lease is released, so restoring never
        // delays the client's result.
        let transcription = std::sync::Arc::clone(&self.job.ctx.transcription);
        std::thread::spawn(move || {
            if let Err(e) = transcription.load_model(&target) {
                error!(
                    "Failed to restore model '{}' after a CLI job: {}",
                    target, e
                );
            }
        });
    }
}

fn save_history(
    job: &JobContext,
    samples: &[f32],
    raw_text: &str,
    request: &TranscribeRequest,
    post_processed_text: &Option<String>,
    post_process_prompt: &Option<String>,
) -> Option<i64> {
    // save_entry takes a name relative to the recordings directory and expects
    // the audio to be there, so the normalized samples are written first.
    let file_name = format!("handy-cli-{}.wav", chrono::Utc::now().timestamp_millis());
    let path = job.ctx.history.recordings_dir().join(&file_name);
    if let Err(e) = save_wav_file(&path, samples) {
        error!("CLI job could not write its recording to history: {}", e);
        return None;
    }
    if let Err(e) = verify_wav_file(&path, samples.len()) {
        error!("CLI job wrote an unreadable recording: {}", e);
        let _ = std::fs::remove_file(&path);
        return None;
    }
    match job.ctx.history.save_entry(
        file_name,
        raw_text.to_string(),
        request.post_process,
        post_processed_text.clone(),
        post_process_prompt.clone(),
    ) {
        Ok(entry) => Some(entry.id),
        Err(e) => {
            error!("CLI job could not save a history entry: {}", e);
            None
        }
    }
}

/// Paste must happen on the main thread; doing it from a worker misbehaves on
/// macOS. Mirrors the hotkey path, including its `paste-error` event.
fn paste_text(job: &JobContext, text: String) -> bool {
    let app = job.ctx.app.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let dispatched = app.clone().run_on_main_thread(move || {
        let result = match utils::paste(text, app.clone()) {
            Ok(()) => true,
            Err(e) => {
                error!("CLI job failed to paste: {}", e);
                let _ = app.emit("paste-error", ());
                false
            }
        };
        let _ = tx.send(result);
    });
    if dispatched.is_err() {
        error!("CLI job could not reach the main thread to paste");
        return false;
    }
    rx.recv_timeout(Duration::from_secs(30)).unwrap_or(false)
}
