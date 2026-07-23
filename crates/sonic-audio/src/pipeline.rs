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

/// Живой снимок состояния приёмника для отладки/визуализации в UI (уровни, гейт, счётчики).
#[derive(Debug, Clone, Copy, Default)]
pub struct RxStats {
    /// RMS хвоста (~125 мс) — «громкость» того, что микрофон слышит сейчас.
    pub rms: f32,
    /// Пиковая амплитуда хвоста.
    pub peak: f32,
    /// Оценённый шумовой пол.
    pub noise_floor: f32,
    /// Порог детекции сигнала (выше него — «есть звук»).
    pub gate: f32,
    /// Длина накопленного буфера в секундах.
    pub buffer_secs: f32,
    /// Идёт ли сейчас всплеск (в буфере есть звук выше гейта).
    pub in_burst: bool,
    /// Успешно декодированных кадров (CRC ok).
    pub frames_ok: u32,
    /// Всплесков с пойманной синхронизацией, но битым CRC (низкий SNR).
    pub frames_bad: u32,
    /// SNR последнего успешного кадра.
    pub last_snr_db: f32,
    /// Что случилось с последним обработанным всплеском — сразу видно, ГДЕ рвётся приём:
    /// «преамбула не найдена» (тайминг/слишком тихо) vs «CRC битый» (сигнал есть, но шумно).
    pub last_event: &'static str,
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

/// Длина истории уровней для оценки шумового пола: 200 отсчётов × ~100 мс ≈ 20 c — заведомо
/// длиннее самого длинного кадра, поэтому кадр не может «завладеть» оценкой.
const LEVEL_HIST_LEN: usize = 200;
/// Потолок шумового пола. Страховка от самозапирания приёмника: даже при пессимистичной оценке
/// гейт (2.5×) остаётся ниже уровня сигнала устройства рядом.
const NOISE_FLOOR_CAP: f32 = 0.03;

/// Приёмный конвейер: копит захваченные сэмплы и выдаёт декодированные кадры.
///
/// **Сегментация по паузам (VAD-стиль).** Прежний конвейер копил ВЕСЬ звук в один растущий
/// буфер и всегда пытался декодировать С НАЧАЛА — а там застревал ранний шум, на который
/// синхронизация цеплялась один раз и падала; реальный кадр приходил позже и шанса не получал
/// (буфер рос до секунд и не декодировался — ровно то, что видно в логе). В полудуплексе между
/// кадрами ВСЕГДА пауза (stop-and-wait), поэтому здесь кадр = всплеск энергии, ограниченный
/// тишиной: как только хвост стал тихим — демодулируем накопленный всплеск и очищаем буфер
/// (успех или нет — ARQ переотправит). Так гарантирован forward progress и нет застрявшего
/// префикса.
pub struct RxDemodulator {
    modems: Vec<Box<dyn Modem>>,
    canceller: Box<dyn EchoCanceller>,
    buf: Vec<f32>,
    /// Кольцо reference-сэмплов (то, что мы воспроизводим) для эхоподавителя.
    reference: std::collections::VecDeque<f32>,
    /// Самый длинный ожидаемый кадр (CSS медленный) — предел буфера при непрерывной энергии.
    max_frame: usize,
    sample_rate: usize,
    /// Оценка шумового пола = низкий перцентиль по `level_hist` (см. `poll`).
    noise_rms: f32,
    /// История уровней (отсчёт ~раз в 100 мс) для перцентильной оценки шумового пола.
    level_hist: std::collections::VecDeque<f32>,
    /// Счётчик сэмплов до следующей записи уровня в историю.
    since_hist: usize,
    /// Троттлинг форс-демода очень длинного (непрерывно шумного) буфера.
    since_force: usize,
    /// Троттлинг диагностического лога уровня захвата.
    since_log: usize,
    // --- отладочные счётчики/уровни (для UI и логов) ---
    stat_rms: f32,
    stat_peak: f32,
    stat_gate: f32,
    frames_ok: u32,
    frames_bad: u32,
    last_snr_db: f32,
    last_event: &'static str,
}

impl RxDemodulator {
    /// Демодуляторы строятся на ПРИЁМНОЙ полосе (полоса передачи пира).
    pub fn new(scheme: &dyn DuplexScheme) -> Self {
        let band = scheme.rx_band();
        let sr = scheme.sample_rate();
        let modems = build_modems(band, sr);
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
            max_frame,
            sample_rate: sr as usize,
            noise_rms: 0.0,
            level_hist: std::collections::VecDeque::with_capacity(LEVEL_HIST_LEN),
            since_hist: 0,
            since_force: 0,
            since_log: 0,
            stat_rms: 0.0,
            stat_peak: 0.0,
            stat_gate: 0.006,
            frames_ok: 0,
            frames_bad: 0,
            last_snr_db: 0.0,
            last_event: "ожидание",
        }
    }

    /// Живой снимок уровней/счётчиков для отладки в UI.
    pub fn stats(&self) -> RxStats {
        RxStats {
            rms: self.stat_rms,
            peak: self.stat_peak,
            noise_floor: self.noise_rms,
            gate: self.stat_gate,
            buffer_secs: self.buf.len() as f32 / self.sample_rate.max(1) as f32,
            in_burst: !self.buf.is_empty() && self.stat_rms > self.stat_gate,
            frames_ok: self.frames_ok,
            frames_bad: self.frames_bad,
            last_snr_db: self.last_snr_db,
            last_event: self.last_event,
        }
    }

    /// Сбрасывает накопленный приёмный буфер и reference. Вызывается в полудуплексе при
    /// переходе «передача → приём»: за время своей передачи микрофон писал собственное эхо,
    /// и его остатки нельзя скармливать демодулятору. Шумовой пол/счётчики сохраняются.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.reference.clear();
        self.since_force = 0;
    }

    /// Кладёт воспроизводимые сэмплы в reference-кольцо (для AEC-шва).
    pub fn push_reference(&mut self, played: &[f32]) {
        for &s in played {
            self.reference.push_back(s);
        }
        while self.reference.len() > self.max_frame * 2 {
            self.reference.pop_front();
        }
    }

    /// Кладёт захваченные с микрофона сэмплы; прогоняет через эхоподавитель (шов).
    pub fn push_captured(&mut self, captured: &[f32]) {
        let mut block = captured.to_vec();
        let mut refs: Vec<f32> = Vec::with_capacity(block.len());
        for _ in 0..block.len() {
            refs.push(self.reference.pop_front().unwrap_or(0.0));
        }
        self.canceller.process(&mut block, &refs);
        self.buf.extend_from_slice(&block);
        self.since_force += block.len();
        self.since_hist += block.len();
        self.since_log += block.len();
    }

    /// RMS хвоста длиной `n` сэмплов.
    fn tail_rms(&self, n: usize) -> (f32, f32) {
        let n = n.max(1).min(self.buf.len());
        if n == 0 {
            return (0.0, 0.0);
        }
        let tail = &self.buf[self.buf.len() - n..];
        let rms = (tail.iter().map(|x| x * x).sum::<f32>() / n as f32).sqrt();
        let peak = tail.iter().fold(0.0f32, |a, &x| a.max(x.abs()));
        (rms, peak)
    }

    /// Есть ли где-нибудь в буфере всплеск выше гейта (без перекрытия окон — дёшево).
    fn buf_has_burst(&self, gate: f32) -> bool {
        let win = (self.sample_rate / 20).max(1); // 50 мс
        if self.buf.len() < win {
            return false;
        }
        let mut i = 0;
        while i + win <= self.buf.len() {
            let e: f32 = self.buf[i..i + win].iter().map(|x| x * x).sum::<f32>() / win as f32;
            if e.sqrt() > gate {
                return true;
            }
            i += win;
        }
        false
    }

    /// Извлекает кадры из буфера, сегментируя поток по паузам (см. док к типу).
    pub fn poll(&mut self) -> Vec<RxEvent> {
        let mut out = Vec::new();
        let sr = self.sample_rate;
        if self.buf.is_empty() {
            return out;
        }

        let (recent, peak) = self.tail_rms(sr / 8); // хвост ~125 мс для детекции паузы

        // ШУМОВОЙ ПОЛ — НИЗКИЙ ПЕРЦЕНТИЛЬ ПО ДЛИННОМУ ОКНУ (~20 c), а не EWMA.
        //
        // Почему не EWMA: прежняя версия подтягивала пол вверх к текущему уровню и во время
        // ПЕРЕДАЧИ тоже. За длинный кадр (CSS — секунды) пол успевал догнать уровень сигнала,
        // гейт (кратный полу) вылезал ВЫШЕ сигнала — и приём вставал намертво: «сначала пару
        // секунд ловит, потом сколько ни прибавляй громкость — тишина». Классический
        // самозапирающийся AGC.
        //
        // Перцентиль устойчив по построению: всплески сигнала — это верхние перцентили окна, а
        // 10-й перцентиль отражает именно паузы между кадрами (в полудуплексе они всегда есть).
        // При этом в реально шумной комнате всё распределение уезжает вверх — и пол честно
        // поднимется. Окно (~20 c) заведомо длиннее самого длинного кадра, поэтому даже
        // семисекундный CSS-кадр не может им завладеть.
        if self.since_hist >= sr / 10 {
            self.since_hist = 0;
            self.level_hist.push_back(recent);
            while self.level_hist.len() > LEVEL_HIST_LEN {
                self.level_hist.pop_front();
            }
            let mut v: Vec<f32> = self.level_hist.iter().copied().collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let idx = (v.len() / 10).min(v.len().saturating_sub(1));
            // Абсолютный потолок пола — страховка «никогда не запереть себя»: как бы ни
            // складывалась оценка, гейт не поднимется выше уровня, который заведомо перекрывает
            // сигнал близкого устройства.
            self.noise_rms = v[idx].min(NOISE_FLOOR_CAP);
        }
        // Гейт: выше шумового пола, но с разумным абсолютным минимумом.
        let gate = (self.noise_rms * 2.5).max(0.006);
        self.stat_rms = recent;
        self.stat_peak = peak;
        self.stat_gate = gate;

        let tail_hot = recent > gate;

        if self.since_log >= sr {
            self.since_log = 0;
            eprintln!(
                "sonic-rx: rms={recent:.4} пик={peak:.3} пол={:.4} гейт={gate:.4} буфер={:.1}c {} (ok={} bad={})",
                self.noise_rms,
                self.buf.len() as f32 / sr as f32,
                if tail_hot { "[СИГНАЛ]" } else { "[тихо]" },
                self.frames_ok,
                self.frames_bad
            );
        }

        if tail_hot {
            // Идёт всплеск — копим. Защита от «бесконечного» всплеска (непрерывный шум/очень
            // длинный кадр): раз в ~0.4 c пробуем декодировать, если буфер перерос макс. кадр.
            if self.buf.len() > self.max_frame + sr / 2 && self.since_force >= sr * 2 / 5 {
                self.since_force = 0;
                if let Some(ev) = self.try_decode() {
                    self.record_ok(&ev);
                    out.push(ev);
                    self.buf.clear();
                } else {
                    // Не декодировалось — сдвигаем окно вперёд, чтобы не переть тот же префикс.
                    let keep = self.max_frame;
                    let drop = self.buf.len().saturating_sub(keep);
                    self.buf.drain(0..drop);
                }
            }
            return out;
        }

        // Хвост тихий. Если в буфере был всплеск — это конец кадра, декодируем и очищаем.
        if self.buf_has_burst(gate) {
            if let Some(ev) = self.try_decode() {
                self.record_ok(&ev);
                out.push(ev);
            }
            self.buf.clear();
        } else {
            // Чистая тишина — держим короткий lead-in, не копим тишину бесконечно.
            let keep = (sr / 3).min(self.buf.len());
            let drop = self.buf.len() - keep;
            if drop > 0 {
                self.buf.drain(0..drop);
            }
        }
        out
    }

    fn record_ok(&mut self, ev: &RxEvent) {
        self.frames_ok += 1;
        self.last_snr_db = ev.snr_db;
        self.last_event = "кадр принят";
        eprintln!(
            "sonic-rx: ✓ кадр {:?} {}Б SNR {:.1}дБ (всего ok={})",
            ev.mode,
            ev.bytes.len(),
            ev.snr_db,
            self.frames_ok
        );
    }

    /// Пробует все модемы на текущем буфере; принимает только кадр, прошедший CRC. Логирует
    /// причину неудачи (нет преамбулы vs преамбула есть, но CRC битый) — для отладки на железе.
    fn try_decode(&mut self) -> Option<RxEvent> {
        let mut detected_bad = false;
        for m in &self.modems {
            if let Some(d) = m.demodulate(&self.buf) {
                // CRC снимает неоднозначность между модемами (напр. OFDM-QPSK/16QAM — общая
                // преамбула): неверная модуляция → мусорные байты → CRC не сойдётся.
                if Frame::parse(&d.bytes).is_ok() {
                    return Some(RxEvent {
                        bytes: d.bytes,
                        snr_db: d.snr_db,
                        mode: m.mode(),
                    });
                }
                detected_bad = true;
            }
        }
        let secs = self.buf.len() as f32 / self.sample_rate as f32;
        self.last_event = if detected_bad {
            "синхронизация есть, CRC битый"
        } else {
            "преамбула не найдена"
        };
        if detected_bad {
            self.frames_bad += 1;
            eprintln!(
                "sonic-rx: ✗ синхронизация есть, но CRC битый — низкий SNR/искажения (всплеск {secs:.2}c, пик {:.3})",
                self.stat_peak
            );
        } else {
            eprintln!(
                "sonic-rx: ✗ преамбула не поймана во всплеске {secs:.2}c (пик {:.3}, гейт {:.4})",
                self.stat_peak, self.stat_gate
            );
        }
        None
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
    use sonic_protocol::bandplan::{Profile, Role, Tdd};
    use sonic_protocol::framing::{Frame, FrameHeader, FrameType};

    /// Полудуплекс: обе стороны в одной общей полосе, различаются лишь ролью/направлением.
    fn peer_schemes() -> (Tdd, Tdd) {
        (
            Tdd::new(Role::Initiator, Profile::Audible),
            Tdd::new(Role::Responder, Profile::Audible),
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

        // Эмулируем захват потоком: тишина, кадр по кускам, затем ПАУЗА (>125 мс) — по ней
        // приёмник понимает, что всплеск-кадр завершён (полудуплекс: между кадрами всегда тихо).
        rx.push_captured(&vec![0.0f32; 3000]);
        for chunk in samples.chunks(4096) {
            rx.push_captured(chunk);
        }
        rx.push_captured(&vec![0.0f32; 16000]);

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
        rx.push_captured(&vec![0.0f32; 16000]);

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

    /// Скармливает сэмплы приёмнику мелкими кусками с постоянным опросом — как в реальном
    /// потоке; собирает декодированные полезные нагрузки.
    fn feed(rx: &mut RxDemodulator, samples: &[f32], out: &mut Vec<String>) {
        for chunk in samples.chunks(1024) {
            rx.push_captured(chunk);
            for ev in rx.poll() {
                if let Ok(f) = Frame::parse(&ev.bytes) {
                    out.push(String::from_utf8_lossy(&f.payload).into_owned());
                }
            }
        }
    }

    #[test]
    fn streaming_decodes_successive_frames_without_getting_stuck() {
        // Ключевая регрессия: реальный поток идёт мелкими кусками, poll крутится постоянно,
        // кадры разделены паузами. Раньше буфер рос и приёмник цеплялся за застрявший префикс
        // → второй (и часто первый) кадр не декодировался. Проверяем, что ДВА кадра подряд
        // проходят — значит forward progress есть, застрявшего буфера нет.
        let (initiator, responder) = peer_schemes();
        let tx = Transmitter::new(&initiator);
        let mut rx = RxDemodulator::new(&responder);
        let dir = initiator.role().direction_bit();
        let silence = vec![0.0f32; 24_000]; // 0.5 c паузы между кадрами

        let mut decoded: Vec<String> = Vec::new();
        for msg in ["first frame here", "second frame here"] {
            let frame = Frame::new(
                FrameHeader::new(PhyMode::OfdmQpsk, FrameType::Data, dir),
                msg.as_bytes().to_vec(),
            );
            let sig = tx.modulate(PhyMode::OfdmQpsk, &frame.serialize());
            feed(&mut rx, &silence, &mut decoded);
            feed(&mut rx, &sig, &mut decoded);
            feed(&mut rx, &silence, &mut decoded);
        }

        assert!(
            decoded.iter().any(|s| s == "first frame here"),
            "первый кадр не декодирован: {decoded:?}"
        );
        assert!(
            decoded.iter().any(|s| s == "second frame here"),
            "второй кадр не декодирован — застрявший буфер: {decoded:?}"
        );
    }

    #[test]
    fn gate_does_not_creep_above_signal_over_long_session() {
        // Регрессия на «сначала пару секунд ловит, а потом сколько ни прибавляй громкость —
        // тишина». Причина была в EWMA-шумовом поле: он подтягивался вверх и ВО ВРЕМЯ сигнала,
        // за длинный кадр догонял его уровень, гейт уезжал ВЫШЕ сигнала и запирал приём.
        // Гоняем длинную серию кадров и требуем, чтобы ПОСЛЕДНИЕ принимались так же, как первые.
        let (initiator, responder) = peer_schemes();
        let tx = Transmitter::new(&initiator);
        let mut rx = RxDemodulator::new(&responder);
        let dir = initiator.role().direction_bit();
        let silence = vec![0.0f32; 24_000]; // 0.5 c паузы

        // Берём MFSK: его кадр звучит ~1.5 c. Это принципиально — прежняя EWMA-оценка
        // подтягивалась к текущему уровню с постоянной ~0.5 c, поэтому уже после ~0.26 c
        // НЕПРЕРЫВНОГО сигнала гейт перерастал сам сигнал. На коротком кадре баг не проявлялся,
        // на длинном — приём запирался намертво.
        const ROUNDS: usize = 6;
        let mut decoded: Vec<String> = Vec::new();
        for i in 0..ROUNDS {
            let msg = format!("frame number {i}");
            let frame = Frame::new(
                FrameHeader::new(PhyMode::Mfsk, FrameType::Data, dir),
                msg.clone().into_bytes(),
            );
            let sig = tx.modulate(PhyMode::Mfsk, &frame.serialize());
            feed(&mut rx, &silence, &mut decoded);
            feed(&mut rx, &sig, &mut decoded);
            feed(&mut rx, &silence, &mut decoded);
        }

        let last = format!("frame number {}", ROUNDS - 1);
        assert!(
            decoded.iter().any(|s| s == &last),
            "последний кадр не принят — гейт запер приём (принято {}/{ROUNDS}): {decoded:?}",
            decoded.len()
        );
        assert_eq!(
            decoded.len(),
            ROUNDS,
            "часть кадров потеряна по ходу сессии: {decoded:?}"
        );

        // Главный инвариант: гейт остаётся у уровня ТИШИНЫ и не подтягивается к уровню сигнала
        // (у передаваемого кадра RMS ~0.2+). Именно нарушение этого запирало приём.
        let st = rx.stats();
        assert!(
            st.gate < 0.05,
            "гейт уехал к уровню сигнала: гейт={:.4}, пол={:.4}",
            st.gate,
            st.noise_floor
        );
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
        rx.push_captured(&vec![0.0f32; 16000]);
        let events = rx.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(Frame::parse(&events[0].bytes).unwrap(), frame);
    }
}
