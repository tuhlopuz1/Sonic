//! Рассинхрон частоты дискретизации двух независимых звуковых карт.
//!
//! У дешёвых АЦП/ЦАП частота дискретизации «плывёт» на десятки–сотни ppm, поэтому
//! приёмник видит сигнал слегка растянутым/сжатым во времени. Эмулируем дробным
//! ресемплингом (линейная интерполяция). Устойчивость к этому — одно из ключевых
//! преимуществ CSS над OFDM, поэтому дрейф входит в стандартный свип тестов (plan.md §5).

/// Ресемплинг, эмулирующий дрейф `ppm` (parts-per-million). Положительный ppm →
/// приёмник дискретизирует чуть быстрее (сигнал во времени «сжимается»).
pub fn resample_ppm(signal: &[f32], ppm: f32) -> Vec<f32> {
    if signal.len() < 2 {
        return signal.to_vec();
    }
    // Отношение частот приёмника к передатчику.
    let ratio = 1.0 + ppm * 1e-6;
    let out_len = ((signal.len() as f64) / ratio as f64).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        // Позиция в исходном сигнале, которую «видит» приёмник на i-м своём отсчёте.
        let src = i as f32 * ratio;
        let idx = src.floor() as usize;
        let frac = src - idx as f32;
        if idx + 1 < signal.len() {
            out.push(signal[idx] * (1.0 - frac) + signal[idx + 1] * frac);
        } else {
            out.push(signal[idx.min(signal.len() - 1)]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn zero_ppm_is_identity() {
        let sig: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let out = resample_ppm(&sig, 0.0);
        assert_eq!(out.len(), sig.len());
        for (a, b) in sig.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-3);
        }
    }

    #[test]
    fn positive_ppm_compresses_time() {
        // 1000 ppm на 48000 сэмплов → примерно на 48 сэмплов короче.
        let sig: Vec<f32> = (0..48_000)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / 48_000.0).sin())
            .collect();
        let out = resample_ppm(&sig, 1000.0);
        let expected = (48_000.0 / 1.001) as usize;
        assert!((out.len() as i32 - expected as i32).abs() <= 2);
    }
}
