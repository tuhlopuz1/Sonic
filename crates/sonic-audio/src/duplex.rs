//! cpal duplex-движок: одновременные input+output потоки поверх [`RxDemodulator`] /
//! [`Transmitter`] (PROTOCOL.md §11.2).
//!
//! cpal `Stream` не `Send`, поэтому потоки живут на выделенном аудио-потоке, который ими
//! владеет; DSP крутится там же, а колбэки cpal (на своих аудио-нитях) трогают только
//! lock-free кольца (`rtrb`) — никаких аллокаций/блокировок в реальном времени
//! (PROTOCOL.md §11.2). Наружу отдаётся `Send`-хендл с каналами команд/событий.

use crate::device::default_io_config;
use crate::pipeline::{RxDemodulator, RxEvent, Transmitter};
use cpal::traits::{DeviceTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};
use sonic_protocol::bandplan::{DuplexScheme, Fdd, Profile, Role};
use sonic_protocol::framing::PhyMode;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    pub profile: Profile,
    pub role: Role,
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
    let scheme = Fdd::new(cfg.role, cfg.profile);
    let target_rate = scheme.sample_rate();

    let io = match default_io_config(target_rate) {
        Ok(io) => io,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };
    let sr = io.input_config.sample_rate().0;

    // Кольца: микрофон → обработка, обработка → динамик. Ёмкость ~1 c.
    let (mut mic_prod, mut mic_cons) = rtrb::RingBuffer::<f32>::new(sr as usize);
    let (mut spk_prod, mut spk_cons) = rtrb::RingBuffer::<f32>::new(sr as usize);

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

    // Схема, построенная под реальную частоту устройства (если она не совпала с целью).
    let scheme = FddAt { inner: scheme, sr };
    let mut rx = RxDemodulator::new(&scheme);
    let tx = Transmitter::new(&scheme);
    // Очередь исходящих сэмплов, которые постепенно скармливаются в кольцо динамика.
    let mut tx_pending: std::collections::VecDeque<f32> = std::collections::VecDeque::new();

    let _ = init_tx.send(Ok(sr));

    let mut capture = vec![0.0f32; 4096];
    loop {
        // 1. Команды.
        match cmd_rx.try_recv() {
            Ok(EngineCommand::Stop) | Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            Ok(EngineCommand::Send { mode, bytes }) => {
                let samples = tx.modulate(mode, &bytes);
                tx_pending.extend(samples);
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
        }

        // 2. Забираем захваченные сэмплы в приёмный конвейер.
        let mut got = 0;
        while let Ok(s) = mic_cons.pop() {
            capture[got] = s;
            got += 1;
            if got == capture.len() {
                rx.push_captured(&capture[..got]);
                got = 0;
            }
        }
        if got > 0 {
            rx.push_captured(&capture[..got]);
        }

        // 3. Демодуляция → события наверх.
        for ev in rx.poll() {
            if evt_tx.send(ev).is_err() {
                break;
            }
        }

        // 4. Скармливаем исходящие сэмплы в кольцо динамика (+ отвод reference для AEC).
        while spk_prod.slots() > 0 {
            let s = match tx_pending.pop_front() {
                Some(s) => s,
                None => break,
            };
            rx.push_reference(&[s]);
            if spk_prod.push(s).is_err() {
                break;
            }
        }

        std::thread::sleep(Duration::from_millis(5));
    }

    drop(in_stream);
    drop(out_stream);
}

/// Обёртка `Fdd` с реальной частотой устройства (устройство может не дать ровно 48 кГц).
#[derive(Clone, Copy)]
struct FddAt {
    inner: Fdd,
    sr: u32,
}

impl sonic_protocol::bandplan::DuplexScheme for FddAt {
    fn tx_band(&self) -> sonic_protocol::bandplan::SubBand {
        self.inner.tx_band()
    }
    fn rx_band(&self) -> sonic_protocol::bandplan::SubBand {
        self.inner.rx_band()
    }
    fn sample_rate(&self) -> u32 {
        self.sr
    }
    fn role(&self) -> Role {
        self.inner.role()
    }
    fn echo_canceller(&self) -> Box<dyn sonic_protocol::bandplan::EchoCanceller> {
        self.inner.echo_canceller()
    }
}

fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut on_mono: impl FnMut(f32) + Send + 'static,
) -> Result<cpal::Stream, String> {
    let channels = config.channels().max(1) as usize;
    let stream_config = config.config();
    let err_fn = |e| eprintln!("sonic-audio: input stream error: {e}");

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    for frame in data.chunks(channels) {
                        on_mono(frame.iter().sum::<f32>() / channels as f32);
                    }
                }));
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    for frame in data.chunks(channels) {
                        let m = frame.iter().map(|&s| s as f32 / i16::MAX as f32).sum::<f32>()
                            / channels as f32;
                        on_mono(m);
                    }
                }));
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            move |data: &[u16], _| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    for frame in data.chunks(channels) {
                        let m = frame
                            .iter()
                            .map(|&s| (s as f32 - 32768.0) / 32768.0)
                            .sum::<f32>()
                            / channels as f32;
                        on_mono(m);
                    }
                }));
            },
            err_fn,
            None,
        ),
        other => return Err(format!("Неподдерживаемый формат микрофона: {other:?}")),
    }
    .map_err(|e| format!("Открытие потока записи: {e}"))?;
    Ok(stream)
}

fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut next_sample: impl FnMut() -> f32 + Send + 'static,
) -> Result<cpal::Stream, String> {
    let channels = config.channels().max(1) as usize;
    let stream_config = config.config();
    let err_fn = |e| eprintln!("sonic-audio: output stream error: {e}");

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            &stream_config,
            move |data: &mut [f32], _| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    for frame in data.chunks_mut(channels) {
                        let s = next_sample();
                        for c in frame.iter_mut() {
                            *c = s;
                        }
                    }
                }));
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            &stream_config,
            move |data: &mut [i16], _| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    for frame in data.chunks_mut(channels) {
                        let v = (next_sample().clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        for c in frame.iter_mut() {
                            *c = v;
                        }
                    }
                }));
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_output_stream(
            &stream_config,
            move |data: &mut [u16], _| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    for frame in data.chunks_mut(channels) {
                        let u = ((next_sample().clamp(-1.0, 1.0) * 32768.0) + 32768.0) as u16;
                        for c in frame.iter_mut() {
                            *c = u;
                        }
                    }
                }));
            },
            err_fn,
            None,
        ),
        other => return Err(format!("Неподдерживаемый формат динамика: {other:?}")),
    }
    .map_err(|e| format!("Открытие потока воспроизведения: {e}"))?;
    Ok(stream)
}
