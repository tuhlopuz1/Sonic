//! Аддитивный белый гауссов шум на заданный SNR + самопроверка измеренного SNR.

use super::{power, Rng};

pub struct AwgnChannel {
    rng: Rng,
}

impl AwgnChannel {
    pub fn new(seed: u64) -> Self {
        AwgnChannel { rng: Rng::new(seed) }
    }

    /// Добавляет гауссов шум так, чтобы отношение мощности сигнала к мощности шума
    /// равнялось `snr_db`. Сигнал не должен быть тишиной.
    pub fn apply(&mut self, signal: &[f32], snr_db: f32) -> Vec<f32> {
        let sig_power = power(signal).max(1e-20);
        let snr_lin = 10f32.powf(snr_db / 10.0);
        let noise_power = sig_power / snr_lin;
        let sigma = noise_power.sqrt();
        signal
            .iter()
            .map(|&s| s + self.rng.next_gaussian() * sigma)
            .collect()
    }

    /// Только шум заданной мощности (для замера/самопроверки).
    pub fn noise(&mut self, len: usize, sigma: f32) -> Vec<f32> {
        (0..len).map(|_| self.rng.next_gaussian() * sigma).collect()
    }
}

/// Измеренный SNR (дБ) чистого сигнала относительно (принятый − чистый) как шум —
/// для самопроверки, что канал добавил именно столько шума, сколько заказано.
pub fn measured_snr_db(clean: &[f32], received: &[f32]) -> f32 {
    let noise: Vec<f32> = clean
        .iter()
        .zip(received.iter())
        .map(|(&c, &r)| r - c)
        .collect();
    let sp = power(clean).max(1e-20);
    let np = power(&noise).max(1e-20);
    10.0 * (sp / np).log10()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn tone(n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * PI * 1000.0 * i as f32 / 48000.0).sin()).collect()
    }

    #[test]
    fn achieves_requested_snr() {
        let clean = tone(48_000);
        let mut ch = AwgnChannel::new(7);
        for &target in &[0.0f32, 6.0, 12.0, 20.0] {
            let rx = ch.apply(&clean, target);
            let got = measured_snr_db(&clean, &rx);
            assert!((got - target).abs() < 0.5, "target {target}, got {got}");
        }
    }
}
