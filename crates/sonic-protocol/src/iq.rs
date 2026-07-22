//! Мост между вещественным аудио и комплексным baseband (квадратурный микшер) +
//! полосовой FIR-фильтр приёмного тракта.
//!
//! Модуляторы работают в baseband (комплексные IQ-сэмплы вокруг 0 Гц), а в эфир идёт
//! вещественный сигнал в под-полосе активной [`DuplexScheme`](crate::DuplexScheme).
//! Апконверсия сдвигает baseband на несущую полосы; на приёме даунконверсия сдвигает
//! обратно, а FIR-ФНЧ убирает образ на 2·fc и — главное — чужую FDD-полосу (это и есть
//! «эхо режется полосовым фильтром» из PROTOCOL.md §2.1).

use num_complex::Complex32;
use std::f32::consts::PI;

/// Апконверсия комплексного baseband в вещественный passband на несущей `center_hz`.
/// `n0` — индекс первого сэмпла в общем потоке (для непрерывности фазы между блоками).
pub fn upconvert(baseband: &[Complex32], sample_rate: f32, center_hz: f32, n0: u64) -> Vec<f32> {
    let w = 2.0 * PI * center_hz / sample_rate;
    baseband
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let phase = w * (n0 + i as u64) as f32;
            // Re{ c · e^{jωt} } = c_re·cos − c_im·sin.
            c.re * phase.cos() - c.im * phase.sin()
        })
        .collect()
}

/// Даунконверсия вещественного passband обратно в комплексный baseband с ФНЧ.
/// Множитель 2 компенсирует расщепление энергии вещественного сигнала на ±f.
pub fn downconvert(
    passband: &[f32],
    sample_rate: f32,
    center_hz: f32,
    n0: u64,
    filter: &mut FirLowpass,
) -> Vec<Complex32> {
    let w = 2.0 * PI * center_hz / sample_rate;
    let mixed: Vec<Complex32> = passband
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let phase = w * (n0 + i as u64) as f32;
            // 2·s·e^{-jωt}: переносит нужную полосу к 0 Гц, образ уходит на −2fc/+2fc.
            Complex32::new(2.0 * s * phase.cos(), -2.0 * s * phase.sin())
        })
        .collect();
    filter.process(&mixed)
}

/// Комплексный FIR-фильтр нижних частот (windowed-sinc, окно Хэмминга). Stateful:
/// хранит хвост предыдущего блока, поэтому корректен и для потоковой обработки в
/// аудио-пайплайне, и для разовой обработки кадра в тестах.
pub struct FirLowpass {
    taps: Vec<f32>,
    /// Линия задержки последних (len-1) входных сэмплов между вызовами `process`.
    history: Vec<Complex32>,
}

impl FirLowpass {
    /// `cutoff_hz` — частота среза, `num_taps` — длина (будет приведена к нечётной).
    pub fn new(cutoff_hz: f32, sample_rate: f32, num_taps: usize) -> Self {
        let len = if num_taps % 2 == 0 { num_taps + 1 } else { num_taps };
        let fc = (cutoff_hz / sample_rate).clamp(1e-4, 0.5); // нормированная частота среза
        let m = (len - 1) as f32;
        let mut taps = vec![0.0f32; len];
        let mut sum = 0.0f32;
        for (i, tap) in taps.iter_mut().enumerate() {
            let x = i as f32 - m / 2.0;
            // sinc(2·fc·x)
            let sinc = if x.abs() < 1e-6 {
                2.0 * fc
            } else {
                (2.0 * PI * fc * x).sin() / (PI * x)
            };
            // окно Хэмминга
            let win = 0.54 - 0.46 * (2.0 * PI * i as f32 / m).cos();
            *tap = sinc * win;
            sum += *tap;
        }
        // Нормировка на единичный коэффициент передачи по постоянному току.
        for tap in taps.iter_mut() {
            *tap /= sum;
        }
        FirLowpass {
            history: vec![Complex32::new(0.0, 0.0); len - 1],
            taps,
        }
    }

    /// Групповая задержка фильтра в сэмплах (линейная фаза → (len-1)/2).
    pub fn group_delay(&self) -> usize {
        (self.taps.len() - 1) / 2
    }

    /// Отфильтровать блок; сохраняет хвост для непрерывности со следующим блоком.
    pub fn process(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let len = self.taps.len();
        // [history | input] — линейная свёртка с учётом хвоста прошлого блока.
        let mut buf = Vec::with_capacity(self.history.len() + input.len());
        buf.extend_from_slice(&self.history);
        buf.extend_from_slice(input);

        let mut out = Vec::with_capacity(input.len());
        for i in 0..input.len() {
            let mut acc = Complex32::new(0.0, 0.0);
            for (k, &tap) in self.taps.iter().enumerate() {
                acc += buf[i + len - 1 - k] * tap;
            }
            out.push(acc);
        }
        // Сохраняем последние len-1 сэмплов как историю для следующего вызова.
        let keep = buf.len().saturating_sub(len - 1);
        self.history = buf[keep..].to_vec();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upconvert_downconvert_recovers_baseband_tone() {
        let fs = 48_000.0;
        let fc = 4000.0;
        let n = 4096;
        // Baseband: комплексная экспонента на +200 Гц.
        let f_bb = 200.0;
        let baseband: Vec<Complex32> = (0..n)
            .map(|i| {
                let p = 2.0 * PI * f_bb * i as f32 / fs;
                Complex32::new(p.cos(), p.sin())
            })
            .collect();
        let passband = upconvert(&baseband, fs, fc, 0);
        let mut lp = FirLowpass::new(1000.0, fs, 129);
        let recovered = downconvert(&passband, fs, fc, 0, &mut lp);

        // После групповой задержки фильтра baseband-тон восстанавливается по фазе.
        let d = lp.group_delay();
        let mut max_err = 0.0f32;
        for i in (d + 200)..(n - 1) {
            let err = (recovered[i] - baseband[i - d]).norm();
            max_err = max_err.max(err);
        }
        assert!(max_err < 0.05, "max_err = {max_err}");
    }

    #[test]
    fn lowpass_rejects_out_of_band_tone() {
        let fs = 48_000.0;
        let n = 8192;
        let mut lp = FirLowpass::new(1000.0, fs, 129);
        // Тон на 5000 Гц — далеко за срезом, должен подавиться.
        let input: Vec<Complex32> = (0..n)
            .map(|i| {
                let p = 2.0 * PI * 5000.0 * i as f32 / fs;
                Complex32::new(p.cos(), p.sin())
            })
            .collect();
        let out = lp.process(&input);
        let tail_rms: f32 =
            (out[2000..].iter().map(|c| c.norm_sqr()).sum::<f32>() / (n - 2000) as f32).sqrt();
        assert!(tail_rms < 0.1, "tail_rms = {tail_rms}");
    }
}
