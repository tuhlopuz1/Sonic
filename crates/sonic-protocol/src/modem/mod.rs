//! Модуляторы физического уровня и общий трейт [`Modem`].
//!
//! Каждый модем самодостаточен: несёт свою преамбулу для синхронизации и робастный
//! префикс длины, поэтому демодулятор никогда не «гадает» длину кадра (цель
//! PROTOCOL.md §1). Модем — это байт-в-сэмплы и обратно; байтовую структуру кадра
//! (заголовок/CRC) знает [`crate::framing`], а не модем.
//!
//! Модем принимает под-полосу и sample rate от активной [`DuplexScheme`]
//! (`crate::bandplan`), поэтому не знает про FDD vs shared-band — это шов под будущий AEC.
//!
//! Реализации:
//! - [`CssModem`] — Chirp Spread Spectrum (LoRa-style), надёжный, PROTOCOL.md §4.
//! - [`OfdmModem`] — OFDM+QAM (Schmidl-Cox, пилот-эквалайзер), быстрый, PROTOCOL.md §5.

pub mod css;
pub mod ofdm;
pub mod qam;

pub use css::CssModem;
pub use ofdm::OfdmModem;

use crate::framing::PhyMode;

/// Состояние приёмного КА модема (plan.md §2, для телеметрии/отладки).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModemState {
    /// Ничего не принимаем — тишина/шум.
    Idle,
    /// Обнаружена энергия, ищем преамбулу.
    Searching,
    /// Преамбула поймана, синхронизированы.
    Synced,
    /// Демодулируем тело кадра.
    Decoding,
}

/// Результат успешной демодуляции одного кадра.
#[derive(Debug, Clone)]
pub struct Demodulated {
    /// Восстановленные байты кадра (то, что подавалось в `modulate`).
    pub bytes: Vec<u8>,
    /// Индекс сэмпла, где кадр начался (для продолжения поиска в потоке).
    pub start_sample: usize,
    /// Индекс сэмпла сразу за концом кадра.
    pub end_sample: usize,
    /// Оценка SNR по преамбуле, дБ.
    pub snr_db: f32,
}

/// Общий интерфейс модуляторов. Реализации работают в под-полосе, заданной при создании.
pub trait Modem: Send {
    fn mode(&self) -> PhyMode;

    /// Модулирует байтовый кадр в вещественные passband-сэмплы (уже в своей под-полосе).
    fn modulate(&self, frame_bytes: &[u8]) -> Vec<f32>;

    /// Ищет и демодулирует первый кадр в `samples`. `None`, если кадр не найден.
    fn demodulate(&self, samples: &[f32]) -> Option<Demodulated>;

    /// Сколько сэмплов занимает кадр из `payload_len` байт — для оценок длительности/скорости.
    fn frame_samples(&self, payload_len: usize) -> usize;
}

/// Упаковка байт в SF-битные символы (MSB-first внутри байта) и обратно — общая
/// утилита для CSS (символ = SF бит). Возвращает вектор значений символов < 2^SF.
pub(crate) fn bytes_to_symbols(bytes: &[u8], sf: u32) -> Vec<u16> {
    let mut symbols = Vec::new();
    let mut acc: u32 = 0;
    let mut nbits = 0u32;
    for &b in bytes {
        acc = (acc << 8) | b as u32;
        nbits += 8;
        while nbits >= sf {
            nbits -= sf;
            symbols.push(((acc >> nbits) & ((1 << sf) - 1)) as u16);
        }
    }
    if nbits > 0 {
        // Хвост дополняется нулями справа до полного символа.
        symbols.push(((acc << (sf - nbits)) & ((1 << sf) - 1)) as u16);
    }
    symbols
}

/// Обратная операция: символы → байты. `nbytes` — сколько байт вернуть (обрезает паддинг).
pub(crate) fn symbols_to_bytes(symbols: &[u16], sf: u32, nbytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(nbytes);
    let mut acc: u32 = 0;
    let mut nbits = 0u32;
    for &s in symbols {
        acc = (acc << sf) | (s as u32 & ((1 << sf) - 1));
        nbits += sf;
        while nbits >= 8 && out.len() < nbytes {
            nbits -= 8;
            out.push(((acc >> nbits) & 0xFF) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_packing_roundtrip_sf8() {
        let bytes = b"acoustic";
        let syms = bytes_to_symbols(bytes, 8);
        assert_eq!(syms.len(), bytes.len()); // SF=8 → 1 символ/байт
        assert_eq!(symbols_to_bytes(&syms, 8, bytes.len()), bytes);
    }

    #[test]
    fn symbol_packing_roundtrip_sf10() {
        let bytes = b"chirp spread spectrum";
        let syms = bytes_to_symbols(bytes, 10);
        assert!(syms.iter().all(|&s| s < 1024));
        assert_eq!(symbols_to_bytes(&syms, 10, bytes.len()), bytes);
    }
}
