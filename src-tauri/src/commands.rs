//! Команды Tauri для мессенджера (PROTOCOL.md §11.3): старт/стоп сессии, отправка
//! сообщения, выбор режима, список аудио-устройств. Заменяют demo-`greet`.

use crate::channel_check::AudioSelection;
use crate::session::{ModePolicy, SessionHandle};
use crate::state::AppState;
use sonic_protocol::framing::PhyMode;
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
        "css" => Ok(ModePolicy::Force(PhyMode::Css)),
        "mfsk" => Ok(ModePolicy::Force(PhyMode::Mfsk)),
        "ofdm" | "ofdm-qpsk" | "qpsk" => Ok(ModePolicy::Force(PhyMode::OfdmQpsk)),
        "ofdm-qam" | "ofdm-16qam" | "16qam" | "qam" => Ok(ModePolicy::Force(PhyMode::Ofdm16Qam)),
        other => Err(format!("Неизвестный режим: {other}")),
    }
}

/// Запускает сессию мессенджера: открывает дуплексный аудио-движок и поток MAC/ARQ.
/// `input_device`/`output_device` — имена устройств из `list_audio_devices`;
/// пусто/отсутствует — системные по умолчанию.
#[tauri::command]
pub fn start_session(
    app: tauri::AppHandle,
    state: State<AppState>,
    profile: String,
    role: String,
    input_device: Option<String>,
    output_device: Option<String>,
) -> Result<(), String> {
    let profile = parse_profile(&profile)?;
    let role = parse_role(&role)?;
    let clean = |s: Option<String>| s.filter(|v| !v.trim().is_empty());

    let mut guard = state.session.lock().map_err(|_| "state poisoned")?;
    if guard.is_some() {
        return Err("Сессия уже запущена".into());
    }
    let handle = SessionHandle::start(
        app,
        profile,
        role,
        clean(input_device),
        clean(output_device),
    )?;
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

/// Список аудио-устройств для UI (первичное заполнение; дальше UI обновляется
/// событием `audio-devices-changed` от `audio_watch`).
#[tauri::command]
pub fn list_audio_devices() -> Result<crate::audio_watch::AudioDevices, String> {
    crate::audio_watch::snapshot()
}

fn selection(input_device: Option<String>, output_device: Option<String>) -> AudioSelection {
    AudioSelection {
        input: input_device,
        output: output_device,
    }
}

/// Активный зонд канала на ВЫБРАННЫХ устройствах (не на системных по умолчанию —
/// иначе зонд играет не туда, куда слушает пользователь).
#[tauri::command]
pub fn check_channel(
    input_device: Option<String>,
    output_device: Option<String>,
) -> Result<crate::channel_check::ChannelReport, String> {
    crate::channel_check::check_channel(&selection(input_device, output_device))
}

/// Loopback-самотест модема на одном устройстве: играет кадр в динамик, пишет
/// микрофоном, пробует декодировать. Показывает, работает ли DSP через реальный звук.
#[tauri::command]
pub fn modem_self_test(
    mode: String,
    input_device: Option<String>,
    output_device: Option<String>,
) -> Result<crate::self_test::SelfTestReport, String> {
    let mode = match mode.to_ascii_lowercase().as_str() {
        "css" => PhyMode::Css,
        "mfsk" => PhyMode::Mfsk,
        "ofdm" | "ofdm-qpsk" | "qpsk" => PhyMode::OfdmQpsk,
        "ofdm-qam" | "ofdm-16qam" | "16qam" | "qam" => PhyMode::Ofdm16Qam,
        other => return Err(format!("Неизвестный режим: {other}")),
    };
    crate::self_test::run(mode, &selection(input_device, output_device))
}

/// Акустическое обнаружение устройств поблизости — тоже на выбранных устройствах.
#[tauri::command]
pub fn discover_devices(
    app: tauri::AppHandle,
    nickname: String,
    input_device: Option<String>,
    output_device: Option<String>,
) -> Result<(), String> {
    crate::discovery::discover(app, nickname, selection(input_device, output_device))
}
