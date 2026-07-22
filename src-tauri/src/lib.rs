mod acoustic_beacon;
mod android_permissions;
mod channel_check;
mod commands;
mod discovery;
mod events;
mod session;
mod state;

use state::AppState;

#[tauri::command]
fn check_channel() -> Result<channel_check::ChannelReport, String> {
    channel_check::check_channel()
}

#[tauri::command]
fn discover_devices(app: tauri::AppHandle, nickname: String) -> Result<(), String> {
    discovery::discover(app, nickname)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            // Мессенджер (session/MAC поверх PHY)
            commands::start_session,
            commands::stop_session,
            commands::send_message,
            commands::set_mode,
            commands::list_audio_devices,
            // Самопроверка канала и акустическое обнаружение устройств
            check_channel,
            discover_devices,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
