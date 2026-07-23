//! Реалистичный акустический тракт «через воздух»: то, чего не хватало симуляции и из-за
//! чего тесты были зелёными, а связь между устройствами не работала.
//!
//! Старые каналы ([`super::awgn`]/[`super::multipath`]/[`super::clock_drift`]) СПЕКТРАЛЬНО
//! ПЛОСКИЕ: одинаково пропускают и 1 кГц, и 14 кГц. Реальные динамик и микрофон — нет. У
//! типового ноутбука/телефона рабочая полоса грубо 300–8000 Гц: ниже ~200 Гц не тянет
//! динамик, выше ~8–10 кГц заваливается и динамик, и микрофон (а на телефоне голосовой
//! тракт ОС режет всё выше ~4 кГц). Плюс на громкости динамик нелинейно искажает (клиппинг
//! пиков). Из-за этого верхняя FDD-полоса (7.8–15 кГц) на реальном железе почти не звучит —
//! отсюда «связь не работает ни в какую сторону»: данные-то в нижней полосе проходят, а
//! обратный ACK в верхней — нет.
//!
//! Здесь это моделируется каскадом биквадов (полосовая АЧХ обоих преобразователей) плюс
//! мягкий клиппинг. Модель нарочно консервативна: если кадр проходит через неё, он пройдёт
//! и через реальную пару «динамик–микрофон».

use super::{AwgnChannel, MultipathChannel};

/// Биквад (RBJ cookbook), stateful по одному каналу сэмплов.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn from_coeffs(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn lowpass(fc: f32, fs: f32, q: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * fc / fs;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let b1 = 1.0 - cos;
        Biquad::from_coeffs(b1 * 0.5, b1, b1 * 0.5, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    fn highpass(fc: f32, fs: f32, q: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * fc / fs;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let b1 = -(1.0 + cos);
        Biquad::from_coeffs((1.0 + cos) * 0.5, b1, (1.0 + cos) * 0.5, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Полосовая АЧХ одного преобразователя (динамика ИЛИ микрофона): ВЧ-срез снизу + пара
/// НЧ-срезов сверху для крутого спада за верхней границей.
#[derive(Clone)]
pub struct DeviceResponse {
    stages: Vec<Biquad>,
}

impl DeviceResponse {
    /// Типовой потребительский тракт (ноутбук/телефон в обычном режиме): полоса ~250–8000 Гц.
    pub fn consumer(sample_rate: f32) -> Self {
        DeviceResponse {
            stages: vec![
                Biquad::highpass(250.0, sample_rate, 0.7),
                Biquad::lowpass(8000.0, sample_rate, 0.7),
                Biquad::lowpass(8000.0, sample_rate, 0.9),
            ],
        }
    }

    /// Худший случай — телефон с голосовой обработкой ОС: полоса ~300–3800 Гц. Именно так
    /// ведёт себя микрофон в voice-режиме Android (см. память android-signing/acoustic).
    pub fn voice_limited(sample_rate: f32) -> Self {
        DeviceResponse {
            stages: vec![
                Biquad::highpass(300.0, sample_rate, 0.7),
                Biquad::lowpass(3800.0, sample_rate, 0.7),
                Biquad::lowpass(3800.0, sample_rate, 0.9),
            ],
        }
    }

    pub fn apply(&self, signal: &[f32]) -> Vec<f32> {
        let mut stages = self.stages.clone();
        signal
            .iter()
            .map(|&x| {
                let mut s = x;
                for st in stages.iter_mut() {
                    s = st.process(s);
                }
                s
            })
            .collect()
    }
}

/// Мягкий клиппинг динамика на громкости: пики выше `drive` сжимаются (tanh-подобно). Это
/// вносит гармоники — на реальном железе именно они рассыпали узкополосные схемы у шкалы.
fn soft_clip(signal: &[f32], drive: f32) -> Vec<f32> {
    signal
        .iter()
        .map(|&x| {
            let d = x / drive;
            (drive * d.tanh()).clamp(-1.0, 1.0)
        })
        .collect()
}

/// Полный тракт «динамик → воздух → микрофон» для тестов, максимально близкий к реальности:
/// нелинейность динамика, полосы обоих преобразователей, реверберация комнаты, шум, лёгкая
/// АРУ микрофона. Если кадр проходит это — он проходит и вживую.
pub struct OverTheAir {
    speaker: DeviceResponse,
    mic: DeviceResponse,
    reverb: MultipathChannel,
    snr_db: f32,
    drive: f32,
    seed: u64,
}

impl OverTheAir {
    /// Типовая пара устройств рядом на столе: хороший, но не идеальный SNR.
    pub fn typical(sample_rate: f32, seed: u64) -> Self {
        OverTheAir {
            speaker: DeviceResponse::consumer(sample_rate),
            mic: DeviceResponse::consumer(sample_rate),
            // Близкая дистанция (устройства на столе): разброс задержек ~2 мс — ранние
            // отражения от поверхности, прямой путь доминирует.
            reverb: MultipathChannel::exponential((sample_rate * 0.002) as usize, sample_rate * 0.0004),
            snr_db: 18.0,
            // Динамик слегка нелинеен на громкости, но TX держит −3 дБ запас (пик 0.7),
            // поэтому компрессия мягкая (пик сжимается лишь на ~6%), а не пороговый клиппинг.
            drive: 1.5,
            seed,
        }
    }

    /// Худший случай: оба конца в голосовом режиме (узкая полоса), громкий динамик
    /// (сильнее клиппинг), шумнее.
    pub fn harsh_voice(sample_rate: f32, seed: u64) -> Self {
        OverTheAir {
            speaker: DeviceResponse::voice_limited(sample_rate),
            mic: DeviceResponse::voice_limited(sample_rate),
            // Чуть больше разброс (~3 мс), но в пределах защитного интервала MFSK (8 мс).
            reverb: MultipathChannel::exponential((sample_rate * 0.003) as usize, sample_rate * 0.0006),
            snr_db: 12.0,
            // Громче/хуже динамик — заметнее нелинейность, но всё ещё не жёсткий клиппинг.
            drive: 1.0,
            seed,
        }
    }

    pub fn with_snr(mut self, snr_db: f32) -> Self {
        self.snr_db = snr_db;
        self
    }

    pub fn apply(&self, tx: &[f32]) -> Vec<f32> {
        // 1. Динамик: нелинейность на громкости, затем его АЧХ.
        let clipped = soft_clip(tx, self.drive);
        let spk = self.speaker.apply(&clipped);
        // 2. Комната: реверберация.
        let room = self.reverb.apply(&spk);
        // 3. Микрофон: его АЧХ.
        let mut miced = self.mic.apply(&room);
        // 4. Лёгкая АРУ микрофона: приводим к разумному уровню (демод инвариантен к масштабу,
        //    но так тест ближе к реальному входу и проверяет устойчивость порогов).
        let peak = miced.iter().fold(0.0f32, |a, &x| a.max(x.abs()));
        if peak > 1e-6 {
            let g = 0.3 / peak;
            for s in miced.iter_mut() {
                *s *= g;
            }
        }
        // 5. Аддитивный шум комнаты.
        AwgnChannel::new(self.seed).apply(&miced, self.snr_db)
    }
}

/// Энергия сигнала в полосе [lo, hi] Гц относительно полной энергии — грубая оценка «сколько
/// от кадра переживает АЧХ железа». Через Гёрцель-подобную сумму по сетке частот.
pub fn band_energy_fraction(signal: &[f32], sample_rate: f32, lo: f32, hi: f32) -> f32 {
    // Простая ДПФ-оценка на грубой сетке (достаточно для диагностики в тестах).
    let n = signal.len().min(8192);
    if n == 0 {
        return 0.0;
    }
    let s = &signal[..n];
    let mut in_band = 0.0f32;
    let mut total = 0.0f32;
    let step = (sample_rate / n as f32).max(1.0);
    let mut f = 0.0f32;
    while f < sample_rate / 2.0 {
        let w = 2.0 * std::f32::consts::PI * f / sample_rate;
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (i, &x) in s.iter().enumerate() {
            let p = w * i as f32;
            re += x * p.cos();
            im -= x * p.sin();
        }
        let e = re * re + im * im;
        total += e;
        if f >= lo && f <= hi {
            in_band += e;
        }
        f += step;
    }
    if total < 1e-12 {
        0.0
    } else {
        in_band / total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Тон в рабочей полосе проходит, тон за ней — глохнет: базовая проверка модели.
    #[test]
    fn consumer_passes_midband_blocks_highband() {
        let fs = 48_000.0;
        let resp = DeviceResponse::consumer(fs);
        let tone = |f: f32| -> Vec<f32> {
            (0..8192)
                .map(|i| (2.0 * std::f32::consts::PI * f * i as f32 / fs).sin())
                .collect()
        };
        let mid = resp.apply(&tone(2000.0));
        let high = resp.apply(&tone(13000.0));
        let rms = |s: &[f32]| (s[2000..].iter().map(|x| x * x).sum::<f32>() / (s.len() - 2000) as f32).sqrt();
        let mid_rms = rms(&mid);
        let high_rms = rms(&high);
        assert!(mid_rms > 0.5, "midband tone lost: {mid_rms}");
        assert!(high_rms < mid_rms * 0.2, "highband tone not attenuated: {high_rms} vs {mid_rms}");
    }

    /// ГПСЧ-детерминизм модели: один и тот же seed — один и тот же выход.
    #[test]
    fn over_the_air_is_deterministic() {
        let fs = 48_000.0;
        let mut rng = super::super::Rng::new(5);
        let tx: Vec<f32> = (0..4000).map(|_| rng.next_gaussian() * 0.3).collect();
        let a = OverTheAir::typical(fs, 1).apply(&tx);
        let b = OverTheAir::typical(fs, 1).apply(&tx);
        assert_eq!(a.len(), b.len());
        assert!(a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-9));
    }
}
