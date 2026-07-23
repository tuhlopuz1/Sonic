//! Сессия мессенджера: связывает модем/аудио (`sonic-audio`) с MAC/ARQ
//! (`sonic-protocol::arq`) и приложением. Живёт в отдельном потоке; общение — через
//! каналы. Здесь реализованы уровни APP/MAC поверх PHY (ср. PROTOCOL.md §1, §7, §9).
//!
//! Что делает поток сессии:
//! - режет исходящее сообщение на чанки, кладёт под FEC, шлёт кадрами с ARQ-окном;
//! - принимает кадры, фильтрует своё эхо по direction, декодирует FEC, собирает чанки;
//! - шлёт ACK встречным потоком, обрабатывает входящие ACK, ретрансмитит по таймауту;
//! - ведёт auto-fallback режима по деградации канала и телеметрию (событие link-quality).
//!
//! Осознанные упрощения относительно PROTOCOL.md (не заглушки — явные решения под
//! срок, см. отчёт): Noise-handshake/шифрование/SAS (§8) не реализованы — сообщения
//! идут открыто; ACK ходят в текущем режиме, а не строго в CSS (иначе латентность ACK
//! на 50 бит/с убивает throughput). Auto стартует в OFDM-QPSK (нет хрупкой фазы
//! хендшейка, которую §9 защищает стартом в CSS).

use crate::events;
use serde::Serialize;
use sonic_audio::{DuplexEngine, EngineConfig, RxEvent};
use sonic_protocol::arq::{ArqConfig, ArqReceiver, ArqSender, AutoFallback};
use sonic_protocol::bandplan::{DuplexScheme, Fdd};
use sonic_protocol::fec::FecCodec;
use sonic_protocol::framing::{Frame, FrameHeader, FrameType, PhyMode};
use sonic_protocol::modem::qam::Modulation;
use sonic_protocol::modem::{CssModem, MfskModem, Modem, OfdmModem};
use sonic_protocol::telemetry::LinkQuality;
use sonic_protocol::{Profile, Role};
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

/// Максимум байт текста в одном кадре — длинные сообщения режутся на чанки. 28 подобрано
/// так, чтобы внутренний чанк (4 служебных + текст) укладывался ровно в ОДИН FEC-блок
/// (k=32): один блок = самый короткий кадр в эфире, что особенно важно для медленных
/// режимов (CSS/MFSK), где каждый лишний блок — это лишние секунды передачи.
const CHUNK_LEN: usize = 28;
/// Геометрия FEC полезной нагрузки (RS(48,32) на блок, t=8).
const FEC_K: usize = 32;
const FEC_NSYM: usize = 16;

/// Сколько миллисекунд ОДИН кадр данных этого режима реально звучит в эфире (для
/// airtime-осознанного ARQ-таймаута). Считаем по фактическим параметрам модема, поэтому
/// оценка остаётся верной, если параметры поменяются.
fn frame_airtime_ms(scheme: &Fdd, mode: PhyMode, payload_len: usize) -> f32 {
    let band = scheme.tx_band();
    let sr = scheme.sample_rate();
    let samples = match mode {
        PhyMode::Css => CssModem::with_defaults(band, sr).frame_samples(payload_len),
        PhyMode::Mfsk => MfskModem::new(band, sr).frame_samples(payload_len),
        PhyMode::OfdmQpsk => OfdmModem::new(band, sr, Modulation::Qpsk).frame_samples(payload_len),
        PhyMode::Ofdm16Qam => OfdmModem::new(band, sr, Modulation::Qam16).frame_samples(payload_len),
    };
    samples as f32 * 1000.0 / sr as f32
}

/// Политика выбора режима из UI. Кроме Auto (авто-fallback по лестнице) — явный выбор
/// конкретной модуляции, чтобы пользователь мог принудительно проверить каждый режим.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModePolicy {
    Auto,
    Force(PhyMode),
}

/// Команды потоку сессии.
pub enum SessionCommand {
    Send(String),
    SetMode(ModePolicy),
    Stop,
}

/// Хендл сессии, живёт в состоянии Tauri.
pub struct SessionHandle {
    cmd_tx: mpsc::Sender<SessionCommand>,
}

impl SessionHandle {
    /// `input_device`/`output_device` — имена выбранных в UI устройств; `None` — системные.
    pub fn start(
        app: AppHandle,
        profile: Profile,
        role: Role,
        input_device: Option<String>,
        output_device: Option<String>,
    ) -> Result<SessionHandle, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = crossbeam_channel::unbounded::<RxEvent>();

        let engine = DuplexEngine::start(
            EngineConfig {
                profile,
                role,
                input_device,
                output_device,
            },
            evt_tx,
        )?;

        std::thread::Builder::new()
            .name("sonic-session".into())
            .spawn(move || session_loop(app, profile, role, engine, cmd_rx, evt_rx))
            .map_err(|e| format!("spawn session: {e}"))?;

        Ok(SessionHandle { cmd_tx })
    }

    pub fn send_message(&self, text: String) -> Result<(), String> {
        self.cmd_tx
            .send(SessionCommand::Send(text))
            .map_err(|_| "Сессия остановлена".into())
    }

    pub fn set_mode(&self, policy: ModePolicy) -> Result<(), String> {
        self.cmd_tx
            .send(SessionCommand::SetMode(policy))
            .map_err(|_| "Сессия остановлена".into())
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(SessionCommand::Stop);
    }
}

// --- события в UI ---

#[derive(Serialize, Clone)]
struct MessageReceivedEvent {
    text: String,
}

#[derive(Serialize, Clone)]
struct MessageStatusEvent {
    msg_id: u16,
    status: &'static str, // "sent" | "delivered"
    text: String,
}

#[derive(Serialize, Clone)]
struct LinkQualityEvent {
    snr_db: f32,
    mode: &'static str,
    retransmits: u32,
    rtt_ms: f32,
    frames_ok: u32,
    frames_bad: u32,
    per: f32,
    in_flight: usize,
}

#[derive(Serialize, Clone)]
struct SessionStateEvent {
    state: &'static str, // "up" | "down"
}

fn mode_label(mode: PhyMode) -> &'static str {
    mode.label()
}

/// Заготовка исходящего чанка (то, что арк хранит для ретрансмиссии — FEC-нагрузка).
struct PendingChunk {
    payload: Vec<u8>, // FEC-кодированный внутренний чанк
    msg_id: u16,
    text: String, // для события "delivered"
}

/// Сборщик чанков одного сообщения на приёме.
struct Reassembly {
    total: u8,
    chunks: HashMap<u8, Vec<u8>>,
}

fn session_loop(
    app: AppHandle,
    profile: Profile,
    role: Role,
    engine: DuplexEngine,
    cmd_rx: mpsc::Receiver<SessionCommand>,
    evt_rx: crossbeam_channel::Receiver<RxEvent>,
) {
    let fec = FecCodec::new(FEC_K, FEC_NSYM);
    let my_dir = role.direction_bit();
    let scheme = Fdd::new(role, profile);
    // Репрезентативный размер кадра данных (FEC-байт на полный чанк) для оценки airtime.
    let rep_payload = fec.encoded_len(4 + CHUNK_LEN);
    let airtime = move |mode| frame_airtime_ms(&scheme, mode, rep_payload);

    let mut fb = AutoFallback::new();
    fb.force(PhyMode::OfdmQpsk); // Auto стартует в быстром режиме (см. модульный комментарий)
    let mut policy = ModePolicy::Auto;

    let start_mode = current_mode(policy, &fb);
    let mut arq_tx = ArqSender::new(ArqConfig::for_mode(start_mode, 500.0, airtime(start_mode)));
    let mut arq_rx: ArqReceiver<Vec<u8>> = ArqReceiver::new();

    // seq → метаданные исходящего кадра (для события "delivered" и авто-fallback).
    let mut sent_meta: HashMap<u16, (u16, String)> = HashMap::new();
    let mut out_queue: std::collections::VecDeque<PendingChunk> = std::collections::VecDeque::new();
    let mut reassembly: HashMap<u16, Reassembly> = HashMap::new();
    let mut next_msg_id: u16 = 1;

    let mut link = LinkQuality::default();
    let start = Instant::now();
    let now_ms = || start.elapsed().as_millis() as u64;

    let _ = app.emit(events::SESSION_STATE_CHANGED, SessionStateEvent { state: "up" });

    let mut last_tick = Instant::now();
    let mut last_telemetry = Instant::now();

    loop {
        // 1. Команды от UI.
        match cmd_rx.try_recv() {
            Ok(SessionCommand::Stop) | Err(mpsc::TryRecvError::Disconnected) => break,
            Ok(SessionCommand::Send(text)) => {
                enqueue_message(&fec, text, &mut next_msg_id, &mut out_queue, &app);
            }
            Ok(SessionCommand::SetMode(p)) => {
                policy = p;
                if let ModePolicy::Force(mode) = p {
                    fb.force(mode);
                }
                let m = current_mode(policy, &fb);
                arq_tx.set_config(ArqConfig::for_mode(m, link.rtt_ms.max(500.0), airtime(m)));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        // 2. Входящие кадры из эфира.
        while let Ok(ev) = evt_rx.try_recv() {
            let frame = match Frame::parse(&ev.bytes) {
                Ok(f) => f,
                Err(e) => {
                    // Кадр пойман модемом, но CRC/структура не сошлись — обычно низкий SNR.
                    eprintln!("sonic-session: битый кадр ({e:?}), {} байт", ev.bytes.len());
                    link.frames_bad += 1;
                    continue;
                }
            };
            // Фильтр собственного эха: свой direction игнорируем.
            if frame.header.direction == my_dir {
                continue;
            }
            eprintln!(
                "sonic-session: кадр {:?} seq={} от пира (SNR {:.1} дБ)",
                frame.header.frame_type, frame.header.seq, ev.snr_db
            );
            link.frames_ok += 1;
            link.snr_db = ev.snr_db;

            match frame.header.frame_type {
                FrameType::Data => {
                    handle_data(
                        &fec,
                        &frame,
                        &mut arq_rx,
                        &mut reassembly,
                        &app,
                        &engine,
                        my_dir,
                        current_mode(policy, &fb),
                    );
                }
                FrameType::Ack => {
                    let before = arq_tx.in_flight();
                    arq_tx.on_ack(frame.header.ack, frame.header.sack, now_ms());
                    if let Some(rtt) = arq_tx.take_rtt_sample() {
                        link.update_rtt(rtt);
                    }
                    let acked = before.saturating_sub(arq_tx.in_flight());
                    for _ in 0..acked {
                        if policy == ModePolicy::Auto && fb.on_success() {
                            apply_mode_change(&mut arq_tx, &fb, &link, policy, &airtime);
                        }
                    }
                    // Уведомляем UI о доставке подтверждённых кадров.
                    notify_delivered(&mut sent_meta, &arq_tx, &app);
                }
                _ => {}
            }
        }

        // 3. Отправляем из очереди, пока есть место в окне.
        while arq_tx.can_send() {
            let Some(chunk) = out_queue.pop_front() else { break };
            let mode = current_mode(policy, &fb);
            if let Some(seq) = arq_tx.send(chunk.payload.clone(), now_ms()) {
                sent_meta.insert(seq, (chunk.msg_id, chunk.text.clone()));
                transmit_frame(&engine, mode, FrameType::Data, my_dir, seq, &arq_rx, &chunk.payload);
            }
        }

        // 4. Периодика: ретрансмиссии, авто-fallback, телеметрия.
        if last_tick.elapsed() >= Duration::from_millis(200) {
            last_tick = Instant::now();
            let mode = current_mode(policy, &fb);
            let retx = arq_tx.due_retransmissions(now_ms());
            for (seq, payload) in &retx {
                transmit_frame(&engine, mode, FrameType::Data, my_dir, *seq, &arq_rx, payload);
            }
            if !retx.is_empty() && policy == ModePolicy::Auto && fb.on_failure() {
                apply_mode_change(&mut arq_tx, &fb, &link, policy, &airtime);
            }
            if arq_tx.link_down() {
                let _ = app.emit(events::SESSION_STATE_CHANGED, SessionStateEvent { state: "down" });
            }
        }

        if last_telemetry.elapsed() >= Duration::from_millis(400) {
            last_telemetry = Instant::now();
            link.mode = current_mode(policy, &fb);
            link.retransmits = arq_tx.retransmits();
            emit_link_quality(&app, &link, arq_tx.in_flight());
        }

        std::thread::sleep(Duration::from_millis(15));
    }

    let _ = app.emit(events::SESSION_STATE_CHANGED, SessionStateEvent { state: "down" });
}

fn current_mode(policy: ModePolicy, fb: &AutoFallback) -> PhyMode {
    match policy {
        ModePolicy::Force(mode) => mode,
        ModePolicy::Auto => fb.current(),
    }
}

fn apply_mode_change(
    arq_tx: &mut ArqSender,
    fb: &AutoFallback,
    link: &LinkQuality,
    policy: ModePolicy,
    airtime: &impl Fn(PhyMode) -> f32,
) {
    let m = current_mode(policy, fb);
    arq_tx.set_config(ArqConfig::for_mode(m, link.rtt_ms.max(500.0), airtime(m)));
}

/// Режет текст на чанки, кладёт под FEC, ставит в очередь отправки.
fn enqueue_message(
    fec: &FecCodec,
    text: String,
    next_msg_id: &mut u16,
    out_queue: &mut std::collections::VecDeque<PendingChunk>,
    app: &AppHandle,
) {
    let bytes = text.as_bytes();
    let total = bytes.len().div_ceil(CHUNK_LEN).max(1) as u8;
    let msg_id = *next_msg_id;
    *next_msg_id = next_msg_id.wrapping_add(1).max(1);

    for idx in 0..total {
        let start = idx as usize * CHUNK_LEN;
        let end = (start + CHUNK_LEN).min(bytes.len());
        let mut inner = Vec::with_capacity(4 + (end - start));
        inner.extend_from_slice(&msg_id.to_be_bytes());
        inner.push(total);
        inner.push(idx);
        inner.extend_from_slice(&bytes[start..end]);
        out_queue.push_back(PendingChunk {
            payload: fec.encode(&inner),
            msg_id,
            text: text.clone(),
        });
    }
    let _ = app.emit(
        events::MESSAGE_STATUS,
        MessageStatusEvent {
            msg_id,
            status: "sent",
            text,
        },
    );
}

/// Строит и передаёт один кадр (Data/Ack) с пиггибек-ACK из приёмника.
fn transmit_frame(
    engine: &DuplexEngine,
    mode: PhyMode,
    frame_type: FrameType,
    direction: u8,
    seq: u16,
    arq_rx: &ArqReceiver<Vec<u8>>,
    payload: &[u8],
) {
    let (cum, sack) = arq_rx.ack_info();
    let mut header = FrameHeader::new(mode, frame_type, direction);
    header.seq = seq;
    header.ack = cum;
    header.sack = sack;
    let frame = Frame::new(header, payload.to_vec());
    let _ = engine.send_frame(mode, frame.serialize());
}

#[allow(clippy::too_many_arguments)]
fn handle_data(
    fec: &FecCodec,
    frame: &Frame,
    arq_rx: &mut ArqReceiver<Vec<u8>>,
    reassembly: &mut HashMap<u16, Reassembly>,
    app: &AppHandle,
    engine: &DuplexEngine,
    my_dir: u8,
    mode: PhyMode,
) {
    // Переупорядочивание/дедуп на MAC-уровне.
    let delivered = arq_rx.push(frame.header.seq, frame.payload.clone());
    for payload in delivered {
        if let Ok(inner) = fec.decode(&payload) {
            reassemble(&inner, reassembly, app);
        }
    }
    // ACK встречным потоком (в текущем режиме — см. модульный комментарий).
    transmit_frame(engine, mode, FrameType::Ack, my_dir, 0, arq_rx, &[]);
}

/// Разбор внутреннего чанка и сборка сообщения; при полном наборе — событие в UI.
fn reassemble(inner: &[u8], reassembly: &mut HashMap<u16, Reassembly>, app: &AppHandle) {
    if inner.len() < 4 {
        return;
    }
    let msg_id = u16::from_be_bytes([inner[0], inner[1]]);
    let total = inner[2];
    let idx = inner[3];
    let text_bytes = inner[4..].to_vec();

    let entry = reassembly.entry(msg_id).or_insert_with(|| Reassembly {
        total,
        chunks: HashMap::new(),
    });
    entry.chunks.insert(idx, text_bytes);

    if entry.chunks.len() as u8 >= entry.total {
        let mut full = Vec::new();
        for i in 0..entry.total {
            if let Some(c) = entry.chunks.get(&i) {
                full.extend_from_slice(c);
            }
        }
        reassembly.remove(&msg_id);
        if let Ok(text) = String::from_utf8(full) {
            let _ = app.emit(events::MESSAGE_RECEIVED, MessageReceivedEvent { text });
        }
    }
}

fn notify_delivered(
    sent_meta: &mut HashMap<u16, (u16, String)>,
    arq_tx: &ArqSender,
    app: &AppHandle,
) {
    // Кадр считается доставленным, когда его больше нет в окне (подтверждён).
    let delivered: Vec<u16> = sent_meta
        .keys()
        .copied()
        .filter(|seq| !arq_tx.contains(*seq))
        .collect();
    for seq in delivered {
        if let Some((msg_id, text)) = sent_meta.remove(&seq) {
            let _ = app.emit(
                events::MESSAGE_STATUS,
                MessageStatusEvent {
                    msg_id,
                    status: "delivered",
                    text,
                },
            );
        }
    }
}

fn emit_link_quality(app: &AppHandle, link: &LinkQuality, in_flight: usize) {
    let _ = app.emit(
        events::LINK_QUALITY,
        LinkQualityEvent {
            snr_db: link.snr_db,
            mode: mode_label(link.mode),
            retransmits: link.retransmits,
            rtt_ms: link.rtt_ms,
            frames_ok: link.frames_ok,
            frames_bad: link.frames_bad,
            per: link.per(),
            in_flight,
        },
    );
}
