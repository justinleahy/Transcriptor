pub mod audio;
mod commands;
pub mod db;
pub mod fs_perms;
pub mod models;
pub mod net;
pub mod recorder;
pub mod settings;
pub mod summarize;
pub mod transcribe;

use commands::AppState;
use std::sync::Arc;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,transcriptor_lib=debug"))
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let config_dir = app.path().app_config_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            std::fs::create_dir_all(&config_dir)?;
            fs_perms::harden_dir(&data_dir);
            fs_perms::harden_dir(&config_dir);
            log::info!("data dir: {}", data_dir.display());

            let db = Arc::new(db::Db::open(&data_dir.join("transcriptor.sqlite"))?);
            let settings = Arc::new(settings::SettingsStore::load(&config_dir)?);
            app.manage(AppState {
                db,
                settings,
                recorder: Arc::new(recorder::Recorder::default()),
                data_dir,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::save_settings,
            commands::set_secret,
            commands::has_secret,
            commands::list_audio_devices,
            commands::start_recording,
            commands::stop_recording,
            commands::recording_status,
            commands::list_meetings,
            commands::get_meeting,
            commands::rename_meeting,
            commands::delete_meeting,
            commands::summarize_meeting,
            commands::list_local_models,
            commands::download_model,
            commands::delete_model,
            commands::open_data_dir,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                let state = app.state::<AppState>();
                recorder::shutdown(&state.recorder, app, &state.db);
            }
        });
}
