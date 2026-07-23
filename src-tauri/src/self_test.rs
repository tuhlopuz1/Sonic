//! Loopback-самотест модема на ОДНОМ устройстве: проигрывает через динамик реальный
//! кадр выбранного режима и одновременно пишет его микрофоном, затем пытается
//! декодировать. Отвечает на ключевой вопрос отладки: работает ли DSP-тракт через
//! настоящий звук вообще — или проблема только в связке из двух устройств
//! (роли/громкость/расстояние).
//!
//! В отличие от сессии, здесь TX и RX в ОДНОЙ полосе (устройство должно услышать само
//! себя), поэтому модем строится напрямую на нижней полосе профиля, без FDD-разделения.

use crate::channel_check::AudioSelection;
use cpal::traits::StreamTrait;
use serde::Serialize;
use sonic_audio::resample::Resampler;
use sonic_audio::streams::{build_input_stream, build_output_stream};
use sonic_protocol::bandplan::Profile;
use sonic_protocol::framing::{Frame, FrameHeader, FrameType, PhyMode};
use sonic_protocol::modem::qam::Modulation;
use sonic_protocol::modem::{CssModem, MfskModem, Modem, OfdmModem};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DSP_RATE: u32 = 48_000;

#[derive(Serialize, Clone)]
pub struct SelfTestReport {
    pub mode: String,
    /// Кадр вообще пойман и синхронизирован.
    pub detected: bool,
    /// Декодирован без ошибок (байты совпали с отправленными).
    pub matched: bool,
    pub snr_db: f32,
    /// Пиковый и RMS уровень захваченного сигнала — сразу видно, слышит ли микрофон.
    pub captured_peak: f32,
    pub captured_rms: f32,
    /// Человекочитаемый диагноз с рекомендацией.
    pub verdict: String,
}

fn build_modem(mode: PhyMode, band: sonic_protocol::bandplan::SubBand) -> Box<dyn Modem> {
    match mode {
        PhyMode::Css => Box::new(CssModem::with_defaults(band, DSP_RATE)),
        PhyMode::Mfsk => Box::new(MfskModem::new(band, DSP_RATE)),
        PhyMode::OfdmQpsk => Box::new(OfdmModem::new(band, DSP_RATE, Modulation::Qpsk)),
        PhyMode::Ofdm16Qam => Box::new(OfdmModem::new(band, DSP_RATE, Modulation::Qam16)),
    }
}

pub fn run(mode: PhyMode, selection: &AudioSelection) -> Result<SelfTestReport, String> {
    if !crate::android_permissions::ensure_record_audio_permission()? {
        return Err("Нет доступа к микрофону (RECORD_AUDIO).".into());
    }

    let (out_device, out_config) =
        sonic_audio::device::open_output(DSP_RATE, selection.output_name())?;
    let (in_device, in_config) =
        sonic_audio::device::open_input(DSP_RATE, selection.input_name())?;
    let out_rate = out_config.sample_rate().0;
    let in_rate = in_config.sample_rate().0;

    // Нижняя полоса профиля — TX и RX в ней же (само-приём).
    let band = Profile::Audible.band_plan().lower;
    let modem = build_modem(mode, band);

    // Тестовый кадр. Payload короткий: у медленных режимов (CSS/MFSK) длина кадра прямо
    // определяет длительность самотеста, а для проверки тракта хватает пары слов.
    let payload = b"SONIC self-test".to_vec();
    let frame = Frame::new(FrameHeader::new(mode, FrameType::Data, 0), payload);
    let frame_bytes = frame.serialize();

    // Модулируем на каноничной частоте, ресемплим в частоту динамика, добавляем
    // тишину в начале (шумовой пол + захват начала) и в конце.
    let sig48 = modem.modulate(&frame_bytes);
    let mut play48 = vec![0.0f32; (DSP_RATE as usize) / 3]; // ~0.33 c тишины перед
    play48.extend_from_slice(&sig48);
    play48.extend(std::iter::repeat(0.0).take((DSP_RATE as usize) / 3));
    let play = if out_rate == DSP_RATE {
        play48
    } else {
        Resampler::new(DSP_RATE, out_rate).process_all(&play48)
    };

    let play = Arc::new(play);
    let position = Arc::new(AtomicUsize::new(0));
    let record = Arc::new(Mutex::new(Vec::<f32>::new()));

    let play_ms = (play.len() as u64 * 1000) / out_rate as u64;
    {
        let rec_cb = record.clone();
        let in_stream = build_input_stream(&in_device, &in_config, move |mono| {
            rec_cb.lock().unwrap_or_else(|p| p.into_inner()).push(mono);
        })?;
        let play_cb = play.clone();
        let pos_cb = position.clone();
        let out_stream = build_output_stream(&out_device, &out_config, move || {
            let i = pos_cb.fetch_add(1, Ordering::Relaxed);
            play_cb.get(i).copied().unwrap_or(0.0)
        })?;
        in_stream.play().map_err(|e| format!("запись: {e}"))?;
        out_stream.play().map_err(|e| format!("воспроизведение: {e}"))?;
        std::thread::sleep(Duration::from_millis(play_ms + 400));
        drop(in_stream);
        drop(out_stream);
    }

    let captured = record.lock().unwrap_or_else(|p| p.into_inner()).clone();
    if captured.is_empty() {
        return Err("Микрофон не отдал ни одного сэмпла.".into());
    }

    let captured_peak = captured.iter().fold(0.0f32, |a, &x| a.max(x.abs()));
    let captured_rms =
        (captured.iter().map(|x| x * x).sum::<f32>() / captured.len() as f32).sqrt();

    // В каноничную частоту и демодуляция.
    let cap48 = if in_rate == DSP_RATE {
        captured
    } else {
        Resampler::new(in_rate, DSP_RATE).process_all(&captured)
    };
    let demod = modem.demodulate(&cap48);

    let (detected, matched, snr_db) = match &demod {
        Some(d) => (true, d.bytes == frame_bytes, d.snr_db),
        None => (false, false, 0.0),
    };

    let verdict = verdict(detected, matched, captured_peak, snr_db);
    eprintln!(
        "self_test[{mode:?}]: peak={captured_peak:.3} rms={captured_rms:.4} detected={detected} matched={matched} snr={snr_db:.1}dB"
    );

    Ok(SelfTestReport {
        mode: mode_label(mode),
        detected,
        matched,
        snr_db,
        captured_peak,
        captured_rms,
        verdict,
    })
}

fn verdict(detected: bool, matched: bool, peak: f32, snr_db: f32) -> String {
    if matched {
        return format!("✓ Модем работает через реальный звук (SNR ~{snr_db:.0} дБ).");
    }
    if peak < 0.01 {
        return "Микрофон почти не слышит сигнал. Прибавьте громкость динамика, \
                выберите правильные устройства и не заглушайте микрофон."
            .into();
    }
    if detected {
        return format!(
            "Сигнал пойман, но декодирован с ошибками (SNR ~{snr_db:.0} дБ). \
             Слишком шумно/тихо или сильные искажения — прибавьте громкость или уберите фоновый шум."
        );
    }
    "Сигнал в микрофон приходит, но синхронизация не поймалась. Вероятно, слишком тихо, \
     либо динамик клиппит на большой громкости. Попробуйте среднюю громкость."
        .into()
}

fn mode_label(mode: PhyMode) -> String {
    mode.label().into()
}
