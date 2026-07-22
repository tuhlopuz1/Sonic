mod acoustic_beacon;
mod android_permissions;
mod audio_watch;
mod channel_check;
mod commands;
mod discovery;
mod events;
mod session;
mod state;

use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .setup(|app| {
            // Фоновое слежение за hot-plug аудио-устройств → событие в UI.
            audio_watch::spawn(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // Мессенджер (session/MAC поверх PHY)
            commands::start_session,
            commands::stop_session,
            commands::send_message,
            commands::set_mode,
            commands::list_audio_devices,
            // Самопроверка канала и акустическое обнаружение устройств
            commands::check_channel,
            commands::discover_devices,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
