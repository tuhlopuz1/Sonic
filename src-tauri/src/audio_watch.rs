//! Слежение за подключением/отключением аудио-устройств (hot-plug).
//!
//! cpal не даёт кроссплатформенных уведомлений о появлении/пропаже устройств (это
//! потребовало бы `IMMNotificationClient` на WASAPI, property listener на CoreAudio и
//! т.д.), поэтому опрашиваем список сами в фоновом потоке. Перечисление дешёвое, а
//! событие в UI эмитится ТОЛЬКО когда список реально изменился — лишних перерисовок нет.

use crate::events;
use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

const POLL_INTERVAL: Duration = Duration::from_millis(2000);

/// Снимок доступных устройств (он же — полезная нагрузка события и ответ команды).
#[derive(Serialize, Clone, PartialEq, Eq)]
pub struct AudioDevices {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

pub fn snapshot() -> Result<AudioDevices, String> {
    let list = sonic_audio::list_devices()?;
    Ok(AudioDevices {
        inputs: list.inputs,
        outputs: list.outputs,
        default_input: list.default_input,
        default_output: list.default_output,
    })
}

/// Запускает фоновый опрос. Вызывается один раз при старте приложения.
pub fn spawn(app: AppHandle) {
    let _ = std::thread::Builder::new()
        .name("sonic-audio-watch".into())
        .spawn(move || {
            let mut prev: Option<AudioDevices> = None;
            loop {
                match snapshot() {
                    Ok(current) => {
                        // Сравниваем со снимком: устройство воткнули/выдернули или
                        // сменилось системное по умолчанию.
                        if prev.as_ref() != Some(&current) {
                            // Первый снимок — не «изменение», а стартовое состояние.
                            if prev.is_some() {
                                eprintln!(
                                    "audio_watch: список устройств изменился — микрофонов {}, динамиков {}",
                                    current.inputs.len(),
                                    current.outputs.len()
                                );
                            }
                            let _ = app.emit(events::AUDIO_DEVICES_CHANGED, current.clone());
                            prev = Some(current);
                        }
                    }
                    Err(e) => eprintln!("audio_watch: не удалось перечислить устройства: {e}"),
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        });
}
