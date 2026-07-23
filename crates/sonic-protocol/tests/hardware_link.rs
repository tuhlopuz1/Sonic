//! Регрессия «связь между устройствами работает на РЕАЛЬНОМ железе»: кадр каждого режима
//! проходит через реалистичный band-limited тракт «динамик→воздух→микрофон» (см.
//! `sim::hardware`). Раньше такого теста не было — канал был спектрально-плоским, поэтому
//! симуляция была зелёной, а по воздуху связь не поднималась (обратная FDD-полоса 8–15 кГц
//! на железе не звучала). Теперь всё в общей полосе 0.5–3.7 кГц и обязано доходить.

use sonic_protocol::bandplan::{DuplexScheme, Profile, Role, Tdd};
use sonic_protocol::framing::{Frame, FrameHeader, FrameType, PhyMode};
use sonic_protocol::modem::qam::Modulation;
use sonic_protocol::modem::{CssModem, MfskModem, Modem, OfdmModem};
use sonic_protocol::sim::OverTheAir;

fn build(mode: PhyMode) -> Box<dyn Modem> {
    // Полудуплекс: обе стороны в общей полосе, поэтому роль тут не важна для полосы.
    let link = Tdd::new(Role::Initiator, Profile::Audible);
    let band = link.tx_band();
    let sr = link.sample_rate();
    match mode {
        PhyMode::Css => Box::new(CssModem::with_defaults(band, sr)),
        PhyMode::Mfsk => Box::new(MfskModem::new(band, sr)),
        PhyMode::OfdmQpsk => Box::new(OfdmModem::new(band, sr, Modulation::Qpsk)),
        PhyMode::Ofdm16Qam => Box::new(OfdmModem::new(band, sr, Modulation::Qam16)),
    }
}

fn frame_of(mode: PhyMode, dir: u8, msg: &[u8]) -> Vec<u8> {
    Frame::new(FrameHeader::new(mode, FrameType::Data, dir), msg.to_vec()).serialize()
}

fn wrap(tx: &[f32], lead: usize) -> Vec<f32> {
    let mut buf = vec![0.0f32; lead];
    buf.extend_from_slice(tx);
    buf.extend(std::iter::repeat(0.0).take(4000));
    buf
}

/// Надёжные режимы — рабочие лошадки авто-режима; обязаны доходить всегда.
const RELIABLE: [PhyMode; 3] = [PhyMode::Css, PhyMode::Mfsk, PhyMode::OfdmQpsk];

#[test]
fn reliable_modes_survive_realistic_hardware_both_directions() {
    let sr = 48_000.0;
    for mode in RELIABLE {
        // Оба направления (бит direction 0/1) — в одной полосе, оба должны декодироваться.
        for dir in [0u8, 1u8] {
            let m = build(mode);
            let fb = frame_of(mode, dir, b"device-to-device over real speakers and mics");
            let tx = m.modulate(&fb);
            // Несколько независимых реализаций шума/тракта — устойчивость, а не везение.
            let mut ok = 0;
            for seed in 0..5 {
                let rx = OverTheAir::typical(sr, seed).apply(&wrap(&tx, 2000));
                if matches!(m.demodulate(&rx), Some(d) if d.bytes == fb) {
                    ok += 1;
                }
            }
            assert!(ok >= 5, "{mode:?} dir={dir}: только {ok}/5 прошло через реальный тракт");
        }
    }
}

#[test]
fn css_survives_worst_case_voice_limited_phone_path() {
    // Худший случай: телефон с голосовой обработкой ОС (полоса ~0.3–3.8 кГц, шумно, громкий
    // динамик). Это и есть та ситуация, ради которой существует CSS — самый надёжный режим
    // (spread spectrum, большой processing gain). Гарантия связи: в худшем случае Auto
    // спускается до CSS, и он ОБЯЗАН доходить. Более быстрые режимы (MFSK/OFDM) здесь могут
    // деградировать — именно поэтому лестница fallback заканчивается на CSS.
    let sr = 48_000.0;
    let m = build(PhyMode::Css);
    let fb = frame_of(PhyMode::Css, 0, b"voice-limited phone microphone path");
    let tx = m.modulate(&fb);
    let mut ok = 0;
    for seed in 0..6 {
        let rx = OverTheAir::harsh_voice(sr, seed).apply(&wrap(&tx, 2000));
        if matches!(m.demodulate(&rx), Some(d) if d.bytes == fb) {
            ok += 1;
        }
    }
    assert!(ok >= 6, "CSS: только {ok}/6 прошло голосовой тракт телефона (это последний рубеж связи)");
}
