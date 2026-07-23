//! "Проверить канал": короткий активный зонд акустического канала.
//!
//! Идея: пишем ~0.7с фоновый шум без воспроизведения (оценка шумового пола),
//! затем одновременно проигрываем через динамик и пишем с микрофона мульти-тоновый
//! пробный сигнал (~1.5с). Мощность каждого тона на приёме (Гёрцель) относительно
//! мощности шума на той же частоте даёт SNR, по которому выбирается режим модуляции:
//! CSS (самый надёжный) -> OFDM+QPSK -> OFDM+16-QAM -> OFDM+64-QAM (самый быстрый).
//!
//! Это самостоятельная оценка канала, не часть будущего протокола (`plan.md`/`PROTOCOL.md`) —
//! реального модема/FEC здесь нет, только измерение и рекомендация режима.

use cpal::traits::StreamTrait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Блокировка, не паникующая на "отравленном" мьютексе: если паника всё же случится с
/// удержанным логом, следующий вызов не должен уронить всё приложение вторично.
/// (Сам `catch_unwind` вокруг тела аудио-колбэка живёт в `sonic_audio::streams` —
/// паника внутри колбэка на Android разворачивается через C++-границу Oboe, это UB.)
fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) const NOISE_CAPTURE_MS: u64 = 700;
pub(crate) const PROBE_TOTAL_MS: u64 = 1500;
const PROBE_SKIP_MS: u64 = 300;
const PROBE_ANALYZE_MS: u64 = 900;
const MIN_VALID_SAMPLES_MS: u64 = 50;

const TEST_FREQS_HZ: [f32; 8] = [
    700.0, 1500.0, 2500.0, 4000.0, 6000.0, 8000.0, 10000.0, 12500.0,
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TonePoint {
    pub freq_hz: f32,
    pub snr_db: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelReport {
    pub noise_floor_db: f32,
    pub signal_db: f32,
    pub snr_db: f32,
    pub quality_label: String,
    pub recommended_mode: String,
    pub mode_label: String,
    pub estimated_bitrate_bps: u32,
    pub per_tone: Vec<TonePoint>,
}

/// Выбранные в UI устройства; `None`/пусто — системные по умолчанию. Проверка канала и
/// обнаружение обязаны использовать ТЕ ЖЕ устройства, что и сессия, иначе зонд играет
/// не туда, куда слушает пользователь (например, Windows переключил вывод по умолчанию
/// на только что воткнутую гарнитуру).
#[derive(Debug, Clone, Default)]
pub struct AudioSelection {
    pub input: Option<String>,
    pub output: Option<String>,
}

impl AudioSelection {
    pub fn input_name(&self) -> Option<&str> {
        self.input.as_deref().filter(|s| !s.trim().is_empty())
    }
    pub fn output_name(&self) -> Option<&str> {
        self.output.as_deref().filter(|s| !s.trim().is_empty())
    }
}

/// Целевая частота: та же, что у протокола, но если устройство её не умеет —
/// используется его дефолтная (зонд подстраивается под фактическую).
const TARGET_RATE: u32 = 48_000;

pub fn check_channel(selection: &AudioSelection) -> Result<ChannelReport, String> {
    if !crate::android_permissions::ensure_record_audio_permission()? {
        return Err(
            "Доступ к микрофону не разрешён (RECORD_AUDIO). Разрешите доступ к микрофону для этого приложения в настройках Android и повторите проверку."
                .to_string(),
        );
    }

    let (input_device, input_config) =
        sonic_audio::device::open_input(TARGET_RATE, selection.input_name())?;
    let (output_device, output_config) =
        sonic_audio::device::open_output(TARGET_RATE, selection.output_name())?;

    let output_sample_rate = output_config.sample_rate().0 as f32;

    // Фаза 1: фоновый шум (записываем, ничего не проигрывая).
    let (noise_samples, input_sample_rate) =
        capture_audio(NOISE_CAPTURE_MS, selection.input_name())?;
    let min_noise_samples = ((input_sample_rate * MIN_VALID_SAMPLES_MS as f32) / 1000.0) as usize;

    // Фаза 2: мульти-тоновый зонд, играем и пишем одновременно.
    let probe = Arc::new(generate_probe_signal(
        output_sample_rate,
        PROBE_TOTAL_MS,
        &TEST_FREQS_HZ,
    ));
    let position = Arc::new(AtomicUsize::new(0));
    let tone_buf = Arc::new(Mutex::new(Vec::<f32>::new()));
    {
        let in_stream = build_input_stream(&input_device, &input_config, tone_buf.clone())?;
        let out_stream =
            build_output_stream(&output_device, &output_config, probe.clone(), position.clone())?;
        in_stream
            .play()
            .map_err(|e| format!("Не удалось запустить запись: {e}"))?;
        out_stream
            .play()
            .map_err(|e| format!("Не удалось запустить воспроизведение: {e}"))?;
        std::thread::sleep(Duration::from_millis(PROBE_TOTAL_MS + 300));
        drop(in_stream);
        drop(out_stream);
    }
    let tone_samples = lock_recover(&tone_buf).clone();
    if tone_samples.len() < min_noise_samples.max(1) {
        return Err(
            "Микрофон не отдал ни одного сэмпла во время зонда — проверьте разрешение на запись звука"
                .to_string(),
        );
    }

    Ok(analyze(&noise_samples, &tone_samples, input_sample_rate))
}

/// Пишет `duration_ms` с выбранного микрофона (`None` — системный) и возвращает
/// (сэмплы, sample_rate). Используется и локальным самотестом (шумовой пол), и
/// акустическим обнаружением устройств (`discovery.rs`, шумовой пол перед раундом).
pub(crate) fn capture_audio(
    duration_ms: u64,
    input_name: Option<&str>,
) -> Result<(Vec<f32>, f32), String> {
    if !crate::android_permissions::ensure_record_audio_permission()? {
        return Err(
            "Доступ к микрофону не разрешён (RECORD_AUDIO). Разрешите доступ к микрофону для этого приложения в настройках Android и повторите проверку."
                .to_string(),
        );
    }

    let (input_device, input_config) = sonic_audio::device::open_input(TARGET_RATE, input_name)?;
    let sample_rate = input_config.sample_rate().0 as f32;

    let buf = Arc::new(Mutex::new(Vec::<f32>::new()));
    {
        let stream = build_input_stream(&input_device, &input_config, buf.clone())?;
        stream
            .play()
            .map_err(|e| format!("Не удалось запустить запись: {e}"))?;
        std::thread::sleep(Duration::from_millis(duration_ms));
        drop(stream);
    }
    let samples = lock_recover(&buf).clone();
    let min_samples = ((sample_rate * MIN_VALID_SAMPLES_MS as f32) / 1000.0) as usize;
    if samples.len() < min_samples.max(1) {
        return Err(
            "Микрофон не отдал ни одного сэмпла — проверьте разрешение на запись звука"
                .to_string(),
        );
    }
    Ok((samples, sample_rate))
}

fn generate_probe_signal(sample_rate: f32, duration_ms: u64, freqs: &[f32]) -> Vec<f32> {
    let n = ((sample_rate * duration_ms as f32) / 1000.0) as usize;
    let mut buf = vec![0.0f32; n];
    let amp = 0.7 / freqs.len() as f32;
    for (i, sample) in buf.iter_mut().enumerate() {
        let t = i as f32 / sample_rate;
        let mut acc = 0.0f32;
        for &f in freqs {
            acc += (2.0 * std::f32::consts::PI * f * t).sin();
        }
        *sample = acc * amp;
    }
    // Короткий fade-in/out (10мс), чтобы не щёлкать динамиком на границах зонда.
    let fade_len = ((sample_rate * 0.01) as usize).max(1).min(buf.len() / 2);
    for i in 0..fade_len {
        let g = i as f32 / fade_len as f32;
        buf[i] *= g;
        let j = buf.len() - 1 - i;
        buf[j] *= g;
    }
    buf
}

pub(crate) fn goertzel_power(samples: &[f32], sample_rate: f32, freq: f32) -> f32 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let k = (0.5 + (n as f32 * freq) / sample_rate).floor();
    let omega = 2.0 * std::f32::consts::PI * k / n as f32;
    let coeff = 2.0 * omega.cos();
    let mut s1 = 0.0f32;
    let mut s2 = 0.0f32;
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power / (n as f32 * n as f32)
}

pub(crate) fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

fn to_dbfs(value: f32) -> f32 {
    20.0 * value.max(1e-8).log10()
}

pub(crate) fn analyze(noise_samples: &[f32], tone_samples: &[f32], sample_rate: f32) -> ChannelReport {
    let noise_floor_db = to_dbfs(rms(noise_samples));

    let skip = ((sample_rate * PROBE_SKIP_MS as f32) / 1000.0) as usize;
    let window_len = ((sample_rate * PROBE_ANALYZE_MS as f32) / 1000.0) as usize;
    let start = skip.min(tone_samples.len());
    let end = (start + window_len).min(tone_samples.len());
    let steady = &tone_samples[start..end];

    let signal_db = to_dbfs(rms(steady));

    let noise_window_len = window_len.min(noise_samples.len());
    let noise_window = &noise_samples[..noise_window_len];

    let mut per_tone = Vec::with_capacity(TEST_FREQS_HZ.len());
    let mut snr_values = Vec::with_capacity(TEST_FREQS_HZ.len());
    for &freq in TEST_FREQS_HZ.iter() {
        let signal_power = goertzel_power(steady, sample_rate, freq);
        let noise_power = goertzel_power(noise_window, sample_rate, freq).max(1e-12);
        let snr_db = 10.0 * (signal_power / noise_power).max(1e-6).log10();
        per_tone.push(TonePoint {
            freq_hz: freq,
            snr_db,
        });
        snr_values.push(snr_db);
    }
    snr_values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // Медиана устойчивее к одиночному завалу/резонансу на одной частоте, чем среднее.
    let snr_db = snr_values[snr_values.len() / 2];

    let (recommended_mode, mode_label, estimated_bitrate_bps) = select_mode(snr_db);

    ChannelReport {
        noise_floor_db,
        signal_db,
        snr_db,
        quality_label: quality_label(snr_db, noise_floor_db),
        recommended_mode: recommended_mode.to_string(),
        mode_label: mode_label.to_string(),
        estimated_bitrate_bps,
        per_tone,
    }
}

/// Пороги и оценки скорости. CSS SF8/BW6000 ≈ 187 бит/с (после ускорения), MFSK ≈ 214
/// бит/с (тоны, некогерентный приём), OFDM: ~120 поднесущих × бит/символ / 24мс.
pub(crate) fn select_mode(snr_db: f32) -> (&'static str, &'static str, u32) {
    if snr_db < 4.0 {
        ("CSS", "CSS (Chirp Spread Spectrum) — максимальная надёжность", 187)
    } else if snr_db < 9.0 {
        ("MFSK", "MFSK (тоновая манипуляция) — надёжный, устойчив к дрейфу", 214)
    } else if snr_db < 15.0 {
        ("OFDM_QPSK", "OFDM + QPSK — сбалансированный режим", 10_000)
    } else if snr_db < 25.0 {
        ("OFDM_16QAM", "OFDM + 16-QAM — высокая скорость", 20_000)
    } else {
        ("OFDM_64QAM", "OFDM + 64-QAM — максимальная скорость", 30_000)
    }
}

/// Общая для локального самотеста и акустического обнаружения (`discovery.rs`)
/// пятиступенчатая шкала "на глаз" по SNR.
pub(crate) fn clarity_label(snr_db: f32) -> &'static str {
    if snr_db >= 25.0 {
        "Отличная"
    } else if snr_db >= 15.0 {
        "Хорошая"
    } else if snr_db >= 8.0 {
        "Средняя"
    } else if snr_db >= 0.0 {
        "Шумная"
    } else {
        "Очень шумная"
    }
}

fn quality_label(snr_db: f32, noise_floor_db: f32) -> String {
    format!("{} (фон {noise_floor_db:.0} дБФС)", clarity_label(snr_db))
}

/// Поток записи, складывающий моно-сэмплы в общий буфер. Формат сэмплов устройства
/// (F32/I16/U16/U8/…) снимается обобщённо в `sonic_audio::streams` — иначе железо с
/// нестандартным форматом (например, U8) вообще не открывалось.
pub(crate) fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    buffer: Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream, String> {
    sonic_audio::streams::build_input_stream(device, config, move |mono| {
        lock_recover(&buffer).push(mono);
    })
}

/// Поток воспроизведения, проигрывающий заранее подготовленный буфер `probe`
/// (позиция — общий атомарный счётчик); после конца буфера отдаёт тишину.
pub(crate) fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    probe: Arc<Vec<f32>>,
    position: Arc<AtomicUsize>,
) -> Result<cpal::Stream, String> {
    sonic_audio::streams::build_output_stream(device, config, move || {
        let idx = position.fetch_add(1, Ordering::Relaxed);
        probe.get(idx).copied().unwrap_or(0.0)
    })
}
