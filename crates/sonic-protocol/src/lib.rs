//! # sonic-protocol — ядро ADL/1 (Acoustic Data Link)
//!
//! Чистое DSP/протокольное ядро full-duplex акустического мессенджера: модуляция
//! (CSS / OFDM+QAM), FEC (Reed-Solomon + интерливер), framing с mode-agnostic
//! заголовком, ARQ и симулированный канал для тестов/BER-замеров.
//!
//! Крейт намеренно не зависит от ОС/аудио (нет cpal/tauri) — весь код здесь
//! проверяется `cargo test` без звукового железа, а cpal-движок живёт в `sonic-audio`.
//!
//! Карта слоёв (ср. PROTOCOL.md §1):
//! - [`bandplan`]   — план спектра, роль A/B, шов FDD→AEC ([`DuplexScheme`]).
//! - [`modem`]      — [`Modem`] трейт и две реализации: [`modem::CssModem`], [`modem::OfdmModem`].
//! - [`framing`]    — PHY-кадр: преамбула + робастный заголовок + payload + CRC.
//! - [`fec`]        — Reed-Solomon над GF(256) + блочный интерливер.
//! - [`arq`]        — скользящее окно ARQ и авто-fallback OFDM→CSS.
//! - [`sim`]        — AWGN / multipath / clock-drift каналы (тесты и BER-свипы).
//! - [`telemetry`]  — [`LinkQuality`], прокидывается в UI-слой.

pub mod arq;
pub mod bandplan;
pub mod fec;
pub mod fft;
pub mod framing;
pub mod iq;
pub mod modem;
pub mod sim;
pub mod telemetry;

pub use arq::{ArqConfig, ArqReceiver, ArqSender, AutoFallback};
pub use bandplan::{DuplexScheme, EchoCanceller, Fdd, NoopEchoCanceller, Profile, Role, SubBand};
pub use framing::{Frame, FrameHeader, FramingError, PhyMode};
pub use modem::{CssModem, Modem, ModemState, OfdmModem};
pub use telemetry::LinkQuality;
