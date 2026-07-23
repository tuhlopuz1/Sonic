//! cpal duplex-движок: одновременные input+output потоки поверх [`RxDemodulator`] /
//! [`Transmitter`] (PROTOCOL.md §11.2).
//!
//! cpal `Stream` не `Send`, поэтому потоки живут на выделенном аудио-потоке, который ими
//! владеет; DSP крутится там же, а колбэки cpal (на своих аудио-нитях) трогают только
//! lock-free кольца (`rtrb`) — никаких аллокаций/блокировок в реальном времени
//! (PROTOCOL.md §11.2). Наружу отдаётся `Send`-хендл с каналами команд/событий.
//!
//! Частоты: весь протокольный DSP идёт на КАНОНИЧЕСКОЙ частоте профиля (48 кГц), а
//! микрофон и динамик могут работать каждый на своей (в shared-режиме WASAPI они
//! залочены настройками ОС и часто не совпадают). Приведение — [`crate::resample`]:
//! микрофон → канон на входе, канон → динамик на выходе.

use crate::device::io_config;
use crate::pipeline::{RxDemodulator, RxEvent, Transmitter};
use crate::resample::Resampler;
use crate::streams::{build_input_stream, build_output_stream};
use cpal::traits::StreamTrait;
use crossbeam_channel::{Receiver, Sender};
use sonic_protocol::bandplan::{DuplexScheme, Profile, Role, Tdd};
use sonic_protocol::framing::PhyMode;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub profile: Profile,
    pub role: Role,
    /// Имя микрофона; `None`/пусто — системный по умолчанию.
    pub input_device: Option<String>,
    /// Имя динамика; `None`/пусто — системный по умолчанию.
    pub output_device: Option<String>,
}

/// Команда аудио-потоку.
enum EngineCommand {
    Send { mode: PhyMode, bytes: Vec<u8> },
    Stop,
}

/// `Send`-хендл движка: канал команд + join. События приёма идут в переданный `evt_tx`.
pub struct DuplexEngine {
    cmd_tx: Sender<EngineCommand>,
    handle: Option<JoinHandle<()>>,
    /// Каноническая частота DSP (не обязательно частота железа).
    pub sample_rate: u32,
}

impl DuplexEngine {
    /// Запускает дуплекс. Ошибки открытия устройств прокидываются синхронно.
    pub fn start(cfg: EngineConfig, evt_tx: Sender<RxEvent>) -> Result<DuplexEngine, String> {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (init_tx, init_rx) = mpsc::channel::<Result<u32, String>>();

        let handle = std::thread::Builder::new()
            .name("sonic-audio-duplex".into())
            .spawn(move || run_engine(cfg, cmd_rx, evt_tx, init_tx))
            .map_err(|e| format!("spawn audio thread: {e}"))?;

        match init_rx.recv() {
            Ok(Ok(sample_rate)) => Ok(DuplexEngine {
                cmd_tx,
                handle: Some(handle),
                sample_rate,
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => Err("Аудио-поток завершился до инициализации".into()),
        }
    }

    /// Ставит кадр в очередь на передачу в указанном режиме.
    pub fn send_frame(&self, mode: PhyMode, bytes: Vec<u8>) -> Result<(), String> {
        self.cmd_tx
            .send(EngineCommand::Send { mode, bytes })
            .map_err(|_| "Движок остановлен".into())
    }
}

impl Drop for DuplexEngine {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(EngineCommand::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run_engine(
    cfg: EngineConfig,
    cmd_rx: Receiver<EngineCommand>,
    evt_tx: Sender<RxEvent>,
    init_tx: mpsc::Sender<Result<u32, String>>,
) {
    let scheme = Tdd::new(cfg.role, cfg.profile);
    // Каноническая частота протокола — на ней работают все модемы, независимо от железа.
    let dsp_rate = scheme.sample_rate();

    let io = match io_config(
        dsp_rate,
        cfg.input_device.as_deref(),
        cfg.output_device.as_deref(),
    ) {
        Ok(io) => io,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };
    let in_rate = io.input_rate();
    let out_rate = io.output_rate();

    // Диагностика: на проблемном железе сразу видно реальные форматы/частоты.
    eprintln!(
        "sonic-audio: DSP {dsp_rate} Гц | вход {:?} {in_rate} Гц ({} кан.) | выход {:?} {out_rate} Гц ({} кан.)",
        io.input_config.sample_format(),
        io.input_config.channels(),
        io.output_config.sample_format(),
        io.output_config.channels(),
    );

    // Кольца: микрофон → обработка, обработка → динамик. Ёмкость ~1 c на своей частоте.
    let (mut mic_prod, mut mic_cons) = rtrb::RingBuffer::<f32>::new(in_rate as usize);
    let (mut spk_prod, mut spk_cons) = rtrb::RingBuffer::<f32>::new(out_rate as usize);

    let in_stream = match build_input_stream(&io.input_device, &io.input_config, move |mono| {
        let _ = mic_prod.push(mono);
    }) {
        Ok(s) => s,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };
    let out_stream = match build_output_stream(&io.output_device, &io.output_config, move || {
        spk_cons.pop().unwrap_or(0.0)
    }) {
        Ok(s) => s,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };
    if let Err(e) = in_stream.play().map_err(|e| format!("play input: {e}")) {
        let _ = init_tx.send(Err(e));
        return;
    }
    if let Err(e) = out_stream.play().map_err(|e| format!("play output: {e}")) {
        let _ = init_tx.send(Err(e));
        return;
    }

    let mut rx = RxDemodulator::new(&scheme);
    let tx = Transmitter::new(&scheme);
    // Микрофон → каноническая частота (состояние сохраняется между блоками).
    let mut in_resampler = Resampler::new(in_rate, dsp_rate);
    // Очередь исходящих сэмплов (уже в частоте динамика).
    let mut tx_pending: std::collections::VecDeque<f32> = std::collections::VecDeque::new();

    let _ = init_tx.send(Ok(dsp_rate));

    // Полудуплекс (TDD): пока мы передаём, микрофон слышит в основном СВОЙ сигнал, поэтому
    // приём на это время глушится (иначе демодулятор давился бы собственным эхом, а буфер
    // забивался бы им к моменту ответа пира). `tx_busy_until` — момент, когда доиграет всё
    // поставленное в очередь аудио (по суммарной длительности, независимо от буферизации
    // колец); плюс хвост на реверберацию/задержку тракта. `TX_TAIL` — этот хвост.
    const TX_TAIL: Duration = Duration::from_millis(220);
    let mut tx_busy_until = Instant::now();
    let mut was_gated = false;

    let mut raw = Vec::with_capacity(8192);
    let mut resampled = Vec::with_capacity(8192);
    loop {
        // 1. Команды.
        match cmd_rx.try_recv() {
            Ok(EngineCommand::Stop) | Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            Ok(EngineCommand::Send { mode, bytes }) => {
                let samples = tx.modulate(mode, &bytes);
                // Отвод AEC-reference — в канонической частоте, в одном домене с приёмом.
                rx.push_reference(&samples);
                // Кадр самодостаточен по синхронизации, поэтому ресемплим его целиком
                // отдельным ресемплером (состояние между кадрами не нужно).
                let played = if out_rate == dsp_rate {
                    samples
                } else {
                    Resampler::new(dsp_rate, out_rate).process_all(&samples)
                };
                // Продлеваем окно передачи на длительность этого кадра (от текущего конца
                // очереди или от «сейчас», если очередь уже отыграла).
                let dur = Duration::from_secs_f64(played.len() as f64 / out_rate.max(1) as f64);
                tx_busy_until = tx_busy_until.max(Instant::now()) + dur;
                tx_pending.extend(played);
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
        }

        // 2. Захваченные сэмплы → каноническая частота. Кольцо микрофона осушаем ВСЕГДА
        //    (иначе переполнится), но во время своей передачи вход в демодулятор не подаём.
        let tx_active = Instant::now() < tx_busy_until + TX_TAIL;
        raw.clear();
        while let Ok(s) = mic_cons.pop() {
            raw.push(s);
        }
        if !raw.is_empty() {
            resampled.clear();
            in_resampler.process(&raw, &mut resampled); // ресемплер крутим всегда — не рвём фазу
            if !tx_active {
                // Переход «передача → приём»: один раз чистим буфер от остатков эха.
                if was_gated {
                    rx.clear();
                    was_gated = false;
                }
                rx.push_captured(&resampled);
            } else {
                was_gated = true;
            }
        }

        // 3. Демодуляция → события наверх (во время своей передачи не демодулируем).
        if !tx_active {
            for ev in rx.poll() {
                if evt_tx.send(ev).is_err() {
                    break;
                }
            }
        }

        // 4. Скармливаем исходящие сэмплы в кольцо динамика.
        while spk_prod.slots() > 0 {
            let s = match tx_pending.pop_front() {
                Some(s) => s,
                None => break,
            };
            if spk_prod.push(s).is_err() {
                break;
            }
        }

        std::thread::sleep(Duration::from_millis(5));
    }

    drop(in_stream);
    drop(out_stream);
}
