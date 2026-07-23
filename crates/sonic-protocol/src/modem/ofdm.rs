//! OFDM + QAM — быстрый режим (PROTOCOL.md §5).
//!
//! Параметры из §5.1: FFT N=1024, CP=128 (2.67 мс — покрывает реверберацию типичной
//! комнаты), разнос поднесущих 46.875 Гц, символ 24 мс. Полезные поднесущие лежат внутри
//! под-полосы активной [`DuplexScheme`]; каждая 8-я — пилот для оценки канала и фазы.
//!
//! Структура кадра (baseband, ср. §5.3):
//! ```text
//! [Schmidl-Cox преамбула] [символ оценки канала] [символ длины (QPSK, робастно)] [данные…]
//! ```
//! - Schmidl-Cox: символ из двух одинаковых половин во времени → грубая
//!   временная/частотная синхронизация автокорреляцией половинок.
//! - Символ оценки канала: все поднесущие известны → H[k] на приём.
//! - Символ длины: 16-битная длина кадра, размазанная по поднесущим с мажоритарным
//!   голосованием, — OFDM никогда не «гадает» длину (цель §1).
//!
//! Многолучёвость в пределах CP превращается в частотно-селективный H[k] и снимается
//! эквалайзером (деление на H). CP короче разброса задержек → пол BER, который FEC не
//! лечит: длину CP берут из замера импульсной характеристики комнаты (plan.md Фаза 0).

use super::qam::{self, Modulation};
use super::{Demodulated, Modem};
use crate::bandplan::SubBand;
use crate::fft::FftEngine;
use crate::framing::PhyMode;
use crate::iq::{downconvert, upconvert, FirLowpass};
use num_complex::Complex32;
use std::f32::consts::PI;

const N: usize = 1024;
const CP: usize = 128;
const HALF: usize = N / 2;
const PILOT_STRIDE: usize = 8;

pub struct OfdmModem {
    center_hz: f32,
    sample_rate: f32,
    bw: f32,
    modulation: Modulation,
    /// Индексы активных поднесущих (в порядке FFT-бинов) — общий для TX/RX.
    active: Vec<usize>,
    pilot_bins: Vec<usize>,
    data_bins: Vec<usize>,
    /// Известные значения на всех активных бинах — символ оценки канала.
    ce_values: Vec<Complex32>,
    /// Известные пилотные значения (выровнены с `pilot_bins`).
    pilot_values: Vec<Complex32>,
    fft: FftEngine,
}

impl OfdmModem {
    pub fn new(band: SubBand, sample_rate: u32, modulation: Modulation) -> Self {
        let sr = sample_rate as f32;
        let bw = band.bandwidth_hz;
        let spacing = sr / N as f32;
        let guard = spacing * 2.0;
        let k_max = (((bw / 2.0) - guard) / spacing).floor() as usize;
        let k_min = 2usize; // пропускаем DC и соседние (утечка несущей)

        // Активные бины: положительные k_min..=k_max и их «зеркала» N-k (отрицательные частоты).
        let mut active = Vec::new();
        for k in k_min..=k_max {
            active.push(k);
        }
        for k in k_min..=k_max {
            active.push(N - k);
        }

        // Каждая PILOT_STRIDE-я активная поднесущая — пилот.
        let mut pilot_bins = Vec::new();
        let mut data_bins = Vec::new();
        for (i, &bin) in active.iter().enumerate() {
            if i % PILOT_STRIDE == 0 {
                pilot_bins.push(bin);
            } else {
                data_bins.push(bin);
            }
        }

        let ce_values: Vec<Complex32> = active.iter().map(|&b| pn_qpsk(b as u64 ^ 0x51)).collect();
        let pilot_values: Vec<Complex32> =
            pilot_bins.iter().map(|&b| pn_qpsk(b as u64 ^ 0xA3)).collect();

        OfdmModem {
            center_hz: band.center_hz,
            sample_rate: sr,
            bw,
            modulation,
            active,
            pilot_bins,
            data_bins,
            ce_values,
            pilot_values,
            fft: FftEngine::new(N),
        }
    }

    pub fn data_subcarriers(&self) -> usize {
        self.data_bins.len()
    }

    fn bits_per_symbol(&self) -> usize {
        self.data_bins.len() * self.modulation.bits_per_symbol()
    }

    /// Один OFDM-символ из частотных значений (длина N, нули на неактивных) → время с CP.
    fn ofdm_symbol_time(&self, freq: &[Complex32]) -> Vec<Complex32> {
        let mut buf = freq.to_vec();
        self.fft.inverse_normalized(&mut buf);
        let mut out = Vec::with_capacity(N + CP);
        out.extend_from_slice(&buf[N - CP..]); // циклический префикс
        out.extend_from_slice(&buf);
        out
    }

    /// Преамбула Schmidl-Cox: PN только на ЧЁТНЫХ активных поднесущих → две одинаковые
    /// половины во времени.
    fn preamble_symbol(&self) -> Vec<Complex32> {
        let mut freq = vec![Complex32::new(0.0, 0.0); N];
        for &bin in &self.active {
            if bin % 2 == 0 {
                // масштаб sqrt(2): та же средняя мощность при половине занятых поднесущих
                freq[bin] = pn_qpsk(bin as u64 ^ 0x7E) * std::f32::consts::SQRT_2;
            }
        }
        self.ofdm_symbol_time(&freq)
    }

    fn ce_symbol(&self) -> Vec<Complex32> {
        let mut freq = vec![Complex32::new(0.0, 0.0); N];
        for (i, &bin) in self.active.iter().enumerate() {
            freq[bin] = self.ce_values[i];
        }
        self.ofdm_symbol_time(&freq)
    }

    /// Символ длины: 16-битная длина кадра, размноженная по data-поднесущим (QPSK).
    fn length_symbol(&self, frame_len: u16) -> Vec<Complex32> {
        let len_bits = qam::bytes_to_bits(&frame_len.to_be_bytes()); // 16 бит
        let need = self.data_bins.len() * 2; // QPSK = 2 бита/поднесущую
        let tiled: Vec<u8> = (0..need).map(|i| len_bits[i % 16]).collect();
        let syms = qam::map(&tiled, Modulation::Qpsk);
        self.assemble_data_symbol(&syms)
    }

    /// Собирает частотный вектор из data-символов + пилотов и переводит во время.
    fn assemble_data_symbol(&self, data_syms: &[Complex32]) -> Vec<Complex32> {
        let mut freq = vec![Complex32::new(0.0, 0.0); N];
        for (&bin, &val) in self.data_bins.iter().zip(data_syms.iter()) {
            freq[bin] = val;
        }
        for (&bin, &val) in self.pilot_bins.iter().zip(self.pilot_values.iter()) {
            freq[bin] = val;
        }
        self.ofdm_symbol_time(&freq)
    }

    /// FFT одного принятого символа: убирает CP из окна [useful_start, +N), возвращает спектр.
    fn symbol_spectrum(&self, bb: &[Complex32], useful_start: usize) -> Vec<Complex32> {
        let mut buf: Vec<Complex32> = bb[useful_start..useful_start + N].to_vec();
        self.fft.forward(&mut buf);
        buf
    }
}

impl Modem for OfdmModem {
    fn mode(&self) -> PhyMode {
        match self.modulation {
            Modulation::Qpsk => PhyMode::OfdmQpsk,
            Modulation::Qam16 => PhyMode::Ofdm16Qam,
        }
    }

    fn modulate(&self, frame_bytes: &[u8]) -> Vec<f32> {
        let mut bb: Vec<Complex32> = Vec::new();
        bb.extend(self.preamble_symbol());
        bb.extend(self.ce_symbol());
        bb.extend(self.length_symbol(frame_bytes.len() as u16));

        // Данные.
        let mut bits = qam::bytes_to_bits(frame_bytes);
        let per = self.bits_per_symbol();
        let pad = (per - bits.len() % per) % per;
        bits.extend(std::iter::repeat(0).take(pad));
        for chunk in bits.chunks(per) {
            let syms = qam::map(chunk, self.modulation);
            bb.extend(self.assemble_data_symbol(&syms));
        }

        // Циклический постфикс: продолжаем последний символ его же началом (IFFT-выход
        // периодичен) — гладкий «хвост» под fade-out, который не трогает полезные данные.
        let ramp = (self.sample_rate * 0.003) as usize;
        if bb.len() >= N {
            let tail_start = bb.len() - N;
            let post: Vec<Complex32> = bb[tail_start..tail_start + ramp.min(N)].to_vec();
            bb.extend(post);
        }

        let mut passband = upconvert(&bb, self.sample_rate, self.center_hz, 0);
        // Пиковая нормализация с запасом до полной шкалы (TX_PEAK): у OFDM высокий пик-фактор,
        // и на пике ~1.0 динамик клиппит именно редкие всплески — 16-QAM (амплитудное
        // созвездие) от этого рассыпается. БЕЗ ограничения/tanh (оно ломает 16-QAM даже в
        // чистом канале). Общий масштаб поглощается эквалайзером на приёме (H с тем же усилением).
        super::normalize_peak(&mut passband, super::TX_PEAK);
        edge_ramp(&mut passband, ramp);
        passband
    }

    fn demodulate(&self, samples: &[f32]) -> Option<Demodulated> {
        let min_len = 4 * (N + CP);
        if samples.len() < min_len {
            return None;
        }

        let mut lp = FirLowpass::new(self.bw * 0.6, self.sample_rate, 129);
        let mut bb = downconvert(samples, self.sample_rate, self.center_hz, 0, &mut lp);

        // 1. Schmidl-Cox: грубая временная привязка + дробный CFO.
        let (d_peak, cfo) = schmidl_cox(&bb)?;

        // 2. Коррекция дробного CFO по всей записи.
        if cfo.abs() > 1e-6 {
            let w = 2.0 * PI * cfo / N as f32;
            for (n, x) in bb.iter_mut().enumerate() {
                let ph = -w * n as f32;
                *x *= Complex32::new(ph.cos(), ph.sin());
            }
        }

        // Курсор идёт по границам символов: после N полезных сэмплов преамбулы начинается
        // символ оценки канала (со своим CP).
        let mut cursor = d_peak + N;
        let take_useful = |start: usize| start + CP; // полезная часть после CP символа

        // 3. Оценка канала по CE-символу.
        let ce_start = take_useful(cursor);
        if ce_start + N > bb.len() {
            return None;
        }
        let ce_spec = self.symbol_spectrum(&bb, ce_start);
        let mut h = vec![Complex32::new(0.0, 0.0); N];
        for (i, &bin) in self.active.iter().enumerate() {
            h[bin] = ce_spec[bin] / self.ce_values[i];
        }
        cursor += N + CP;

        // 4. Символ длины (QPSK), мажоритарное голосование по повторам.
        let len_start = take_useful(cursor);
        if len_start + N > bb.len() {
            return None;
        }
        let len_spec = self.symbol_spectrum(&bb, len_start);
        let len_data = self.equalize_and_track(&len_spec, &h);
        let len_bits_raw = qam::demap(&len_data, Modulation::Qpsk);
        let mut votes = [0i32; 16];
        for (i, &b) in len_bits_raw.iter().enumerate() {
            votes[i % 16] += if b == 1 { 1 } else { -1 };
        }
        let len_bits: Vec<u8> = votes.iter().map(|&v| (v > 0) as u8).collect();
        let frame_len = qam::bits_to_bytes(&len_bits);
        let frame_len = u16::from_be_bytes([frame_len[0], frame_len[1]]) as usize;
        if !(crate::framing::OVERHEAD..=8192).contains(&frame_len) {
            return None;
        }
        cursor += N + CP;

        // 5. Символы данных.
        let per = self.bits_per_symbol();
        let total_bits = frame_len * 8;
        let n_data_syms = total_bits.div_ceil(per);
        let mut bits = Vec::with_capacity(n_data_syms * per);
        let mut snr_acc = 0.0f32;
        for _ in 0..n_data_syms {
            let start = take_useful(cursor);
            if start + N > bb.len() {
                return None;
            }
            let spec = self.symbol_spectrum(&bb, start);
            let data = self.equalize_and_track(&spec, &h);
            snr_acc += evm_snr_db(&data, self.modulation);
            bits.extend(qam::demap(&data, self.modulation));
            cursor += N + CP;
        }
        bits.truncate(total_bits);
        let bytes = qam::bits_to_bytes(&bits);
        let snr_db = if n_data_syms > 0 { snr_acc / n_data_syms as f32 } else { 0.0 };

        Some(Demodulated {
            bytes,
            start_sample: d_peak.saturating_sub(CP),
            end_sample: cursor,
            snr_db,
        })
    }

    fn frame_samples(&self, payload_len: usize) -> usize {
        let frame_len = crate::framing::OVERHEAD + payload_len;
        let n_data = (frame_len * 8).div_ceil(self.bits_per_symbol());
        (3 + n_data) * (N + CP)
    }
}

impl OfdmModem {
    /// Эквалайзер (Y/H) + коррекция остаточной фазы по пилотам. Возвращает data-символы.
    fn equalize_and_track(&self, spec: &[Complex32], h: &[Complex32]) -> Vec<Complex32> {
        // Остаточный поворот фазы (дрейф частоты дискретизации / остаточный CFO) — по пилотам.
        let mut acc = Complex32::new(0.0, 0.0);
        for (&bin, &pv) in self.pilot_bins.iter().zip(self.pilot_values.iter()) {
            if h[bin].norm_sqr() > 1e-12 {
                let eq = spec[bin] / h[bin];
                acc += eq * pv.conj();
            }
        }
        let phi = acc.arg();
        let derot = Complex32::new(phi.cos(), -phi.sin());

        self.data_bins
            .iter()
            .map(|&bin| {
                if h[bin].norm_sqr() > 1e-12 {
                    (spec[bin] / h[bin]) * derot
                } else {
                    Complex32::new(0.0, 0.0)
                }
            })
            .collect()
    }
}

/// Schmidl-Cox: скользящая автокорреляция половинок. Возвращает (начало полезной части
/// преамбулы, дробный CFO в единицах разноса поднесущих).
fn schmidl_cox(bb: &[Complex32]) -> Option<(usize, f32)> {
    let search_end = bb.len().saturating_sub(N);
    if search_end == 0 {
        return None;
    }

    // Инкрементальное скользящее окно P(d)=Σ conj(r[d+m])·r[d+m+L], R(d)=Σ|r[d+m+L]|².
    // Копим (M, R, P) по d; ложные пики метрики бывают в низкоэнергетичных участках
    // (край сигнала → малое R раздувает P/R²), поэтому пик берём с энергетическим гейтом.
    let mut metrics: Vec<(f32, f32, Complex32)> = Vec::with_capacity(search_end + 1);
    let mut p = Complex32::new(0.0, 0.0);
    let mut r = 0.0f32;
    for m in 0..HALF {
        p += bb[m].conj() * bb[m + HALF];
        r += bb[m + HALF].norm_sqr();
    }
    let mut d = 0usize;
    loop {
        let m = if r > 1e-9 { p.norm_sqr() / (r * r) } else { 0.0 };
        metrics.push((m, r, p));
        if d + 1 > search_end {
            break;
        }
        p -= bb[d].conj() * bb[d + HALF];
        r -= bb[d + HALF].norm_sqr();
        let n = d + HALF;
        p += bb[n].conj() * bb[n + HALF];
        r += bb[n + HALF].norm_sqr();
        d += 1;
    }

    let r_max = metrics.iter().fold(0.0f32, |a, x| a.max(x.1));
    if r_max < 1e-6 {
        return None; // тишина
    }
    let r_gate = 0.25 * r_max;

    let mut best_m = 0.0f32;
    let mut best_d = 0usize;
    let mut best_p = Complex32::new(0.0, 0.0);
    for (idx, &(m, rr, pp)) in metrics.iter().enumerate() {
        if rr >= r_gate && m > best_m {
            best_m = m;
            best_d = idx;
            best_p = pp;
        }
    }
    // Порог детекции снижен с 0.5 до 0.35: реальный захват (реверберация, AGC микрофона,
    // клиппинг динамика) роняет метрику ниже идеальных 0.5, из-за чего кадр «ловился
    // 1 раз из 10». От ложных срабатываний на шуме защищает энергетический гейт r_gate:
    // в тишине/шуме R мало → метрика не проходит гейт, а не порог.
    if best_m < 0.35 {
        return None; // преамбула не найдена
    }
    // Тайминг — аргмакс метрики (левый край плато Schmidl-Cox ≈ начало полезной части
    // преамбулы). Небольшой сдвиг внутрь циклического префикса (в сторону CP), чтобы окна
    // символов сэмплировались с запасом на реверберацию, а не впритык к следующему символу.
    let d = best_d.saturating_sub(CP / 8);

    // Дробный CFO: фазовый сдвиг между половинками = π·ε (ε в единицах разноса).
    let cfo = best_p.arg() / PI;
    Some((d, cfo))
}

/// QPSK-точка из seed (детерминированно, одинаково у TX/RX).
fn pn_qpsk(seed: u64) -> Complex32 {
    // FNV-хеш → 2 бита → QPSK.
    let mut h = 0xcbf29ce484222325u64 ^ seed;
    h = h.wrapping_mul(0x100000001b3);
    h ^= h >> 29;
    let s = 0.707_106_77f32;
    let i = if h & 1 == 0 { s } else { -s };
    let q = if h & 2 == 0 { s } else { -s };
    Complex32::new(i, q)
}

/// Оценка SNR по EVM (ошибке относительно ближайшей точки созвездия).
fn evm_snr_db(syms: &[Complex32], modulation: Modulation) -> f32 {
    if syms.is_empty() {
        return 0.0;
    }
    let hard = qam::map(&qam::demap(syms, modulation), modulation);
    let mut err = 0.0f32;
    let mut sig = 0.0f32;
    for (s, h) in syms.iter().zip(hard.iter()) {
        err += (s - h).norm_sqr();
        sig += h.norm_sqr();
    }
    if err < 1e-12 {
        return 40.0;
    }
    (10.0 * (sig / err).log10()).clamp(-10.0, 40.0)
}

fn edge_ramp(signal: &mut [f32], ramp: usize) {
    let ramp = ramp.min(signal.len() / 2).max(1);
    let n = signal.len();
    for i in 0..ramp {
        let g = 0.5 - 0.5 * (PI * i as f32 / ramp as f32).cos();
        signal[i] *= g;
        signal[n - 1 - i] *= g;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bandplan::{DuplexScheme, Fdd, Profile, Role};

    struct Lcg(u64);
    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    fn modem(m: Modulation) -> OfdmModem {
        let fdd = Fdd::new(Role::Initiator, Profile::Audible);
        OfdmModem::new(fdd.tx_band(), fdd.sample_rate(), m)
    }

    #[test]
    fn qpsk_frame_roundtrip_clean() {
        let m = modem(Modulation::Qpsk);
        let frame = b"\x2B\x01OFDM QPSK payload over the acoustic link, hello!".to_vec();
        let tx = m.modulate(&frame);
        let mut buf = vec![0.0f32; 1500];
        buf.extend_from_slice(&tx);
        buf.extend(std::iter::repeat(0.0).take(1500));
        let got = m.demodulate(&buf).expect("OFDM frame not demodulated");
        assert_eq!(got.bytes, frame);
    }

    #[test]
    fn qam16_frame_roundtrip_clean() {
        let m = modem(Modulation::Qam16);
        let frame: Vec<u8> = (0..120).map(|i| (i * 7 + 1) as u8).collect();
        let tx = m.modulate(&frame);
        let mut buf = vec![0.0f32; 1500];
        buf.extend_from_slice(&tx);
        buf.extend(std::iter::repeat(0.0).take(1500));
        let got = m.demodulate(&buf).expect("16-QAM frame not demodulated");
        assert_eq!(got.bytes, frame);
    }

    #[test]
    fn qpsk_survives_awgn() {
        let m = modem(Modulation::Qpsk);
        let frame = b"OFDM QPSK under AWGN, ADL/1 fast mode 0123456789".to_vec();
        let tx = m.modulate(&frame);
        let sig_rms = (tx.iter().map(|x| x * x).sum::<f32>() / tx.len() as f32).sqrt();
        let mut rng = Lcg(7);
        let namp = sig_rms * 0.15;
        let mut buf = vec![0.0f32; 1500];
        buf.extend(tx.iter().map(|&s| s + rng.next_f32() * namp));
        buf.extend((0..1500).map(|_| rng.next_f32() * namp));
        let got = m.demodulate(&buf).expect("OFDM lost under AWGN");
        assert_eq!(got.bytes, frame);
    }

    #[test]
    fn qpsk_survives_multipath_within_cp() {
        let m = modem(Modulation::Qpsk);
        let frame = b"multipath echo test within cyclic prefix bounds!!".to_vec();
        let tx = m.modulate(&frame);
        // Синтетическое эхо на 40 сэмплов (< CP=128) с затуханием — снимается CP+эквалайзером.
        let delay = 40usize;
        let mut echoed = tx.clone();
        for i in delay..tx.len() {
            echoed[i] += 0.5 * tx[i - delay];
        }
        let mut buf = vec![0.0f32; 1500];
        buf.extend_from_slice(&echoed);
        buf.extend(std::iter::repeat(0.0).take(1500));
        let got = m.demodulate(&buf).expect("OFDM lost under multipath");
        assert_eq!(got.bytes, frame);
    }
}
