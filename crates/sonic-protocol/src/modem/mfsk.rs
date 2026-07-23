//! MFSK — M-ичная частотная манипуляция, некогерентный режим (PROTOCOL.md §4, вариант).
//!
//! Информация — в НОМЕРЕ активного тона: символ `s` (0..M) звучит на своей частоте
//! `f_s = (s − M/2)·STRIDE·Δf` внутри под-полосы, где `Δf = fs/SPS_FFT`. Приём
//! некогерентный: берём FFT окна символа и ищем, в каком из M кандидатных бинов больше
//! энергии — позиция пика прямо даёт `s`. Это проще CSS (нет дечирпа/фазовой
//! синхронизации) и очень устойчиво к рассинхрону тактовой частоты и грубому таймингу.
//!
//! Ключ к устойчивости по времени — ЗАЩИТНЫЙ ИНТЕРВАЛ внутри символа: тон звучит
//! `SPS_FFT + GUARD` сэмплов, а анализируем окно из `SPS_FFT` сэмплов, отступив `GUARD/2`
//! от начала символа. Пока ошибка тайминга < `GUARD/2`, окно целиком лежит на постоянном
//! тоне, и FFT даёт чистый пик — как циклический префикс в OFDM, но для одного тона.
//!
//! Структура кадра (baseband, до апконверсии):
//! ```text
//! [PREAMBLE × pilot-тон] [sync-тон] [LEN_REPS × len-символы] [данные…]
//! ```
//! Робастный заголовок mode-agnostic: длина кадра шлётся повторами с мажоритарным
//! голосованием — приёмник не «гадает» длину (цель §1). Затем baseband апконвертится в
//! под-полосу активной [`DuplexScheme`](crate::bandplan::DuplexScheme).

use super::{bytes_to_symbols, symbols_to_bytes, Demodulated, Modem};
use crate::bandplan::SubBand;
use crate::fft::FftEngine;
use crate::framing::PhyMode;
use crate::iq::{downconvert, upconvert, FirLowpass};
use num_complex::Complex32;
use std::f32::consts::PI;

const M: usize = 16; // размер алфавита тонов
const BITS: u32 = 4; // log2(M) бит на символ
const STRIDE: i32 = 2; // шаг между соседними тонами в бинах (частотный guard)
const SPS_FFT: usize = 512; // длина анализирующего окна (Δf = fs/SPS_FFT)
const GUARD: usize = 384; // защитный интервал → терпимость к таймингу ±GUARD/2
const SYM: usize = SPS_FFT + GUARD; // полная длительность символа в сэмплах

const PREAMBLE: usize = 8; // pilot-бурстов для детекции фронта
const PILOT_SYM: u16 = (M / 2) as u16; // тон в центре под-полосы (бин 0)
const SYNC_SYM: u16 = 11; // магический sync-тон (защита от ложного захвата)
const LEN_REPS: usize = 3; // длина кадра шлётся 3× с мажоритарным голосованием

pub struct MfskModem {
    center_hz: f32,
    sample_rate: f32,
    bw: f32,
    fft: FftEngine,
}

impl MfskModem {
    pub fn new(band: SubBand, sample_rate: u32) -> Self {
        let sr = sample_rate as f32;
        // Полоса под тоны: (M/2)·STRIDE бинов в каждую сторону от центра. Δf = fs/SPS_FFT.
        // Проверяем, что размах тонов влезает в под-полосу с запасом.
        let df = sr / SPS_FFT as f32;
        let span = M as f32 * STRIDE as f32 * df; // полный размах занятых частот
        debug_assert!(span <= band.bandwidth_hz * 0.98, "MFSK tone span exceeds sub-band");
        let bw = span.min(band.bandwidth_hz * 0.98);

        MfskModem {
            center_hz: band.center_hz,
            sample_rate: sr,
            bw,
            fft: FftEngine::new(SPS_FFT),
        }
    }

    /// Бин baseband-тона для символа `s` (может быть отрицательным).
    fn bin_of(s: u16) -> i32 {
        (s as i32 - (M as i32) / 2) * STRIDE
    }

    fn len_syms(&self) -> usize {
        (16 + BITS as usize - 1) / BITS as usize // символов на u16 длину кадра
    }

    fn body_syms(&self, nbytes: usize) -> usize {
        (nbytes * 8 + BITS as usize - 1) / BITS as usize
    }

    fn header_slots(&self) -> usize {
        PREAMBLE + 1 /*sync*/ + LEN_REPS * self.len_syms()
    }

    /// Детекция символа: FFT окна SPS_FFT, отступив GUARD/2 от начала символа. Возвращает
    /// (значение символа, магнитуда пика, средняя магнитуда шума по кандидатным бинам).
    fn demod_tone(&self, bb: &[Complex32], sym_start: usize) -> (u16, f32, f32) {
        let off = sym_start + GUARD / 2;
        let mut buf: Vec<Complex32> = bb[off..off + SPS_FFT].to_vec();
        self.fft.forward(&mut buf);

        let mut best_s = 0usize;
        let mut best = f32::MIN;
        let mut sum = 0.0f32;
        for s in 0..M {
            let bin = Self::bin_of(s as u16).rem_euclid(SPS_FFT as i32) as usize;
            let e = buf[bin].norm_sqr();
            sum += e;
            if e > best {
                best = e;
                best_s = s;
            }
        }
        let peak = best.sqrt();
        let noise_mean = ((sum - best) / (M as f32 - 1.0)).max(1e-20).sqrt();
        (best_s as u16, peak, noise_mean)
    }
}

impl Modem for MfskModem {
    fn mode(&self) -> PhyMode {
        PhyMode::Mfsk
    }

    fn modulate(&self, frame_bytes: &[u8]) -> Vec<f32> {
        let len = frame_bytes.len() as u16;
        let len_syms = bytes_to_symbols(&len.to_be_bytes(), BITS);
        let body = bytes_to_symbols(frame_bytes, BITS);

        // Последовательность символов: преамбула pilot × PREAMBLE, sync, длина × LEN_REPS, тело.
        let mut syms: Vec<u16> = Vec::with_capacity(self.header_slots() + body.len());
        syms.extend(std::iter::repeat(PILOT_SYM).take(PREAMBLE));
        syms.push(SYNC_SYM);
        for _ in 0..LEN_REPS {
            syms.extend_from_slice(&len_syms);
        }
        syms.extend_from_slice(&body);

        // CPFSK: НЕПРЕРЫВНАЯ фаза через все символы. Иначе каждый тон стартовал с фазы 0 и
        // на стыке частот получался скачок → щелчок в динамике на каждом символе (~50/с),
        // который вдобавок размазывал спектр и мешал приёму. Демод (пик |FFT|) не зависит
        // от абсолютной фазы, поэтому непрерывность фазы ему безразлична.
        let mut bb: Vec<Complex32> = Vec::with_capacity(syms.len() * SYM);
        let mut phase = 0.0f64;
        for &s in &syms {
            let dphase = 2.0 * std::f64::consts::PI * Self::bin_of(s) as f64 / SPS_FFT as f64;
            for _ in 0..SYM {
                bb.push(Complex32::new(phase.cos() as f32, phase.sin() as f32));
                phase += dphase;
            }
        }

        let mut passband = upconvert(&bb, self.sample_rate, self.center_hz, 0);
        super::normalize_peak(&mut passband, super::TX_PEAK);
        edge_ramp(&mut passband, (self.sample_rate * 0.003) as usize);
        passband
    }

    fn demodulate(&self, samples: &[f32]) -> Option<Demodulated> {
        let min_frame = self.header_slots() * SYM + SPS_FFT;
        if samples.len() < min_frame {
            return None;
        }

        // Даунконверсия в baseband; ФНЧ режет чужую FDD-полосу (эхо) и образ.
        let mut lp = FirLowpass::new(self.bw * 0.6 + 200.0, self.sample_rate, 129);
        let bb = downconvert(samples, self.sample_rate, self.center_hz, 0, &mut lp);

        // 1. Грубый энергетический фронт преамбулы.
        let edge = coarse_edge(&bb, SYM / 16)?;

        // 2. Поиск sync-тона: сканируем символы от фронта; sync считается пойманным, если
        //    он сильный И перед ним стоит pilot-тон (защита от ложного захвата на шуме).
        //    Терпимо к тому, что первые pilot-бурсты «съел» разгон AGC микрофона.
        let mut header_start = None;
        let scan = PREAMBLE + 6;
        for j in 0..scan {
            let s0 = edge + j * SYM;
            if s0 + GUARD / 2 + SPS_FFT > bb.len() {
                break;
            }
            let (val, peak, noise) = self.demod_tone(&bb, s0);
            if val == SYNC_SYM && peak > noise * 4.0 && j >= 1 {
                let (prev, ppeak, pnoise) = self.demod_tone(&bb, edge + (j - 1) * SYM);
                if prev == PILOT_SYM && ppeak > pnoise * 3.0 {
                    header_start = Some(s0 + SYM); // сразу за sync
                    break;
                }
            }
        }
        let header = header_start?;

        // 3. Оценка SNR по sync-тону (для телеметрии).
        let sync_start = header.checked_sub(SYM)?;
        let (_, speak, snoise) = self.demod_tone(&bb, sync_start);
        let snr_db = 20.0 * (speak / snoise).log10();

        // 4. Длина кадра: LEN_REPS повторов, мажоритарное голосование по каждому символу.
        let len_syms = self.len_syms();
        if header + LEN_REPS * len_syms * SYM + SPS_FFT > bb.len() {
            return None;
        }
        let mut len_symbols = Vec::with_capacity(len_syms);
        for pos in 0..len_syms {
            let mut votes = vec![0u16; LEN_REPS];
            for (rep, vote) in votes.iter_mut().enumerate() {
                let s0 = header + (rep * len_syms + pos) * SYM;
                *vote = self.demod_tone(&bb, s0).0;
            }
            len_symbols.push(majority(&votes));
        }
        let len_bytes = symbols_to_bytes(&len_symbols, BITS, 2);
        if len_bytes.len() < 2 {
            return None;
        }
        let frame_len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
        if !(crate::framing::OVERHEAD..=8192).contains(&frame_len) {
            return None; // неправдоподобная длина — ложный захват
        }

        // 5. Тело кадра.
        let body_base = header + LEN_REPS * len_syms * SYM;
        let body_syms = self.body_syms(frame_len);
        let body_end = body_base + body_syms * SYM;
        if body_end - GUARD / 2 + SPS_FFT > bb.len() && body_base + (body_syms - 1) * SYM + GUARD / 2 + SPS_FFT > bb.len() {
            return None;
        }
        let mut body_symbols = Vec::with_capacity(body_syms);
        for i in 0..body_syms {
            let s0 = body_base + i * SYM;
            if s0 + GUARD / 2 + SPS_FFT > bb.len() {
                return None;
            }
            body_symbols.push(self.demod_tone(&bb, s0).0);
        }
        let bytes = symbols_to_bytes(&body_symbols, BITS, frame_len);

        Some(Demodulated {
            bytes,
            start_sample: edge,
            end_sample: body_end.min(samples.len()),
            snr_db,
        })
    }

    fn frame_samples(&self, payload_len: usize) -> usize {
        let frame_len = crate::framing::OVERHEAD + payload_len;
        (self.header_slots() + self.body_syms(frame_len)) * SYM
    }
}

/// Мажоритарное голосование по повторам (для служебной длины кадра).
fn majority(votes: &[u16]) -> u16 {
    let mut best = votes[0];
    let mut best_count = 0;
    for &v in votes {
        let count = votes.iter().filter(|&&x| x == v).count();
        if count > best_count {
            best_count = count;
            best = v;
        }
    }
    best
}

/// Нарастающий фронт энергии — грубая привязка к началу преамбулы. Окно короткое, порог
/// относительно и пикового уровня, и шумового пола записи.
fn coarse_edge(bb: &[Complex32], win: usize) -> Option<usize> {
    let win = win.max(64);
    let step = (win / 2).max(1);
    if bb.len() < SYM * 2 {
        return None;
    }
    let mut rms = Vec::new();
    let mut i = 0;
    while i + win <= bb.len() {
        let e: f32 = bb[i..i + win].iter().map(|c| c.norm_sqr()).sum();
        rms.push(((e / win as f32).sqrt(), i));
        i += step;
    }
    let peak_rms = rms.iter().fold(0.0f32, |a, x| a.max(x.0));
    let floor_rms = rms.iter().fold(f32::MAX, |a, x| a.min(x.0));
    if peak_rms < 1e-4 {
        return None;
    }
    let threshold = (peak_rms * 0.35).max(floor_rms * 4.0);
    for &(r, idx) in &rms {
        if r >= threshold {
            return Some(idx);
        }
    }
    None
}

/// Мягкий фронт/срез (raised-cosine), чтобы динамик не щёлкал на границах кадра.
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

    fn modem() -> MfskModem {
        let fdd = Fdd::new(Role::Initiator, Profile::Audible);
        MfskModem::new(fdd.tx_band(), fdd.sample_rate())
    }

    #[test]
    fn tone_span_fits_subband() {
        // Не должно паниковать в debug: размах тонов влезает в под-полосу.
        let _ = modem();
    }

    #[test]
    fn zero_noise_frame_roundtrip() {
        let m = modem();
        let frame = b"\x2B\x10hello acoustic world via MFSK tones".to_vec();
        let tx = m.modulate(&frame);
        let mut buf = vec![0.0f32; 2000];
        buf.extend_from_slice(&tx);
        buf.extend(std::iter::repeat(0.0).take(2000));
        let got = m.demodulate(&buf).expect("MFSK frame not demodulated");
        assert_eq!(got.bytes, frame);
    }

    #[test]
    fn survives_moderate_awgn() {
        let m = modem();
        let frame = b"ADL/1 MFSK under noise 0123456789".to_vec();
        let tx = m.modulate(&frame);
        let mut rng = Lcg(12345);
        let sig_rms = (tx.iter().map(|x| x * x).sum::<f32>() / tx.len() as f32).sqrt();
        let noise_amp = sig_rms * 0.4;
        let mut buf = vec![0.0f32; 3000];
        buf.extend(tx.iter().map(|&s| s + rng.next_f32() * noise_amp));
        buf.extend((0..3000).map(|_| rng.next_f32() * noise_amp));
        let got = m.demodulate(&buf).expect("MFSK frame lost under AWGN");
        assert_eq!(got.bytes, frame);
    }

    #[test]
    fn survives_timing_jitter() {
        // Грубый тайминг: произвольный сдвиг старта — защитный интервал должен вытянуть.
        let m = modem();
        let frame = b"MFSK timing tolerance test payload".to_vec();
        let tx = m.modulate(&frame);
        for lead in [1500usize, 1571, 1633, 1700, 1811] {
            let mut buf = vec![0.0f32; lead];
            buf.extend_from_slice(&tx);
            buf.extend(std::iter::repeat(0.0).take(2000));
            let got = m.demodulate(&buf).expect("MFSK lost under timing jitter");
            assert_eq!(got.bytes, frame, "lead={lead}");
        }
    }

    #[test]
    fn no_false_frame_on_pure_noise() {
        let m = modem();
        let mut rng = Lcg(999);
        let buf: Vec<f32> = (0..120_000).map(|_| rng.next_f32() * 0.2).collect();
        assert!(m.demodulate(&buf).is_none());
    }
}
