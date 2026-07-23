//! PHY/MAC-кадр ADL/1: сериализация в байты + разбор с проверкой CRC.
//!
//! Кадр (байтовый уровень, ср. PROTOCOL.md §6/§7.2):
//! ```text
//! ┌──────────────── header (12 байт) ─────────────────┬─ payload ─┬─ CRC32 ─┐
//! │ magic ver|mode type  src   seq   ack   sack_bitmap │   …       │  tail   │
//! └────────────────────────────────────────────────────┴──────────┴─────────┘
//! ```
//! `src` — случайный идентификатор устройства на сессию: приёмник ИГНОРИРУЕТ кадры со СВОИМ
//! `src` (это своё эхо) и принимает кадры с ЧУЖИМ `src` (это пир). Так связь работает без
//! согласования ролей: два устройства почти наверняка выберут разные src (а разные роли —
//! гарантированно, роль задаёт старший бит).
//! Этот байтовый кадр — единица, которую модулятор ([`crate::modem`]) кладёт в эфир.
//! Заголовок робастный и mode-agnostic: поле `mode` говорит, каким FEC закодирован
//! payload, поэтому приёмнику не нужно заранее знать режим (основа auto-fallback,
//! plan.md §2). Сам модем несёт длину кадра в своей преамбуле/служебном префиксе, так
//! что демодулятор OFDM никогда не «гадает» длину (цель PROTOCOL.md §1).
//!
//! Заметка о слоях: seq/ack/sack живут здесь (робастно закодированы в CSS-преамбуле
//! кадра модемом), чтобы ARQ-подтверждения работали встречным потоком даже когда
//! payload идёт в хрупком OFDM (plan.md §2). CRC32 хвоста ловит любую порчу payload.

use crc32fast::Hasher;
use serde::{Deserialize, Serialize};

/// Режим модуляции payload. Дискриминант кладётся в заголовок (PROTOCOL.md §6.1, Mode).
///
/// Лестница «надёжно→быстро»: CSS (чирп, макс. processing gain) → MFSK (некогерентные
/// тоны, быстрее, устойчив к сдвигам частоты/времени) → OFDM+QPSK → OFDM+16-QAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhyMode {
    /// CSS — Chirp Spread Spectrum, самый надёжный, стартовый режим (PROTOCOL.md §9).
    Css,
    /// MFSK — M-ичная частотная манипуляция: некогерентный приём (пик FFT), проще и
    /// быстрее CSS, очень устойчив к рассинхрону тактовой частоты и грубому таймингу.
    Mfsk,
    /// OFDM + QPSK — быстрый режим, фазовая манипуляция поднесущих (устойчив к AGC).
    OfdmQpsk,
    /// OFDM + 16-QAM — самый быстрый из реализованных (амплитудно-фазовое созвездие).
    Ofdm16Qam,
}

impl PhyMode {
    pub fn to_bits(self) -> u8 {
        match self {
            PhyMode::Css => 0,
            PhyMode::Mfsk => 1,
            PhyMode::OfdmQpsk => 2,
            PhyMode::Ofdm16Qam => 3,
        }
    }
    pub fn from_bits(b: u8) -> Option<Self> {
        match b {
            0 => Some(PhyMode::Css),
            1 => Some(PhyMode::Mfsk),
            2 => Some(PhyMode::OfdmQpsk),
            3 => Some(PhyMode::Ofdm16Qam),
            _ => None,
        }
    }
    pub fn is_ofdm(self) -> bool {
        matches!(self, PhyMode::OfdmQpsk | PhyMode::Ofdm16Qam)
    }

    /// Человекочитаемая метка для UI/логов.
    pub fn label(self) -> &'static str {
        match self {
            PhyMode::Css => "CSS",
            PhyMode::Mfsk => "MFSK",
            PhyMode::OfdmQpsk => "OFDM-QPSK",
            PhyMode::Ofdm16Qam => "OFDM-16QAM",
        }
    }
}

/// Тип MAC-кадра (PROTOCOL.md §7.2). Управляющие типы всегда идут в CSS (§9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameType {
    Hello,
    HelloAck,
    Data,
    Ack,
    Nack,
    ModeRequest,
    Ping,
    Bye,
}

impl FrameType {
    fn to_bits(self) -> u8 {
        match self {
            FrameType::Hello => 0,
            FrameType::HelloAck => 1,
            FrameType::Data => 2,
            FrameType::Ack => 3,
            FrameType::Nack => 4,
            FrameType::ModeRequest => 5,
            FrameType::Ping => 6,
            FrameType::Bye => 7,
        }
    }
    fn from_bits(b: u8) -> Option<Self> {
        Some(match b {
            0 => FrameType::Hello,
            1 => FrameType::HelloAck,
            2 => FrameType::Data,
            3 => FrameType::Ack,
            4 => FrameType::Nack,
            5 => FrameType::ModeRequest,
            6 => FrameType::Ping,
            7 => FrameType::Bye,
            _ => return None,
        })
    }
}

const MAGIC: u8 = 0x2B;
const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 12;
pub const CRC_LEN: usize = 4;
pub const OVERHEAD: usize = HEADER_LEN + CRC_LEN;

/// Робастный заголовок кадра.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub mode: PhyMode,
    pub frame_type: FrameType,
    /// Идентификатор устройства-отправителя (случайный на сессию). Приёмник отбрасывает
    /// кадры со своим `src` (собственное эхо) и принимает кадры с чужим `src` (пир). Заменяет
    /// прежний бит направления: связь больше не требует согласования ролей вручную.
    pub src: u8,
    /// Номер кадра отправителя (ARQ).
    pub seq: u16,
    /// Кумулятивный ACK: последний подряд принятый seq пира.
    pub ack: u16,
    /// Селективный ACK: битовая маска следующих 32 seq после `ack` (аналог TCP SACK).
    pub sack: u32,
}

impl FrameHeader {
    pub fn new(mode: PhyMode, frame_type: FrameType, src: u8) -> Self {
        FrameHeader {
            mode,
            frame_type,
            src,
            seq: 0,
            ack: 0,
            sack: 0,
        }
    }
}

/// Разобранный кадр: заголовок + сырой payload (до расшифровки/верхних слоёв).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FramingError {
    TooShort,
    BadMagic,
    BadVersion,
    BadCrc,
    BadField,
}

impl Frame {
    pub fn new(header: FrameHeader, payload: Vec<u8>) -> Self {
        Frame { header, payload }
    }

    /// Сериализация: header || payload || CRC32(header||payload).
    pub fn serialize(&self) -> Vec<u8> {
        let h = &self.header;
        let mut out = Vec::with_capacity(OVERHEAD + self.payload.len());
        out.push(MAGIC);
        out.push((VERSION << 4) | (h.mode.to_bits() & 0x0F));
        out.push(h.frame_type.to_bits());
        out.push(h.src);
        out.extend_from_slice(&h.seq.to_be_bytes());
        out.extend_from_slice(&h.ack.to_be_bytes());
        out.extend_from_slice(&h.sack.to_be_bytes());
        debug_assert_eq!(out.len(), HEADER_LEN);
        out.extend_from_slice(&self.payload);

        let mut hasher = Hasher::new();
        hasher.update(&out);
        let crc = hasher.finalize();
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    /// Разбор с полной проверкой (magic, версия, CRC). Битый кадр → `Err` (молча
    /// отбрасывается выше по стеку, инициирует NACK/таймаут — PROTOCOL.md §6.3).
    pub fn parse(bytes: &[u8]) -> Result<Frame, FramingError> {
        if bytes.len() < OVERHEAD {
            return Err(FramingError::TooShort);
        }
        if bytes[0] != MAGIC {
            return Err(FramingError::BadMagic);
        }
        let version = bytes[1] >> 4;
        if version != VERSION {
            return Err(FramingError::BadVersion);
        }

        let body_len = bytes.len() - CRC_LEN;
        let mut hasher = Hasher::new();
        hasher.update(&bytes[..body_len]);
        let crc = hasher.finalize();
        let got = u32::from_be_bytes(bytes[body_len..].try_into().unwrap());
        if crc != got {
            return Err(FramingError::BadCrc);
        }

        let mode = PhyMode::from_bits(bytes[1] & 0x0F).ok_or(FramingError::BadField)?;
        let frame_type = FrameType::from_bits(bytes[2]).ok_or(FramingError::BadField)?;
        let src = bytes[3];
        let seq = u16::from_be_bytes([bytes[4], bytes[5]]);
        let ack = u16::from_be_bytes([bytes[6], bytes[7]]);
        let sack = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let payload = bytes[HEADER_LEN..body_len].to_vec();

        Ok(Frame {
            header: FrameHeader {
                mode,
                frame_type,
                src,
                seq,
                ack,
                sack,
            },
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame() -> Frame {
        let mut h = FrameHeader::new(PhyMode::OfdmQpsk, FrameType::Data, 1);
        h.seq = 0x1234;
        h.ack = 0x00FF;
        h.sack = 0xDEAD_BEEF;
        Frame::new(h, b"acoustic messenger payload".to_vec())
    }

    #[test]
    fn serialize_parse_roundtrip() {
        let frame = sample_frame();
        let bytes = frame.serialize();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed, frame);
    }

    #[test]
    fn crc_catches_payload_corruption() {
        let mut bytes = sample_frame().serialize();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x01;
        assert_eq!(Frame::parse(&bytes), Err(FramingError::BadCrc));
    }

    #[test]
    fn rejects_bad_magic_and_short() {
        let mut bytes = sample_frame().serialize();
        bytes[0] = 0x00;
        assert_eq!(Frame::parse(&bytes), Err(FramingError::BadMagic));
        assert_eq!(Frame::parse(&[0u8; 4]), Err(FramingError::TooShort));
    }

    #[test]
    fn empty_payload_control_frame() {
        let h = FrameHeader::new(PhyMode::Css, FrameType::Ack, 0);
        let frame = Frame::new(h, Vec::new());
        let bytes = frame.serialize();
        assert_eq!(Frame::parse(&bytes).unwrap(), frame);
    }
}
