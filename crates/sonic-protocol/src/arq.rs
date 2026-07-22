//! MAC-уровень ARQ: скользящее окно, selective repeat + авто-fallback режима (PROTOCOL.md §7, §9).
//!
//! FDD даёт настоящий full-duplex: ACK идут встречным потоком в другой полосе, не
//! блокируя приём данных. Окно настраивается под режим (CSS — 4 кадра, OFDM — 32,
//! PROTOCOL.md §7.3). Время передаётся явным `now_ms` — так ARQ детерминированно
//! тестируется без реального таймера.
//!
//! Замечание о номерах кадров: используется u16 без обработки заворота в пределах
//! сессии (сессия не шлёт 65536 сообщений) — сознательное упрощение под мессенджер.

use crate::framing::PhyMode;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
pub struct ArqConfig {
    pub window: usize,
    pub timeout_ms: u64,
    pub max_retries: u32,
}

impl ArqConfig {
    /// Окно под режим: CSS медленный (окно 4), OFDM быстрый (окно 32) — PROTOCOL.md §7.3.
    pub fn for_mode(mode: PhyMode, rtt_ms: f32) -> Self {
        let window = if mode.is_ofdm() { 32 } else { 4 };
        ArqConfig {
            window,
            timeout_ms: (3.0 * rtt_ms).max(300.0) as u64, // таймаут = 3×RTT (§7.3)
            max_retries: 8, // после 8 неудач подряд — LINK_DOWN (§7.3)
        }
    }
}

struct Outstanding {
    payload: Vec<u8>,
    sent_at: u64,
    retries: u32,
}

/// Отправитель со скользящим окном.
pub struct ArqSender {
    cfg: ArqConfig,
    next_seq: u16,
    outstanding: HashMap<u16, Outstanding>,
    retransmits: u32,
    link_down: bool,
    last_rtt_sample: Option<f32>,
}

impl ArqSender {
    pub fn new(cfg: ArqConfig) -> Self {
        ArqSender {
            cfg,
            next_seq: 0,
            outstanding: HashMap::new(),
            retransmits: 0,
            link_down: false,
            last_rtt_sample: None,
        }
    }

    pub fn set_config(&mut self, cfg: ArqConfig) {
        self.cfg = cfg;
    }

    /// Есть ли место в окне для нового кадра.
    pub fn can_send(&self) -> bool {
        !self.link_down && self.outstanding.len() < self.cfg.window
    }

    /// Регистрирует новый кадр в окне, возвращает его seq (или None, если окно полно).
    pub fn send(&mut self, payload: Vec<u8>, now_ms: u64) -> Option<u16> {
        if !self.can_send() {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.outstanding.insert(
            seq,
            Outstanding {
                payload,
                sent_at: now_ms,
                retries: 0,
            },
        );
        Some(seq)
    }

    /// Обрабатывает подтверждение: кумулятивный ACK + 32-битный SACK следующих seq.
    /// Возвращает измеренные RTT-выборки подтверждённых кадров.
    pub fn on_ack(&mut self, cumulative: u16, sack: u32, now_ms: u64) {
        let mut acked: Vec<u16> = Vec::new();
        // Кумулятив: все seq <= cumulative (в пределах активного окна).
        let seqs: Vec<u16> = self.outstanding.keys().copied().collect();
        for seq in seqs {
            if seq_le(seq, cumulative) {
                acked.push(seq);
            } else {
                // Селективное: бит i маски = seq (cumulative + 1 + i).
                let offset = seq.wrapping_sub(cumulative.wrapping_add(1));
                if offset < 32 && (sack & (1 << offset)) != 0 {
                    acked.push(seq);
                }
            }
        }
        for seq in acked {
            if let Some(o) = self.outstanding.remove(&seq) {
                let sample = (now_ms.saturating_sub(o.sent_at)) as f32;
                self.last_rtt_sample = Some(sample);
            }
        }
    }

    /// Кадры, у которых истёк таймаут — их нужно переслать. Экспоненциальный backoff и
    /// подсчёт ретрансмиссий; после `max_retries` — LINK_DOWN (PROTOCOL.md §7.3).
    pub fn due_retransmissions(&mut self, now_ms: u64) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut down = false;
        for (&seq, o) in self.outstanding.iter_mut() {
            // Backoff: таймаут растёт вдвое с каждой ретрансмиссией (§7.3).
            let eff_timeout = self.cfg.timeout_ms.saturating_mul(1u64 << o.retries.min(6));
            if now_ms.saturating_sub(o.sent_at) >= eff_timeout {
                if o.retries >= self.cfg.max_retries {
                    down = true;
                    continue;
                }
                o.retries += 1;
                o.sent_at = now_ms;
                self.retransmits += 1;
                out.push((seq, o.payload.clone()));
            }
        }
        if down {
            self.link_down = true;
        }
        out
    }

    /// Ждёт ли ещё этот seq подтверждения (в окне).
    pub fn contains(&self, seq: u16) -> bool {
        self.outstanding.contains_key(&seq)
    }

    pub fn link_down(&self) -> bool {
        self.link_down
    }
    pub fn retransmits(&self) -> u32 {
        self.retransmits
    }
    pub fn in_flight(&self) -> usize {
        self.outstanding.len()
    }
    pub fn take_rtt_sample(&mut self) -> Option<f32> {
        self.last_rtt_sample.take()
    }
}

/// Приёмник с переупорядочиванием: отдаёт полезную нагрузку строго по порядку,
/// дедуплицирует и формирует данные для ACK (кумулятив + SACK).
pub struct ArqReceiver<T> {
    expected: u16,
    pending: HashMap<u16, T>,
    have: std::collections::HashSet<u16>,
}

impl<T> Default for ArqReceiver<T> {
    fn default() -> Self {
        ArqReceiver {
            expected: 0,
            pending: HashMap::new(),
            have: std::collections::HashSet::new(),
        }
    }
}

impl<T> ArqReceiver<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Принимает кадр (seq, элемент). Возвращает элементы, ставшие доступными по порядку.
    /// Повторы и уже доставленные seq игнорируются (дедуп).
    pub fn push(&mut self, seq: u16, item: T) -> Vec<T> {
        if seq_lt(seq, self.expected) || self.have.contains(&seq) {
            return Vec::new(); // дубликат/устаревший
        }
        self.have.insert(seq);
        self.pending.insert(seq, item);

        let mut delivered = Vec::new();
        while let Some(item) = self.pending.remove(&self.expected) {
            delivered.push(item);
            self.expected = self.expected.wrapping_add(1);
        }
        delivered
    }

    /// (кумулятивный ACK, SACK-маска). Кумулятив = последний доставленный по порядку seq.
    pub fn ack_info(&self) -> (u16, u32) {
        let cumulative = self.expected.wrapping_sub(1);
        let mut sack = 0u32;
        for i in 0..32u16 {
            let seq = self.expected.wrapping_add(i);
            if self.pending.contains_key(&seq) {
                sack |= 1 << i;
            }
        }
        (cumulative, sack)
    }

    /// Есть ли дыра в последовательности (пришёл seq вперёд, а промежуточные нет) —
    /// повод немедленно выслать NACK, не дожидаясь таймаута отправителя (PROTOCOL.md §7.3).
    pub fn has_gap(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// Авто-fallback режима по деградации канала (PROTOCOL.md §9, plan.md §2).
///
/// Лестница режимов (сверху вниз — быстрее→надёжнее): 16-QAM → QPSK → CSS. При серии
/// неудач ARQ спускаемся на ступень; после серии успехов (гистерезис, чтобы не
/// осциллировать) — поднимаемся обратно.
pub struct AutoFallback {
    ladder: Vec<PhyMode>,
    level: usize, // индекс в ladder; 0 = самый быстрый
    consecutive_failures: u32,
    consecutive_successes: u32,
    fail_threshold: u32,
    recover_threshold: u32,
}

impl AutoFallback {
    pub fn new() -> Self {
        AutoFallback {
            ladder: vec![PhyMode::Ofdm16Qam, PhyMode::OfdmQpsk, PhyMode::Css],
            // Стартуем с CSS: сессия начинается в самом надёжном режиме (PROTOCOL.md §9).
            level: 2,
            consecutive_failures: 0,
            consecutive_successes: 0,
            fail_threshold: 3,    // 3 неподтверждённых кадра подряд → вниз (§9)
            recover_threshold: 5, // 5 успехов подряд → пробуем ступень выше
        }
    }

    pub fn current(&self) -> PhyMode {
        self.ladder[self.level]
    }

    /// Успешный round-trip: копим успехи; при достаточном числе — поднимаемся на ступень.
    /// Возвращает true, если режим сменился.
    pub fn on_success(&mut self) -> bool {
        self.consecutive_failures = 0;
        self.consecutive_successes += 1;
        if self.level > 0 && self.consecutive_successes >= self.recover_threshold {
            self.level -= 1;
            self.consecutive_successes = 0;
            return true;
        }
        false
    }

    /// Неудача (кадр не подтверждён): при серии — спускаемся на ступень (к более
    /// надёжному режиму). Возвращает true, если режим сменился.
    pub fn on_failure(&mut self) -> bool {
        self.consecutive_successes = 0;
        self.consecutive_failures += 1;
        if self.level + 1 < self.ladder.len() && self.consecutive_failures >= self.fail_threshold {
            self.level += 1;
            self.consecutive_failures = 0;
            return true;
        }
        false
    }

    /// Форсировать конкретный режим (ручной выбор в UI: Auto/ForceCss/ForceOfdm).
    pub fn force(&mut self, mode: PhyMode) {
        if let Some(idx) = self.ladder.iter().position(|&m| m == mode) {
            self.level = idx;
            self.consecutive_failures = 0;
            self.consecutive_successes = 0;
        }
    }
}

impl Default for AutoFallback {
    fn default() -> Self {
        Self::new()
    }
}

// --- сравнение seq с учётом заворота u16 ---
#[inline]
fn seq_lt(a: u16, b: u16) -> bool {
    (a.wrapping_sub(b) as i16) < 0
}
#[inline]
fn seq_le(a: u16, b: u16) -> bool {
    a == b || seq_lt(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_limits_in_flight() {
        let mut tx = ArqSender::new(ArqConfig {
            window: 4,
            timeout_ms: 300,
            max_retries: 8,
        });
        for i in 0..4 {
            assert_eq!(tx.send(vec![i], 0), Some(i as u16));
        }
        assert!(!tx.can_send());
        assert_eq!(tx.send(vec![9], 0), None);
    }

    #[test]
    fn ack_frees_window_and_measures_rtt() {
        let mut tx = ArqSender::new(ArqConfig {
            window: 4,
            timeout_ms: 300,
            max_retries: 8,
        });
        tx.send(vec![0], 100);
        tx.send(vec![1], 100);
        tx.on_ack(1, 0, 250); // кумулятивно подтверждаем seq 0 и 1
        assert_eq!(tx.in_flight(), 0);
        assert_eq!(tx.take_rtt_sample(), Some(150.0));
    }

    #[test]
    fn selective_ack_via_bitmap() {
        let mut tx = ArqSender::new(ArqConfig {
            window: 8,
            timeout_ms: 300,
            max_retries: 8,
        });
        for i in 0..4 {
            tx.send(vec![i], 0);
        }
        // seq 0 потерян, 1,2,3 приняты: cumulative=0 (ничего по порядку кроме 0?),
        // на деле cumulative = максимальный подряд. Здесь подтвердим 0 кумулятивно,
        // а 2,3 селективно (бит для seq 2 = offset 1, seq 3 = offset 2).
        tx.on_ack(0, 0b110, 0);
        assert_eq!(tx.in_flight(), 1); // остался только seq 1
    }

    #[test]
    fn timeout_triggers_retransmit_then_link_down() {
        let mut tx = ArqSender::new(ArqConfig {
            window: 4,
            timeout_ms: 100,
            max_retries: 2,
        });
        tx.send(vec![7], 0);
        assert!(tx.due_retransmissions(50).is_empty()); // ещё не пора
        let r = tx.due_retransmissions(200);
        assert_eq!(r, vec![(0u16, vec![7u8])]);
        // Гоняем до превышения retries (с учётом backoff растим время).
        let mut t = 200u64;
        for _ in 0..10 {
            t += 100_000;
            tx.due_retransmissions(t);
        }
        assert!(tx.link_down());
    }

    #[test]
    fn receiver_reorders_and_dedups() {
        let mut rx: ArqReceiver<u8> = ArqReceiver::new();
        assert_eq!(rx.push(0, 10), vec![10]);
        assert_eq!(rx.push(2, 12), Vec::<u8>::new()); // дыра, буферизуем
        assert!(rx.has_gap());
        assert_eq!(rx.push(2, 12), Vec::<u8>::new()); // дубликат
        assert_eq!(rx.push(1, 11), vec![11, 12]); // дыра закрыта → отдаём 1 и 2
        assert!(!rx.has_gap());
        let (cum, sack) = rx.ack_info();
        assert_eq!(cum, 2);
        assert_eq!(sack, 0);
    }

    #[test]
    fn receiver_sack_reports_out_of_order() {
        let mut rx: ArqReceiver<u8> = ArqReceiver::new();
        rx.push(0, 0);
        rx.push(2, 2); // seq 1 отсутствует
        let (cum, sack) = rx.ack_info();
        assert_eq!(cum, 0); // по порядку доставлен только 0
        assert_eq!(sack, 0b10); // seq 2 = expected(1)+1 → бит 1
    }

    #[test]
    fn auto_fallback_degrades_and_recovers() {
        let mut fb = AutoFallback::new();
        assert_eq!(fb.current(), PhyMode::Css); // старт в CSS
        fb.force(PhyMode::Ofdm16Qam);
        assert_eq!(fb.current(), PhyMode::Ofdm16Qam);

        // 3 неудачи подряд → спуск на ступень (QPSK).
        fb.on_failure();
        fb.on_failure();
        assert!(fb.on_failure());
        assert_eq!(fb.current(), PhyMode::OfdmQpsk);
        // ещё серия → CSS.
        fb.on_failure();
        fb.on_failure();
        assert!(fb.on_failure());
        assert_eq!(fb.current(), PhyMode::Css);

        // 5 успехов подряд → подъём обратно к QPSK.
        for _ in 0..4 {
            assert!(!fb.on_success());
        }
        assert!(fb.on_success());
        assert_eq!(fb.current(), PhyMode::OfdmQpsk);
    }
}
