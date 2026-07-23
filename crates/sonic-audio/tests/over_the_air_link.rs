//! Сквозная регрессия «два устройства реально слышат друг друга»: полный приёмный конвейер
//! `RxDemodulator` принимает кадр, отправленный `Transmitter` пира, ПРОШЕДШИЙ через
//! реалистичный band-limited тракт «динамик→воздух→микрофон» (`sim::hardware::OverTheAir`) и
//! пару ресемплингов 48к↔44.1к (несовпадение частот железа). Это ближе всего к настоящей
//! связи между телефонами из того, что можно проверить без железа.
//!
//! Раньше такого теста не было, и симуляция (спектрально-плоская) была зелёной, пока по
//! воздуху связь не поднималась. Теперь оба надёжных режима обязаны доходить в ОБЕ стороны.

use sonic_audio::pipeline::{RxDemodulator, Transmitter};
use sonic_audio::resample::Resampler;
use sonic_protocol::bandplan::{DuplexScheme, Profile, Role, Tdd};
use sonic_protocol::framing::{Frame, FrameHeader, FrameType, PhyMode};
use sonic_protocol::sim::OverTheAir;

/// Прогоняет кадр `sender → channel → receiver` через полный конвейер и возвращает, декодировался
/// ли он в точности. Ресемплинг 48к→44.1к→48к моделирует несовпадение частот звуковых карт.
fn deliver(sender: &Tdd, receiver: &Tdd, mode: PhyMode, msg: &[u8], seed: u64) -> bool {
    let tx = Transmitter::new(sender);
    let mut rx = RxDemodulator::new(receiver);

    let frame = Frame::new(
        FrameHeader::new(mode, FrameType::Data, sender.role().direction_bit()),
        msg.to_vec(),
    );
    let clean = tx.modulate(mode, &frame.serialize()); // 48 кГц
    // Несовпадение частот железа: 48к → 44.1к (динамик) … → 44.1к → 48к (микрофон приёмника).
    let at_hw = Resampler::new(48_000, 44_100).process_all(&clean);
    let aired = OverTheAir::typical(44_100.0, seed).apply(&at_hw);
    let back = Resampler::new(44_100, 48_000).process_all(&aired);

    // Немного тишины по краям; хвост короче окна «свежести» приёмника, чтобы кадр остался в нём.
    rx.push_captured(&vec![0.0f32; 4000]);
    for chunk in back.chunks(4096) {
        rx.push_captured(chunk);
    }
    rx.push_captured(&vec![0.0f32; 2000]);

    rx.poll()
        .iter()
        .filter_map(|ev| Frame::parse(&ev.bytes).ok())
        .any(|f| f == frame)
}

const RELIABLE: [PhyMode; 3] = [PhyMode::Css, PhyMode::Mfsk, PhyMode::OfdmQpsk];

#[test]
fn reliable_modes_link_both_directions_over_the_air() {
    let a = Tdd::new(Role::Initiator, Profile::Audible);
    let b = Tdd::new(Role::Responder, Profile::Audible);

    for mode in RELIABLE {
        // A → B и B → A: обе стороны в общей полосе, обе обязаны доходить.
        for (sender, receiver, who) in [(&a, &b, "A→B"), (&b, &a, "B→A")] {
            let mut ok = 0;
            for seed in 0..4 {
                if deliver(sender, receiver, mode, b"hello from the other phone", seed) {
                    ok += 1;
                }
            }
            assert!(ok >= 4, "{mode:?} {who}: только {ok}/4 кадров дошло через реальный тракт");
        }
    }
}

#[test]
fn css_fallback_links_over_the_air() {
    // CSS — гарантированный нижний рубеж связи: даже отдельным прогоном обязан доходить.
    let a = Tdd::new(Role::Initiator, Profile::Audible);
    let b = Tdd::new(Role::Responder, Profile::Audible);
    assert!(deliver(&a, &b, PhyMode::Css, b"css last-resort delivery", 1));
    assert!(deliver(&b, &a, PhyMode::Css, b"css last-resort delivery", 2));
}
