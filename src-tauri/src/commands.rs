//! Tauri commands — the IPC surface described in `docs/IPC.md`.

use crate::db::Db;
use crate::models::{
    events, AudioDevices, Meeting, MeetingDetail, MeetingListItem, ModelInfo, RecordingStatus,
    SecondBrainStatus, Settings,
};
use crate::recorder::Recorder;
use crate::settings::{self as secrets, SettingsStore};
use crate::transcribe::models as whisper_models;
use crate::{audio, export, summarize};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

pub struct AppState {
    pub db: Arc<Db>,
    pub settings: Arc<SettingsStore>,
    pub recorder: Arc<Recorder>,
    pub data_dir: PathBuf,
}

type CmdResult<T> = Result<T, String>;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings.get()
}

#[tauri::command]
pub fn save_settings(state: State<'_, AppState>, settings: Settings) -> CmdResult<Settings> {
    state.settings.save(settings).map_err(err)
}

#[tauri::command]
pub fn set_secret(name: String, value: String) -> CmdResult<()> {
    secrets::set_secret(&name, &value).map_err(err)
}

#[tauri::command]
pub fn has_secret(name: String) -> bool {
    secrets::has_secret(&name)
}

#[tauri::command]
pub fn list_audio_devices() -> AudioDevices {
    audio::list_devices()
}

#[tauri::command]
pub fn start_recording(
    app: AppHandle,
    state: State<'_, AppState>,
    title: Option<String>,
) -> CmdResult<Meeting> {
    state
        .recorder
        .start(
            app,
            state.db.clone(),
            state.settings.get(),
            state.data_dir.clone(),
            title,
        )
        .map_err(err)
}

#[tauri::command]
pub fn stop_recording(app: AppHandle, state: State<'_, AppState>) -> CmdResult<Meeting> {
    state.recorder.stop(app, state.db.clone()).map_err(err)
}

#[tauri::command]
pub fn recording_status(state: State<'_, AppState>) -> RecordingStatus {
    state.recorder.status()
}

#[tauri::command]
pub fn list_meetings(
    state: State<'_, AppState>,
    query: Option<String>,
) -> CmdResult<Vec<MeetingListItem>> {
    let q = query.as_deref().map(str::trim).filter(|q| !q.is_empty());
    state.db.list_meetings(q).map_err(err)
}

#[tauri::command]
pub fn get_meeting(state: State<'_, AppState>, id: String) -> CmdResult<MeetingDetail> {
    let meeting = state.db.require_meeting(&id).map_err(err)?;
    let segments = state.db.segments_for(&id).map_err(err)?;
    Ok(MeetingDetail { meeting, segments })
}

#[tauri::command]
pub fn rename_meeting(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> CmdResult<Meeting> {
    let title = title.trim();
    if title.is_empty() {
        return Err("title cannot be empty".into());
    }
    state.db.rename_meeting(&id, title).map_err(err)?;
    let m = state.db.require_meeting(&id).map_err(err)?;
    let _ = app.emit(events::MEETING_UPDATED, m.clone());
    Ok(m)
}

#[tauri::command]
pub fn delete_meeting(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let status = state.recorder.status();
    if status.active && status.meeting_id.as_deref() == Some(id.as_str()) {
        return Err("stop the recording before deleting this meeting".into());
    }
    state.db.require_meeting(&id).map_err(err)?;
    state.db.delete_meeting(&id).map_err(err)?;
    // Never trust the stored audio_path: derive the file from the meeting id.
    // Ids that aren't UUIDs (e.g. a tampered DB) have no audio file to delete.
    match uuid::Uuid::parse_str(&id) {
        Ok(uuid) => {
            let path = crate::recorder::recording_path(&state.data_dir, &uuid);
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("removing {}: {e}", path.display());
                }
            }
        }
        Err(_) => log::warn!("meeting id {id:?} is not a UUID; not deleting any audio file"),
    }
    Ok(())
}

#[tauri::command]
pub async fn summarize_meeting(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> CmdResult<Meeting> {
    let meeting = state.db.require_meeting(&id).map_err(err)?;
    let segments = state.db.segments_for(&id).map_err(err)?;
    let api_key = secrets::require_secret(secrets::SECRET_ANTHROPIC, "Anthropic API key").map_err(err)?;
    let model = state.settings.get().claude_model;
    let summary = summarize::summarize(&api_key, &model, &meeting, &segments)
        .await
        .map_err(err)?;
    state
        .db
        .set_summary(&id, &summary, &chrono::Utc::now().to_rfc3339())
        .map_err(err)?;
    let m = state.db.require_meeting(&id).map_err(err)?;
    let _ = app.emit(events::MEETING_UPDATED, m.clone());
    // Push the new summary to SecondBrain if this meeting is (to be) exported.
    export::spawn_auto_export(&app, &id, export::Trigger::Summarized);
    Ok(m)
}

#[tauri::command]
pub async fn export_meeting(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> CmdResult<Meeting> {
    export::export_now(&app, state.db.clone(), state.settings.clone(), &id)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn secondbrain_status(state: State<'_, AppState>) -> CmdResult<SecondBrainStatus> {
    Ok(export::status(&state.settings.get()).await)
}

#[tauri::command]
pub fn list_local_models(state: State<'_, AppState>) -> Vec<ModelInfo> {
    whisper_models::list(&state.data_dir)
}

#[tauri::command]
pub async fn download_model(
    app: AppHandle,
    state: State<'_, AppState>,
    name: String,
) -> CmdResult<ModelInfo> {
    whisper_models::download(&app, &state.data_dir, &name)
        .await
        .map_err(err)
}

#[tauri::command]
pub fn delete_model(state: State<'_, AppState>, name: String) -> CmdResult<()> {
    whisper_models::delete(&state.data_dir, &name).map_err(err)
}

#[tauri::command]
pub fn open_data_dir(state: State<'_, AppState>) -> CmdResult<()> {
    tauri_plugin_opener::open_path(&state.data_dir, None::<&str>).map_err(|e| e.to_string())
}
