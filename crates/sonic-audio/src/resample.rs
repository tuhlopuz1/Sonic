//! Потоковый ресемплер между частотой железа и канонической частотой DSP.
//!
//! Зачем: в shared-режиме WASAPI каждое устройство залочено на свою частоту из настроек
//! ОС — микрофон вполне может быть на 44100, а динамик на 48000. Общей частоты может не
//! быть вообще, поэтому весь протокольный DSP крутится на ОДНОЙ канонической частоте
//! (48 кГц, PROTOCOL.md §2.2), а вход/выход подгоняются здесь.
//!
//! Гонять DSP прямо на частоте железа нельзя: у OFDM разнос поднесущих = `sr/N`
//! (46.875 Гц при 48 кГц против 43.07 Гц при 44.1 кГц) — приёмник не попал бы в сетку
//! передатчика. CSS бы выжил, OFDM — нет.
//!
//! Интерполяция линейная: полоса сигнала (≤15 кГц) сильно ниже Найквиста, спад АЧХ
//! плавный и поглощается пилот-эквалайзером OFDM, а внеполосные образы срезает
//! полосовой фильтр даунконверсии. Апгрейд до полифазного/sinc (`rubato`) — очевидный
//! следующий шаг, если понадобится качество.

/// Потоковый ресемплер с сохранением состояния между блоками.
pub struct Resampler {
    /// Сколько исходных сэмплов приходится на один выходной.
    ratio: f64,
    /// Дробная позиция чтения внутри текущего блока (с учётом хвоста предыдущего).
    pos: f64,
    /// Последний сэмпл предыдущего блока — «индекс −1» для непрерывной интерполяции.
    prev: f32,
    identity: bool,
}

impl Resampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        let src = src_rate.max(1) as f64;
        let dst = dst_rate.max(1) as f64;
        Resampler {
            ratio: src / dst,
            pos: 0.0,
            prev: 0.0,
            identity: src_rate == dst_rate,
        }
    }

    /// Частоты совпали — можно копировать без обработки.
    pub fn is_identity(&self) -> bool {
        self.identity
    }

    /// Ресемплирует блок, дописывая результат в `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        if self.identity {
            out.extend_from_slice(input);
            return;
        }

        // Виртуальный буфер: [prev, input...] — так интерполяция не рвётся на границе блоков.
        let n = input.len() + 1;
        let at = |i: usize| -> f32 {
            if i == 0 {
                self.prev
            } else {
                input[i - 1]
            }
        };

        while self.pos + 1.0 < n as f64 {
            let idx = self.pos.floor() as usize;
            let frac = (self.pos - idx as f64) as f32;
            out.push(at(idx) * (1.0 - frac) + at(idx + 1) * frac);
            self.pos += self.ratio;
        }

        // Переносим начало координат на последний сэмпл блока (он станет новым `prev`).
        self.pos -= (n - 1) as f64;
        if self.pos < 0.0 {
            self.pos = 0.0;
        }
        self.prev = input[input.len() - 1];
    }

    /// Удобная обёртка для разовой обработки целого буфера (например, одного кадра).
    pub fn process_all(&mut self, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(
            ((input.len() as f64 / self.ratio).ceil() as usize).saturating_add(2),
        );
        self.process(input, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn sine(n: usize, freq: f32, sr: f32) -> Vec<f32> {
        (0..n).map(|i| (2.0 * PI * freq * i as f32 / sr).sin()).collect()
    }

    #[test]
    fn identity_when_rates_match() {
        let mut r = Resampler::new(48_000, 48_000);
        assert!(r.is_identity());
        let input = sine(1000, 1000.0, 48_000.0);
        assert_eq!(r.process_all(&input), input);
    }

    #[test]
    fn upsampling_produces_expected_length() {
        // 44100 → 48000: выходных сэмплов должно быть примерно в 48/44.1 раза больше.
        let mut r = Resampler::new(44_100, 48_000);
        let input = sine(44_100, 1000.0, 44_100.0);
        let out = r.process_all(&input);
        let expected = (44_100.0 * 48_000.0 / 44_100.0) as usize;
        assert!(
            (out.len() as i64 - expected as i64).abs() < 4,
            "len {} vs expected {expected}",
            out.len()
        );
    }

    #[test]
    fn streaming_matches_single_shot() {
        // Обработка по кускам должна давать столько же сэмплов, сколько разом —
        // иначе состояние на границах блоков теряется и звук «щёлкает».
        let input = sine(20_000, 3000.0, 44_100.0);
        let mut whole = Resampler::new(44_100, 48_000);
        let a = whole.process_all(&input);

        let mut chunked = Resampler::new(44_100, 48_000);
        let mut b = Vec::new();
        for chunk in input.chunks(512) {
            chunked.process(chunk, &mut b);
        }
        assert!((a.len() as i64 - b.len() as i64).abs() <= 1, "{} vs {}", a.len(), b.len());
        // И совпадать по значениям (кроме возможного последнего сэмпла).
        let n = a.len().min(b.len()) - 1;
        for i in 0..n {
            assert!((a[i] - b[i]).abs() < 1e-4, "sample {i}: {} vs {}", a[i], b[i]);
        }
    }

    #[test]
    fn preserves_tone_frequency() {
        // 1 кГц при 44100 → после ресемплинга в 48000 остаётся 1 кГц:
        // считаем переходы через ноль.
        let mut r = Resampler::new(44_100, 48_000);
        let input = sine(44_100, 1000.0, 44_100.0);
        let out = r.process_all(&input);
        let crossings = out.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        // 1 секунда сигнала → ~1000 положительных переходов через ноль.
        assert!((crossings as i64 - 1000).abs() <= 2, "crossings {crossings}");
    }

    #[test]
    fn downsampling_works() {
        let mut r = Resampler::new(48_000, 44_100);
        let input = sine(48_000, 1000.0, 48_000.0);
        let out = r.process_all(&input);
        assert!((out.len() as i64 - 44_100).abs() < 4, "len {}", out.len());
        let crossings = out.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        assert!((crossings as i64 - 1000).abs() <= 2, "crossings {crossings}");
    }
}
