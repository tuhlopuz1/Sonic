//! Сквозной тест полного стека: текстовое сообщение → FEC → PHY-кадр → модем →
//! симулированный канал (AWGN + многолучёвость) → демод → разбор кадра → FEC-декод →
//! исходное сообщение. Проверяет, что слои стыкуются end-to-end (то, что реально
//! происходит «под капотом» при отправке сообщения в чате).

use sonic_protocol::bandplan::{DuplexScheme, Fdd, Profile, Role};
use sonic_protocol::fec::FecCodec;
use sonic_protocol::framing::{Frame, FrameHeader, FrameType, PhyMode};
use sonic_protocol::modem::qam::Modulation;
use sonic_protocol::modem::{CssModem, Modem, OfdmModem};
use sonic_protocol::sim::{AwgnChannel, MultipathChannel};

/// Сборка полезной нагрузки кадра: [длина сообщения u16][сообщение] под защитой FEC.
fn build_payload(fec: &FecCodec, message: &[u8]) -> Vec<u8> {
    let mut inner = (message.len() as u16).to_be_bytes().to_vec();
    inner.extend_from_slice(message);
    fec.encode(&inner)
}

/// Обратная сборка: FEC-декод → снять длину → вернуть сообщение.
fn recover_message(fec: &FecCodec, payload: &[u8]) -> Option<Vec<u8>> {
    let inner = fec.decode(payload).ok()?;
    if inner.len() < 2 {
        return None;
    }
    let len = u16::from_be_bytes([inner[0], inner[1]]) as usize;
    if 2 + len > inner.len() {
        return None;
    }
    Some(inner[2..2 + len].to_vec())
}

fn wrap_with_silence(tx: &[f32]) -> Vec<f32> {
    let mut buf = vec![0.0f32; 2500];
    buf.extend_from_slice(tx);
    buf.extend(std::iter::repeat(0.0).take(2500));
    buf
}

fn roundtrip(modem: &dyn Modem, mode: PhyMode, message: &[u8], snr_db: f32, seed: u64) -> Option<Vec<u8>> {
    let fec = FecCodec::new(32, 16);
    let payload = build_payload(&fec, message);

    let mut header = FrameHeader::new(mode, FrameType::Data, 0);
    header.seq = 7;
    let frame = Frame::new(header, payload);
    let frame_bytes = frame.serialize();

    let tx = modem.modulate(&frame_bytes);
    let mut buf = wrap_with_silence(&tx);
    let mut awgn = AwgnChannel::new(seed);
    buf = awgn.apply(&buf, snr_db);

    let demod = modem.demodulate(&buf)?;
    let parsed = Frame::parse(&demod.bytes).ok()?;
    recover_message(&fec, &parsed.payload)
}

#[test]
fn css_message_survives_noisy_channel() {
    let fdd = Fdd::new(Role::Initiator, Profile::Audible);
    let css = CssModem::with_defaults(fdd.tx_band(), fdd.sample_rate());
    let message = b"Full-duplex acoustic messenger: hello over CSS!";
    let got = roundtrip(&css, PhyMode::Css, message, 6.0, 1).expect("CSS message lost");
    assert_eq!(got, message);
}

#[test]
fn ofdm_qpsk_message_clean_channel() {
    let fdd = Fdd::new(Role::Initiator, Profile::Audible);
    let ofdm = OfdmModem::new(fdd.tx_band(), fdd.sample_rate(), Modulation::Qpsk);
    let message = b"OFDM QPSK carries the chat payload much faster than CSS.";
    let got = roundtrip(&ofdm, PhyMode::OfdmQpsk, message, 20.0, 2).expect("OFDM message lost");
    assert_eq!(got, message);
}

#[test]
fn fec_repairs_symbol_errors_end_to_end() {
    // FEC должен вытянуть кадр даже когда модем отдал байты с несколькими ошибками.
    let fec = FecCodec::new(32, 16); // t=8 на блок
    let message = b"reed-solomon repairs demodulation errors";
    let payload = build_payload(&fec, message);

    // Прямо портим payload (эмулируем ошибки демодуляции) в пределах исправимого.
    let mut corrupted = payload.clone();
    for i in [1usize, 5, 9, 40, 41, 42] {
        if i < corrupted.len() {
            corrupted[i] ^= 0x7C;
        }
    }
    let recovered = recover_message(&fec, &corrupted).expect("FEC failed to repair");
    assert_eq!(recovered, message);
}

#[test]
fn css_survives_multipath_plus_noise() {
    let fdd = Fdd::new(Role::Initiator, Profile::Audible);
    let css = CssModem::with_defaults(fdd.tx_band(), fdd.sample_rate());
    let fec = FecCodec::new(32, 16);
    let message = b"CSS is robust to room reverberation and noise";
    let payload = build_payload(&fec, message);
    let frame = Frame::new(FrameHeader::new(PhyMode::Css, FrameType::Data, 0), payload);

    let tx = css.modulate(&frame.serialize());
    // Многолучёвость (реверберация) + шум.
    let channel = MultipathChannel::exponential(96, 20.0);
    let echoed = channel.apply(&tx);
    let mut buf = wrap_with_silence(&echoed);
    buf = AwgnChannel::new(5).apply(&buf, 12.0);

    let demod = css.demodulate(&buf).expect("CSS lost under multipath+noise");
    let parsed = Frame::parse(&demod.bytes).expect("frame parse failed");
    assert_eq!(recover_message(&fec, &parsed.payload).unwrap(), message);
}
