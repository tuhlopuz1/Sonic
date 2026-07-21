mod acoustic_beacon;
mod android_permissions;
mod channel_check;
mod discovery;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

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
        .invoke_handler(tauri::generate_handler![greet, check_channel, discover_devices])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
