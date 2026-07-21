//! Обнаружение устройств поблизости чисто через звук: каждое устройство поочерёдно
//! слушает и в свой (детерминированный по никнейму) момент внутри раунда проигрывает
//! акустический маячок со своим никнеймом (`acoustic_beacon.rs`). Никакой сети —
//! в отличие от более ранней версии этой фичи на TCP, координация тоже идёт по воздуху,
//! иначе теряется весь смысл "передать данные, когда обычной связи нет" (`task.md`).
//!
//! За несколько раундов увеличивается шанс, что чужой маячок не попадёт точно на край
//! окна записи и будет декодирован целиком хотя бы раз.

use crate::acoustic_beacon;
use crate::channel_check;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

const ROUNDS: u32 = 5;
const ROUND_MS: u64 = 4000;
const PRE_ROUND_NOISE_MS: u64 = 300;

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredDevice {
    pub nickname: String,
    pub snr_db: f32,
    pub quality_label: String,
    pub recommended_mode: String,
    pub mode_label: String,
    pub estimated_bitrate_bps: u32,
    pub round: u32,
}

pub fn discover(app: AppHandle, nickname: String) -> Result<(), String> {
    if !crate::android_permissions::ensure_record_audio_permission()? {
        return Err(
            "Доступ к микрофону не разрешён (RECORD_AUDIO). Разрешите доступ к микрофону для этого приложения в настройках Android и повторите поиск."
                .to_string(),
        );
    }
    let nickname = nickname.trim().to_string();
    if nickname.is_empty() {
        return Err("Введите никнейм устройства".to_string());
    }

    std::thread::spawn(move || {
        for round in 0..ROUNDS {
            match run_round(&nickname, round) {
                Ok(devices) => {
                    for device in devices {
                        let _ = app.emit("device-discovered", device);
                    }
                }
                Err(err) => {
                    let _ = app.emit("discovery-error", err);
                }
            }
        }
        let _ = app.emit("discovery-finished", ());
    });

    Ok(())
}

fn run_round(nickname: &str, round: u32) -> Result<Vec<DiscoveredDevice>, String> {
    let host = cpal::default_host();
    let input_device = host
        .default_input_device()
        .ok_or_else(|| "Микрофон не найден".to_string())?;
    let output_device = host
        .default_output_device()
        .ok_or_else(|| "Динамик не найден".to_string())?;
    let input_config = input_device
        .default_input_config()
        .map_err(|e| format!("Не удалось получить конфигурацию микрофона: {e}"))?;
    let output_config = output_device
        .default_output_config()
        .map_err(|e| format!("Не удалось получить конфигурацию динамика: {e}"))?;
    let output_sample_rate = output_config.sample_rate().0 as f32;

    // Короткая тишина перед раундом — опорный шумовой пол для SNR декодированных маячков.
    let (noise_samples, input_sample_rate) = channel_check::capture_audio(PRE_ROUND_NOISE_MS)?;
    let noise_floor_rms = channel_check::rms(&noise_samples);

    // Момент внутри раунда, когда МЫ проигрываем свой маячок, зависит только от своего
    // никнейма и номера раунда — без какой-либо координации с другими устройствами.
    let beacon_ms = acoustic_beacon::beacon_duration_ms();
    let max_offset = ROUND_MS.saturating_sub(beacon_ms + PRE_ROUND_NOISE_MS);
    let my_offset_ms = if max_offset == 0 {
        0
    } else {
        hash_str(&format!("{nickname}-{round}")) % max_offset
    };

    let beacon_signal = acoustic_beacon::generate_beacon_signal(output_sample_rate, nickname);
    let round_total_n = ((output_sample_rate * ROUND_MS as f32) / 1000.0) as usize;
    let silence_lead_n = ((output_sample_rate * my_offset_ms as f32) / 1000.0) as usize;
    let mut play_buf = vec![0.0f32; round_total_n];
    if silence_lead_n < play_buf.len() {
        let end = (silence_lead_n + beacon_signal.len()).min(play_buf.len());
        play_buf[silence_lead_n..end].copy_from_slice(&beacon_signal[..end - silence_lead_n]);
    }

    let record_buf = Arc::new(Mutex::new(Vec::<f32>::new()));
    let in_stream = channel_check::build_input_stream(&input_device, &input_config, record_buf.clone())?;
    let out_stream = channel_check::build_output_stream(
        &output_device,
        &output_config,
        Arc::new(play_buf),
        Arc::new(AtomicUsize::new(0)),
    )?;
    in_stream
        .play()
        .map_err(|e| format!("Не удалось запустить запись: {e}"))?;
    out_stream
        .play()
        .map_err(|e| format!("Не удалось запустить воспроизведение: {e}"))?;
    std::thread::sleep(Duration::from_millis(ROUND_MS));
    drop(in_stream);
    drop(out_stream);

    let samples = record_buf.lock().unwrap().clone();
    let decoded = acoustic_beacon::decode_beacons_from_buffer(&samples, input_sample_rate, noise_floor_rms);
    let my_canonical = acoustic_beacon::canonicalize(nickname);

    Ok(decoded
        .into_iter()
        .filter(|d| d.nickname != my_canonical)
        .map(|d| {
            let (mode, mode_label, bps) = channel_check::select_mode(d.snr_db);
            DiscoveredDevice {
                nickname: d.nickname,
                snr_db: d.snr_db,
                quality_label: channel_check::clarity_label(d.snr_db).to_string(),
                recommended_mode: mode.to_string(),
                mode_label: mode_label.to_string(),
                estimated_bitrate_bps: bps,
                round,
            }
        })
        .collect())
}

/// FNV-1a — детерминированное распределение момента своего маячка внутри раунда,
/// без дополнительных крейтов.
fn hash_str(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
