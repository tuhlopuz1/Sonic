//! Пайплайн обработки: сырые сэмплы ↔ модем.
//!
//! RX: захваченные сэмплы → (шов) эхоподавитель → скользящий буфер → демодуляторы →
//! события наверх. TX: байты кадра → модем → сэмплы в буфер воспроизведения, с отводом
//! копии как AEC-reference (plan.md §3 — точка отвода заложена сразу, даже если FDD её
//! не использует).
//!
//! Приёмник mode-agnostic: гоняет CSS и оба OFDM-демодулятора, кто первым поймал свою
//! преамбулу — тот и декодировал (auto-fallback работает без предварительного знания
//! режима). Демодуляция запускается только когда в буфере есть энергия (гейт), чтобы в
//! тишине не жечь CPU.
//!
//! Замечание о производительности: при длинных CSS-кадрах (низкая скорость → большой
//! буфер) демодуляция по всему буферу дорога; здесь это ограничено энергетическим
//! гейтом и обрезкой буфера. Инкрементальная даунконверсия — очевидная будущая
//! оптимизация (Фаза 2/6 в plan.md), не заглушка.

use sonic_protocol::bandplan::{DuplexScheme, EchoCanceller, SubBand};
use sonic_protocol::modem::qam::Modulation;
use sonic_protocol::modem::{CssModem, MfskModem, Modem, OfdmModem};
use sonic_protocol::framing::{Frame, PhyMode};

/// Событие приёма кадра из эфира (сырые байты кадра до разбора framing).
#[derive(Debug, Clone)]
pub struct RxEvent {
    pub bytes: Vec<u8>,
    pub snr_db: f32,
    pub mode: PhyMode,
}

/// Строит полный набор демодуляторов для полосы (CSS + MFSK + оба OFDM) — приёмник
/// mode-agnostic. У каждого свой sync-маркер, поэтому перекрёстных ложных срабатываний нет.
fn build_modems(band: SubBand, sample_rate: u32) -> Vec<Box<dyn Modem>> {
    vec![
        Box::new(CssModem::with_defaults(band, sample_rate)),
        Box::new(MfskModem::new(band, sample_rate)),
        Box::new(OfdmModem::new(band, sample_rate, Modulation::Qpsk)),
        Box::new(OfdmModem::new(band, sample_rate, Modulation::Qam16)),
    ]
}

/// Приёмный конвейер: копит захваченные сэмплы и выдаёт декодированные кадры.
pub struct RxDemodulator {
    modems: Vec<Box<dyn Modem>>,
    canceller: Box<dyn EchoCanceller>,
    buf: Vec<f32>,
    /// Кольцо reference-сэмплов (то, что мы воспроизводим) для эхоподавителя.
    reference: std::collections::VecDeque<f32>,
    max_buf: usize,
    min_attempt: usize,
    sample_rate: usize,
    /// Адаптивный шумовой пол (EWMA RMS) — гейт считается относительно него, а не по
    /// фиксированному порогу, иначе тихий захват микрофона блокируется навсегда.
    noise_rms: f32,
    /// Троттлинг тяжёлой демодуляции: сколько новых сэмплов накоплено с прошлой попытки.
    since_attempt: usize,
    /// Троттлинг диагностического лога уровня захвата.
    since_log: usize,
}

impl RxDemodulator {
    /// Демодуляторы строятся на ПРИЁМНОЙ полосе (полоса передачи пира).
    pub fn new(scheme: &dyn DuplexScheme) -> Self {
        let band = scheme.rx_band();
        let sr = scheme.sample_rate();
        let modems = build_modems(band, sr);
        // Буфер должен вмещать самый длинный ожидаемый кадр (CSS медленный) с запасом.
        let max_frame = modems
            .iter()
            .map(|m| m.frame_samples(64))
            .max()
            .unwrap_or(sr as usize);
        RxDemodulator {
            modems,
            canceller: scheme.echo_canceller(),
            buf: Vec::new(),
            reference: std::collections::VecDeque::new(),
            max_buf: max_frame * 2,
            min_attempt: sr as usize / 8, // не пытаться, пока нет хотя бы ~125 мс
            sample_rate: sr as usize,
            noise_rms: 0.0,
            since_attempt: 0,
            since_log: 0,
        }
    }

    /// Кладёт воспроизводимые сэмплы в reference-кольцо (для AEC-шва).
    pub fn push_reference(&mut self, played: &[f32]) {
        for &s in played {
            self.reference.push_back(s);
        }
        // Reference не должен расти безгранично.
        while self.reference.len() > self.max_buf {
            self.reference.pop_front();
        }
    }

    /// Кладёт захваченные с микрофона сэмплы; прогоняет через эхоподавитель (шов).
    pub fn push_captured(&mut self, captured: &[f32]) {
        let mut block = captured.to_vec();
        // Reference того же интервала (приблизительно) — для FDD canceller его игнорирует.
        let mut refs: Vec<f32> = Vec::with_capacity(block.len());
        for _ in 0..block.len() {
            refs.push(self.reference.pop_front().unwrap_or(0.0));
        }
        self.canceller.process(&mut block, &refs);
        self.buf.extend_from_slice(&block);
        self.since_attempt += block.len();
        self.since_log += block.len();
    }

    /// Уровень (RMS) последнего ~0.25 c буфера.
    fn recent_rms(&self) -> f32 {
        let win = (self.sample_rate / 4).max(1).min(self.buf.len());
        if win == 0 {
            return 0.0;
        }
        let tail = &self.buf[self.buf.len() - win..];
        (tail.iter().map(|x| x * x).sum::<f32>() / win as f32).sqrt()
    }

    /// Пытается извлечь готовые кадры из буфера. Тяжёлая демодуляция запускается только
    /// при наличии энергии (относительно адаптивного шумового пола) и не чаще, чем раз в
    /// ~100 мс накопленного аудио — иначе на длинном CSS-кадре демод по всему буферу
    /// каждые 5 мс пожирает CPU.
    pub fn poll(&mut self) -> Vec<RxEvent> {
        let mut out = Vec::new();
        let recent = self.recent_rms();

        // Диагностика раз в ~секунду: видно, слышит ли микрофон хоть что-то.
        if self.since_log >= self.sample_rate {
            self.since_log = 0;
            eprintln!(
                "sonic-rx: уровень захвата rms={recent:.4} (шумовой пол {:.4}, буфер {} c)",
                self.noise_rms,
                self.buf.len() / self.sample_rate.max(1)
            );
        }

        // Порог: выше шумового пола, но с низким абсолютным минимумом (тихий микрофон не
        // должен блокироваться навсегда). Множитель 2× (а не 3×): по воздуху между двумя
        // устройствами принятый сигнал бывает всего вдвое-втрое громче шума, и слишком
        // жадный гейт вообще не давал демодулятору шанса. Сам гейт лишь экономит CPU —
        // ложный кадр отсекают sync-маркеры модемов, а не этот порог.
        let gate = (self.noise_rms * 2.0).max(0.0025);
        let has_signal = recent > gate;
        if !has_signal {
            // Тишина/шум — медленно обновляем оценку шумового пола и не демодулируем.
            self.noise_rms = if self.noise_rms == 0.0 {
                recent
            } else {
                0.95 * self.noise_rms + 0.05 * recent
            };
            self.trim_if_needed();
            return out;
        }

        // Троттлинг: не запускать демод чаще, чем раз в ~100 мс нового аудио.
        if self.buf.len() < self.min_attempt || self.since_attempt < self.sample_rate / 10 {
            return out;
        }
        self.since_attempt = 0;

        loop {
            let mut found: Option<(RxEvent, usize)> = None;
            for m in &self.modems {
                if let Some(d) = m.demodulate(&self.buf) {
                    // Принимаем декод только если кадр проходит CRC (Frame::parse). Иначе
                    // чужой модем (например, OFDM-16QAM на сигнале OFDM-QPSK — у них общая
                    // преамбула) «захватил» бы кадр, выдал мусор и СРЕЗАЛ буфер до нужного
                    // демодулятора — настоящий кадр терялся бы. CRC-проверка снимает
                    // неоднозначность: неверная модуляция → мусорные байты → CRC не сойдётся.
                    if Frame::parse(&d.bytes).is_ok() {
                        found = Some((
                            RxEvent {
                                bytes: d.bytes,
                                snr_db: d.snr_db,
                                mode: m.mode(),
                            },
                            d.end_sample,
                        ));
                        break;
                    }
                }
            }
            match found {
                Some((ev, end)) => {
                    eprintln!(
                        "sonic-rx: поймал кадр {:?}, {} байт, SNR {:.1} дБ",
                        ev.mode,
                        ev.bytes.len(),
                        ev.snr_db
                    );
                    out.push(ev);
                    let end = end.min(self.buf.len());
                    self.buf.drain(0..end);
                }
                None => {
                    self.trim_if_needed();
                    break;
                }
            }
        }
        out
    }

    /// Обрезка буфера, если он перерос максимум (кадр так и не собрался/не найден).
    fn trim_if_needed(&mut self) {
        if self.buf.len() > self.max_buf {
            let keep = self.max_buf / 2;
            let drop = self.buf.len() - keep;
            self.buf.drain(0..drop);
        }
    }
}

/// Передатчик: кэширует TX-модемы (дорогие FFT-планы/таблицы чирпов строятся один раз).
pub struct Transmitter {
    css: CssModem,
    mfsk: MfskModem,
    ofdm_qpsk: OfdmModem,
    ofdm_16: OfdmModem,
}

impl Transmitter {
    /// Модемы строятся на ПЕРЕДАЮЩЕЙ полосе активной схемы.
    pub fn new(scheme: &dyn DuplexScheme) -> Self {
        let band = scheme.tx_band();
        let sr = scheme.sample_rate();
        Transmitter {
            css: CssModem::with_defaults(band, sr),
            mfsk: MfskModem::new(band, sr),
            ofdm_qpsk: OfdmModem::new(band, sr, Modulation::Qpsk),
            ofdm_16: OfdmModem::new(band, sr, Modulation::Qam16),
        }
    }

    /// Модулирует байты кадра в выбранном режиме.
    pub fn modulate(&self, mode: PhyMode, frame_bytes: &[u8]) -> Vec<f32> {
        match mode {
            PhyMode::Css => self.css.modulate(frame_bytes),
            PhyMode::Mfsk => self.mfsk.modulate(frame_bytes),
            PhyMode::OfdmQpsk => self.ofdm_qpsk.modulate(frame_bytes),
            PhyMode::Ofdm16Qam => self.ofdm_16.modulate(frame_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonic_protocol::bandplan::{Fdd, Profile, Role};
    use sonic_protocol::framing::{Frame, FrameHeader, FrameType};

    /// Приёмник видит две разные роли: TX одной стороны = RX другой.
    fn peer_schemes() -> (Fdd, Fdd) {
        (
            Fdd::new(Role::Initiator, Profile::Audible),
            Fdd::new(Role::Responder, Profile::Audible),
        )
    }

    #[test]
    fn transmit_then_receive_css_frame() {
        let (initiator, responder) = peer_schemes();
        // Инициатор передаёт (нижняя полоса), респондер принимает.
        let tx = Transmitter::new(&initiator);
        let mut rx = RxDemodulator::new(&responder);

        let frame = Frame::new(
            FrameHeader::new(PhyMode::Css, FrameType::Data, initiator.role().direction_bit()),
            b"streamed CSS frame over the pipeline".to_vec(),
        );
        let samples = tx.modulate(PhyMode::Css, &frame.serialize());

        // Эмулируем захват потоком: тишина, затем кадр по кускам, затем тишина.
        rx.push_captured(&vec![0.0f32; 3000]);
        for chunk in samples.chunks(4096) {
            rx.push_captured(chunk);
        }
        rx.push_captured(&vec![0.0f32; 3000]);

        let events = rx.poll();
        assert_eq!(events.len(), 1, "expected exactly one frame");
        let parsed = Frame::parse(&events[0].bytes).expect("frame parse");
        assert_eq!(parsed, frame);
        assert_eq!(events[0].mode, PhyMode::Css);
    }

    /// Железо приёмника работает на 44.1 кГц, а протокол — на 48 кГц: сигнал «виден»
    /// микрофону на его частоте, движок приводит его обратно к канонической. Кадр обязан
    /// пережить эту пару ресемплингов — иначе на ноутбуках с несовпадающими частотами
    /// (обычное дело в shared-режиме WASAPI) мессенджер не работает вовсе.
    fn survives_44100_hardware(mode: PhyMode, text: &[u8]) {
        use crate::resample::Resampler;
        let (initiator, responder) = peer_schemes();
        let tx = Transmitter::new(&initiator);
        let mut rx = RxDemodulator::new(&responder);

        let frame = Frame::new(
            FrameHeader::new(mode, FrameType::Data, initiator.role().direction_bit()),
            text.to_vec(),
        );
        let samples = tx.modulate(mode, &frame.serialize()); // канонические 48 кГц
        let at_mic = Resampler::new(48_000, 44_100).process_all(&samples); // микрофон 44.1
        let back = Resampler::new(44_100, 48_000).process_all(&at_mic); // обратно в канон

        rx.push_captured(&vec![0.0f32; 3000]);
        rx.push_captured(&back);
        rx.push_captured(&vec![0.0f32; 3000]);

        let events = rx.poll();
        assert_eq!(events.len(), 1, "кадр потерян после ресемплинга 48k↔44.1k ({mode:?})");
        assert_eq!(Frame::parse(&events[0].bytes).unwrap(), frame);
    }

    #[test]
    fn css_survives_44100_hardware() {
        survives_44100_hardware(PhyMode::Css, b"CSS through a 44.1 kHz sound card");
    }

    #[test]
    fn ofdm_survives_44100_hardware() {
        survives_44100_hardware(PhyMode::OfdmQpsk, b"OFDM through a 44.1 kHz sound card");
    }

    #[test]
    fn silence_produces_no_frames() {
        let (_, responder) = peer_schemes();
        let mut rx = RxDemodulator::new(&responder);
        rx.push_captured(&vec![0.0f32; 96_000]);
        assert!(rx.poll().is_empty());
    }

    #[test]
    fn reference_seam_accepts_playback_copy() {
        // Шов AEC: reference принимается и (для FDD) не влияет на приём.
        let (initiator, responder) = peer_schemes();
        let tx = Transmitter::new(&initiator);
        let mut rx = RxDemodulator::new(&responder);
        let frame = Frame::new(
            FrameHeader::new(PhyMode::Css, FrameType::Data, 0),
            b"reference tap does not corrupt FDD receive".to_vec(),
        );
        let samples = tx.modulate(PhyMode::Css, &frame.serialize());
        rx.push_reference(&vec![0.3f32; 5000]); // как будто мы что-то играли
        rx.push_captured(&vec![0.0f32; 3000]);
        rx.push_captured(&samples);
        rx.push_captured(&vec![0.0f32; 3000]);
        let events = rx.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(Frame::parse(&events[0].bytes).unwrap(), frame);
    }
}
