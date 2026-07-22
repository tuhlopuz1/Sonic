//! Многолучёвость: FIR-свёртка сигнала с импульсной характеристикой канала.
//!
//! Звук идёт ~343 м/с, поэтому отражения от стен даже в паре метров дают
//! многомиллисекундный разброс задержек — часто больше, чем у RF относительно
//! длительности символа (plan.md §2). Модель — синтетическая экспоненциальная (или
//! измеренный RIR из WAV): проверяет, что CP OFDM достаточен, а CSS устойчив.

/// Канал с конечной импульсной характеристикой (RIR).
#[derive(Debug, Clone)]
pub struct MultipathChannel {
    /// Импульсная характеристика (tap[0] — прямой путь).
    taps: Vec<f32>,
}

impl MultipathChannel {
    /// Произвольная импульсная характеристика (например, измеренная из WAV через `hound`).
    pub fn from_impulse_response(taps: Vec<f32>) -> Self {
        MultipathChannel { taps }
    }

    /// Синтетическая экспоненциальная модель реверберации: прямой путь + затухающий хвост
    /// длиной `len` сэмплов с постоянной времени `decay_samples`.
    pub fn exponential(len: usize, decay_samples: f32) -> Self {
        let mut taps = vec![0.0f32; len.max(1)];
        let mut energy = 0.0f32;
        for (i, t) in taps.iter_mut().enumerate() {
            *t = (-(i as f32) / decay_samples).exp();
            energy += *t * *t;
        }
        // Нормируем на единичную энергию — канал не меняет общий уровень сигнала.
        let norm = energy.sqrt().max(1e-9);
        for t in taps.iter_mut() {
            *t /= norm;
        }
        MultipathChannel { taps }
    }

    /// Разброс задержек = длина импульсной характеристики (в сэмплах).
    pub fn delay_spread(&self) -> usize {
        self.taps.len().saturating_sub(1)
    }

    /// Свёртка сигнала с RIR (полная линейная свёртка, длина растёт на len(taps)-1).
    pub fn apply(&self, signal: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0f32; signal.len() + self.taps.len() - 1];
        for (i, &s) in signal.iter().enumerate() {
            for (k, &t) in self.taps.iter().enumerate() {
                out[i + k] += s * t;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_path_only_is_passthrough() {
        let ch = MultipathChannel::from_impulse_response(vec![1.0]);
        let sig = vec![0.1, -0.2, 0.3, 0.4];
        assert_eq!(ch.apply(&sig)[..sig.len()], sig[..]);
    }

    #[test]
    fn exponential_is_unit_energy_and_has_tail() {
        let ch = MultipathChannel::exponential(64, 12.0);
        assert!(ch.delay_spread() > 0);
        let energy: f32 = ch.taps.iter().map(|t| t * t).sum();
        assert!((energy - 1.0).abs() < 1e-4);
    }
}
