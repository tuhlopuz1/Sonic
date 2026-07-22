//! Симулированный канал для тестов и BER/throughput-замеров (plan.md §2, «sim/»).
//!
//! Двойное назначение: инфраструктура `cargo test` (модемы гоняются без железа) и
//! источник объективных цифр для жюри (`examples/ber_sweep.rs`). Три эффекта реального
//! акустического канала:
//! - [`awgn`] — аддитивный гауссов шум на заданный SNR;
//! - [`multipath`] — многолучёвость (FIR-свёртка, реверберация комнаты);
//! - [`clock_drift`] — рассинхрон частоты дискретизации двух независимых звуковых карт.

pub mod awgn;
pub mod clock_drift;
pub mod multipath;

pub use awgn::AwgnChannel;
pub use clock_drift::resample_ppm;
pub use multipath::MultipathChannel;

/// Детерминированный ГПСЧ (xorshift64*) — без внешних крейтов, чтобы тесты были
/// воспроизводимы и не тянули зависимостей в чистое DSP-ядро.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
    spare: Option<f32>,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng {
            state: seed.max(1),
            spare: None,
        }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Равномерное [0,1).
    #[inline]
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Стандартное нормальное N(0,1) через Бокса-Мюллера (с кэшем второй величины).
    pub fn next_gaussian(&mut self) -> f32 {
        if let Some(s) = self.spare.take() {
            return s;
        }
        let u1 = self.next_f32().max(1e-9);
        let u2 = self.next_f32();
        let mag = (-2.0 * u1.ln()).sqrt();
        let (s, c) = (2.0 * std::f32::consts::PI * u2).sin_cos();
        self.spare = Some(mag * s);
        mag * c
    }
}

/// Средняя мощность сигнала (для расчёта уровня шума и самопроверки SNR).
pub fn power(signal: &[f32]) -> f32 {
    if signal.is_empty() {
        return 0.0;
    }
    signal.iter().map(|&s| s * s).sum::<f32>() / signal.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaussian_has_expected_moments() {
        let mut rng = Rng::new(42);
        let n = 100_000;
        let samples: Vec<f32> = (0..n).map(|_| rng.next_gaussian()).collect();
        let mean: f32 = samples.iter().sum::<f32>() / n as f32;
        let var: f32 = samples.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n as f32;
        assert!(mean.abs() < 0.02, "mean {mean}");
        assert!((var - 1.0).abs() < 0.05, "var {var}");
    }
}
