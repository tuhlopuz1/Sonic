//! Команды Tauri для мессенджера (PROTOCOL.md §11.3): старт/стоп сессии, отправка
//! сообщения, выбор режима, список аудио-устройств. Заменяют demo-`greet`.

use crate::session::{ModePolicy, SessionHandle};
use crate::state::AppState;
use serde::Serialize;
use sonic_protocol::{Profile, Role};
use tauri::State;

fn parse_profile(s: &str) -> Result<Profile, String> {
    match s.to_ascii_lowercase().as_str() {
        "audible" => Ok(Profile::Audible),
        "ultrasonic" => Ok(Profile::Ultrasonic),
        other => Err(format!("Неизвестный профиль: {other}")),
    }
}

fn parse_role(s: &str) -> Result<Role, String> {
    match s.to_ascii_lowercase().as_str() {
        "initiator" | "a" => Ok(Role::Initiator),
        "responder" | "b" => Ok(Role::Responder),
        other => Err(format!("Неизвестная роль: {other}")),
    }
}

fn parse_mode(s: &str) -> Result<ModePolicy, String> {
    match s.to_ascii_lowercase().as_str() {
        "auto" => Ok(ModePolicy::Auto),
        "css" => Ok(ModePolicy::ForceCss),
        "ofdm" => Ok(ModePolicy::ForceOfdm),
        other => Err(format!("Неизвестный режим: {other}")),
    }
}

/// Запускает сессию мессенджера: открывает дуплексный аудио-движок и поток MAC/ARQ.
#[tauri::command]
pub fn start_session(
    app: tauri::AppHandle,
    state: State<AppState>,
    profile: String,
    role: String,
) -> Result<(), String> {
    let profile = parse_profile(&profile)?;
    let role = parse_role(&role)?;

    let mut guard = state.session.lock().map_err(|_| "state poisoned")?;
    if guard.is_some() {
        return Err("Сессия уже запущена".into());
    }
    let handle = SessionHandle::start(app, profile, role)?;
    *guard = Some(handle);
    Ok(())
}

/// Останавливает сессию (роняет аудио-движок и поток сессии).
#[tauri::command]
pub fn stop_session(state: State<AppState>) -> Result<(), String> {
    let mut guard = state.session.lock().map_err(|_| "state poisoned")?;
    *guard = None; // Drop у SessionHandle/DuplexEngine остановит потоки
    Ok(())
}

/// Отправляет текстовое сообщение пиру через акустический канал.
#[tauri::command]
pub fn send_message(state: State<AppState>, text: String) -> Result<(), String> {
    let guard = state.session.lock().map_err(|_| "state poisoned")?;
    match guard.as_ref() {
        Some(session) => session.send_message(text),
        None => Err("Сессия не запущена".into()),
    }
}

/// Меняет политику режима модуляции: auto (fallback) / css / ofdm.
#[tauri::command]
pub fn set_mode(state: State<AppState>, mode: String) -> Result<(), String> {
    let policy = parse_mode(&mode)?;
    let guard = state.session.lock().map_err(|_| "state poisoned")?;
    match guard.as_ref() {
        Some(session) => session.set_mode(policy),
        None => Err("Сессия не запущена".into()),
    }
}

#[derive(Serialize)]
pub struct AudioDevices {
    inputs: Vec<String>,
    outputs: Vec<String>,
    default_input: Option<String>,
    default_output: Option<String>,
}

/// Список аудио-устройств для UI.
#[tauri::command]
pub fn list_audio_devices() -> Result<AudioDevices, String> {
    let list = sonic_audio::list_devices()?;
    Ok(AudioDevices {
        inputs: list.inputs,
        outputs: list.outputs,
        default_input: list.default_input,
        default_output: list.default_output,
    })
}
