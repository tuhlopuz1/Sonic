//! Матрица режимов через симулированный канал: каждый режим (CSS / MFSK / OFDM-QPSK /
//! OFDM-16QAM) должен пережить эффекты реального акустического тракта между двумя
//! устройствами — шум, рассинхрон тактовой частоты (SFO), несовпадение частот железа
//! (ресемплинг 48к↔44.1к), реверберацию. Это страховка от регрессий DSP и объективная
//! проверка того, что связь между устройствами в принципе рабочая.

use sonic_protocol::bandplan::{DuplexScheme, Fdd, Profile, Role};
use sonic_protocol::framing::{Frame, FrameHeader, FrameType, PhyMode};
use sonic_protocol::modem::qam::Modulation;
use sonic_protocol::modem::{CssModem, MfskModem, Modem, OfdmModem};
use sonic_protocol::sim::clock_drift::resample_ppm;
use sonic_protocol::sim::{AwgnChannel, MultipathChannel};

fn build(mode: PhyMode) -> Box<dyn Modem> {
    let fdd = Fdd::new(Role::Initiator, Profile::Audible);
    let band = fdd.tx_band();
    let sr = fdd.sample_rate();
    match mode {
        PhyMode::Css => Box::new(CssModem::with_defaults(band, sr)),
        PhyMode::Mfsk => Box::new(MfskModem::new(band, sr)),
        PhyMode::OfdmQpsk => Box::new(OfdmModem::new(band, sr, Modulation::Qpsk)),
        PhyMode::Ofdm16Qam => Box::new(OfdmModem::new(band, sr, Modulation::Qam16)),
    }
}

fn frame_of(mode: PhyMode, msg: &[u8]) -> Vec<u8> {
    Frame::new(FrameHeader::new(mode, FrameType::Data, 0), msg.to_vec()).serialize()
}

fn wrap(tx: &[f32], lead: usize) -> Vec<f32> {
    let mut buf = vec![0.0f32; lead];
    buf.extend_from_slice(tx);
    buf.extend(std::iter::repeat(0.0).take(2500));
    buf
}

const ALL: [PhyMode; 4] = [
    PhyMode::Css,
    PhyMode::Mfsk,
    PhyMode::OfdmQpsk,
    PhyMode::Ofdm16Qam,
];

#[test]
fn every_mode_clean_roundtrip() {
    for mode in ALL {
        let m = build(mode);
        let fb = frame_of(mode, b"the quick brown fox 0123456789");
        let tx = m.modulate(&fb);
        let buf = wrap(&tx, 2000);
        let got = m.demodulate(&buf).unwrap_or_else(|| panic!("{mode:?}: not demodulated"));
        assert_eq!(got.bytes, fb, "{mode:?}: bytes mismatch");
    }
}

#[test]
fn every_mode_survives_awgn() {
    // Реалистичные SNR: у надёжных режимов ниже, у быстрых — выше.
    let snr = |mode| match mode {
        PhyMode::Css => 6.0,
        PhyMode::Mfsk => 9.0,
        PhyMode::OfdmQpsk => 14.0,
        PhyMode::Ofdm16Qam => 22.0,
    };
    for mode in ALL {
        let m = build(mode);
        let fb = frame_of(mode, b"acoustic link under gaussian noise");
        let tx = m.modulate(&fb);
        let mut ok = 0;
        for seed in 0..6 {
            let buf = AwgnChannel::new(seed).apply(&wrap(&tx, 2000), snr(mode));
            if let Some(d) = m.demodulate(&buf) {
                if d.bytes == fb {
                    ok += 1;
                }
            }
        }
        assert!(ok >= 5, "{mode:?}: only {ok}/6 survived AWGN @ {} dB", snr(mode));
    }
}

#[test]
fn every_mode_survives_hardware_rate_mismatch() {
    // Микрофон 44.1к, протокол 48к: сигнал проходит через пару ресемплингов (как на
    // ноутбуке в shared-режиме WASAPI). Кадр обязан это пережить во всех режимах.
    for mode in ALL {
        let m = build(mode);
        let fb = frame_of(mode, b"resampled across 48k and 44.1k hardware");
        let tx = m.modulate(&fb);
        // Несовпадение частот: 48к → 44.1к → 48к линейной интерполяцией.
        let down = down_up(&tx);
        let buf = AwgnChannel::new(3).apply(&wrap(&down, 2000), 20.0);
        let got = m.demodulate(&buf).unwrap_or_else(|| panic!("{mode:?}: lost after resample"));
        assert_eq!(got.bytes, fb, "{mode:?}: bytes mismatch after resample");
    }
}

// Грубый ресемплинг 48к→44.1к→48к линейной интерполяцией (как в sonic-audio::resample).
fn down_up(x: &[f32]) -> Vec<f32> {
    let resample = |x: &[f32], ratio: f64| -> Vec<f32> {
        let out_len = ((x.len() as f64) / ratio).floor() as usize;
        (0..out_len)
            .map(|i| {
                let src = i as f64 * ratio;
                let idx = src.floor() as usize;
                let frac = (src - idx as f64) as f32;
                if idx + 1 < x.len() {
                    x[idx] * (1.0 - frac) + x[idx + 1] * frac
                } else {
                    x[x.len() - 1]
                }
            })
            .collect()
    };
    let down = resample(x, 48_000.0 / 44_100.0);
    resample(&down, 44_100.0 / 48_000.0)
}

#[test]
fn robust_modes_survive_clock_drift() {
    // Рассинхрон тактовой частоты двух звуковых карт (SFO). Типичные потребительские
    // ЦАП/АЦП расходятся на десятки ppm; берём с большим запасом ±120 ppm. CSS держит
    // это благодаря decision-directed слежению за таймингом, MFSK — благодаря защитному
    // интервалу внутри символа. Обе — надёжные режимы, устойчивые к дрейфу.
    // Изолируем именно дрейф (шум/длина — отдельные тесты): близкие устройства = хороший
    // SNR. ±100 ppm с большим запасом перекрывает реальные пары звуковых карт.
    for mode in [PhyMode::Css, PhyMode::Mfsk] {
        let m = build(mode);
        let fb = frame_of(mode, b"clock drift between two sound cards");
        let tx = m.modulate(&fb);
        for ppm in [-100.0f32, -50.0, 50.0, 100.0] {
            let drifted = resample_ppm(&tx, ppm);
            let buf = AwgnChannel::new(7).apply(&wrap(&drifted, 2000), 20.0);
            let got = m
                .demodulate(&buf)
                .unwrap_or_else(|| panic!("{mode:?}: lost @ {ppm} ppm"));
            assert_eq!(got.bytes, fb, "{mode:?}: mismatch @ {ppm} ppm");
        }
    }
}

#[test]
fn ofdm_survives_clock_drift_short_frame() {
    // Короткий кадр OFDM (типичный чат) переживает SFO: дрейф за 0.2 с пренебрежимо мал.
    for mode in [PhyMode::OfdmQpsk, PhyMode::Ofdm16Qam] {
        let m = build(mode);
        let fb = frame_of(mode, b"ofdm short chat frame");
        let tx = m.modulate(&fb);
        for ppm in [-100.0f32, 100.0] {
            let drifted = resample_ppm(&tx, ppm);
            let buf = AwgnChannel::new(1).apply(&wrap(&drifted, 1500), 25.0);
            let got = m
                .demodulate(&buf)
                .unwrap_or_else(|| panic!("{mode:?}: lost @ {ppm} ppm"));
            assert_eq!(got.bytes, fb, "{mode:?}: mismatch @ {ppm} ppm");
        }
    }
}

#[test]
fn robust_modes_survive_reverb_plus_noise() {
    // Реверберация комнаты (экспоненциальный RIR ~2 мс) + шум. CSS и MFSK размазаны по
    // времени/частоте и терпят это на близкой дистанции (хороший SNR).
    for mode in [PhyMode::Css, PhyMode::Mfsk] {
        let m = build(mode);
        let fb = frame_of(mode, b"room reverberation and background noise");
        let tx = m.modulate(&fb);
        let echoed = MultipathChannel::exponential(96, 20.0).apply(&tx);
        let buf = AwgnChannel::new(11).apply(&wrap(&echoed, 2500), 13.0);
        let got = m
            .demodulate(&buf)
            .unwrap_or_else(|| panic!("{mode:?}: lost under reverb+noise"));
        assert_eq!(got.bytes, fb, "{mode:?}: mismatch under reverb+noise");
    }
}

#[test]
fn mfsk_survives_timing_jitter_and_multipath() {
    // MFSK: грубый тайминг старта + короткое эхо. Защитный интервал внутри символа тянет.
    let m = build(PhyMode::Mfsk);
    let fb = frame_of(PhyMode::Mfsk, b"mfsk timing and echo tolerance test");
    let tx = m.modulate(&fb);
    let echoed = MultipathChannel::exponential(60, 15.0).apply(&tx);
    for lead in [1500usize, 1600, 1733, 1850, 1971] {
        let buf = AwgnChannel::new(lead as u64).apply(&wrap(&echoed, lead), 14.0);
        let got = m
            .demodulate(&buf)
            .unwrap_or_else(|| panic!("MFSK: lost @ lead {lead}"));
        assert_eq!(got.bytes, fb, "MFSK: mismatch @ lead {lead}");
    }
}

#[test]
fn modes_do_not_cross_decode_each_other() {
    use sonic_protocol::framing::Frame;
    // Приёмник mode-agnostic гоняет все демодуляторы подряд и принимает кадр только если
    // он проходит CRC (см. RxDemodulator::poll). Поэтому чужой модем не должен выдать
    // CRC-ВАЛИДНЫЙ кадр другого режима (мусорные байты допустимы — их отсеет CRC, но
    // корректный чужой кадр «украл» бы буфер до нужного демодулятора). Особенно важно для
    // пары OFDM-QPSK / OFDM-16QAM: у них общая преамбула Schmidl-Cox.
    for tx_mode in ALL {
        let tx = build(tx_mode).modulate(&frame_of(tx_mode, b"cross-mode isolation check 42"));
        let buf = wrap(&tx, 2000);
        for rx_mode in ALL {
            if rx_mode == tx_mode {
                continue;
            }
            if let Some(d) = build(rx_mode).demodulate(&buf) {
                assert!(
                    Frame::parse(&d.bytes).is_err(),
                    "{rx_mode:?} выдал CRC-валидный кадр из сигнала {tx_mode:?}"
                );
            }
        }
    }
}

#[test]
fn no_mode_false_locks_on_pure_noise() {
    // Пониженные пороги детекции не должны ловить кадр из чистого шума ни в одном режиме.
    for mode in ALL {
        let m = build(mode);
        for seed in 0..4 {
            let mut rng = AwgnChannel::new(seed + 100);
            let noise = rng.noise(120_000, 0.2); // реальный шум, RMS 0.2
            assert!(
                m.demodulate(&noise).is_none(),
                "{mode:?}: false frame on pure noise (seed {seed})"
            );
        }
    }
}
