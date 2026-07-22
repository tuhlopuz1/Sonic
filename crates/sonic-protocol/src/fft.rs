//! Тонкая обёртка над `rustfft` с кэшированным планировщиком. Общая для `modem::css`
//! (дечирп + пик) и `modem::ofdm` (IFFT/FFT поднесущих) — один планировщик на длину.

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Кэширует спланированные прямое и обратное преобразования одной длины `n`.
pub struct FftEngine {
    n: usize,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
}

impl FftEngine {
    pub fn new(n: usize) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        FftEngine {
            n,
            forward: planner.plan_fft_forward(n),
            inverse: planner.plan_fft_inverse(n),
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    /// In-place прямое ДПФ (ненормированное, как в rustfft).
    pub fn forward(&self, buf: &mut [Complex32]) {
        debug_assert_eq!(buf.len(), self.n);
        self.forward.process(buf);
    }

    /// In-place обратное ДПФ. rustfft не нормирует — делим на N сами (нужно OFDM для
    /// корректной амплитуды сэмплов после IFFT).
    pub fn inverse_normalized(&self, buf: &mut [Complex32]) {
        debug_assert_eq!(buf.len(), self.n);
        self.inverse.process(buf);
        let scale = 1.0 / self.n as f32;
        for x in buf.iter_mut() {
            *x *= scale;
        }
    }

    /// Обратное ДПФ без нормировки.
    pub fn inverse_raw(&self, buf: &mut [Complex32]) {
        debug_assert_eq!(buf.len(), self.n);
        self.inverse.process(buf);
    }
}

/// Индекс бина с максимальной величиной и сама величина — базовая операция дечирпа CSS.
pub fn argmax_magnitude(spectrum: &[Complex32]) -> (usize, f32) {
    let mut best_idx = 0;
    let mut best = f32::MIN;
    for (i, c) in spectrum.iter().enumerate() {
        let m = c.norm_sqr();
        if m > best {
            best = m;
            best_idx = i;
        }
    }
    (best_idx, best.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_roundtrip_recovers_signal() {
        let engine = FftEngine::new(64);
        let original: Vec<Complex32> = (0..64)
            .map(|i| Complex32::new((i as f32 * 0.3).sin(), 0.0))
            .collect();
        let mut buf = original.clone();
        engine.forward(&mut buf);
        engine.inverse_normalized(&mut buf);
        for (a, b) in original.iter().zip(buf.iter()) {
            assert!((a.re - b.re).abs() < 1e-4, "{} vs {}", a.re, b.re);
        }
    }

    #[test]
    fn argmax_finds_single_tone_bin() {
        let n = 128;
        let engine = FftEngine::new(n);
        let bin = 17;
        let mut buf: Vec<Complex32> = (0..n)
            .map(|i| {
                let phase = 2.0 * std::f32::consts::PI * bin as f32 * i as f32 / n as f32;
                Complex32::new(phase.cos(), phase.sin())
            })
            .collect();
        engine.forward(&mut buf);
        assert_eq!(argmax_magnitude(&buf).0, bin);
    }
}
