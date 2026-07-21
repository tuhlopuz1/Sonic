//! Минимальный акустический "маячок" для обнаружения устройств без всякой сети:
//! никнейм кодируется простым FSK (один тон на слот, Goertzel на приёме) и рассылается
//! через динамик; другие устройства ищут маячок в записи с микрофона и декодируют его.
//! Координация раундов в `discovery.rs` тоже полностью акустическая — иначе теряется
//! смысл "передать данные, когда обычной связи нет" (см. `task.md`).
//!
//! Это не CSS/OFDM из `plan.md` — совсем простое FSK для короткого ID, не для данных.

pub(crate) const NICK_MAX_LEN: usize = 6;
const SLOT_MS: u64 = 110;
const GUARD_MS: u64 = 25;
const SYNC_SLOTS: usize = 3;
const DATA_SLOTS: usize = NICK_MAX_LEN * 2; // 2 нибл-тона на байт никнейма
const BEACON_SLOTS: usize = SYNC_SLOTS + DATA_SLOTS;
const BASE_FREQ_HZ: f32 = 2000.0;
const FREQ_STEP_HZ: f32 = 375.0; // 17 тонов: 0 = sync, 1..=16 = нибблы 0x0..0xF
const DOMINANCE_THRESHOLD: f32 = 4.0;
const PAD_CHAR: u8 = b'_';

pub(crate) fn beacon_duration_ms() -> u64 {
    BEACON_SLOTS as u64 * (SLOT_MS + GUARD_MS)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DecodedBeacon {
    pub nickname: String,
    pub snr_db: f32,
}

fn tone_freq(index: usize) -> f32 {
    BASE_FREQ_HZ + FREQ_STEP_HZ * index as f32
}

fn normalize_nickname(nickname: &str) -> [u8; NICK_MAX_LEN] {
    let mut out = [PAD_CHAR; NICK_MAX_LEN];
    let cleaned: Vec<u8> = nickname
        .to_ascii_uppercase()
        .bytes()
        .filter(|b| b.is_ascii_alphanumeric())
        .take(NICK_MAX_LEN)
        .collect();
    out[..cleaned.len()].copy_from_slice(&cleaned);
    out
}

/// То, во что превратится `nickname` после кодирования/декодирования — используется,
/// чтобы отфильтровать собственный маячок, случайно услышанный самим устройством.
pub(crate) fn canonicalize(nickname: &str) -> String {
    let bytes = normalize_nickname(nickname);
    String::from_utf8_lossy(&bytes)
        .trim_end_matches(PAD_CHAR as char)
        .to_string()
}

fn nickname_to_nibbles(nickname: &str) -> Vec<u8> {
    normalize_nickname(nickname)
        .iter()
        .flat_map(|&b| [b >> 4, b & 0x0F])
        .collect()
}

fn nibbles_to_nickname(nibbles: &[u8]) -> String {
    let bytes: Vec<u8> = nibbles
        .chunks(2)
        .map(|pair| (pair[0] << 4) | pair.get(1).copied().unwrap_or(0))
        .collect();
    String::from_utf8_lossy(&bytes)
        .trim_end_matches(PAD_CHAR as char)
        .to_string()
}

fn render_tone_slot(sample_rate: f32, freq: f32, out: &mut Vec<f32>) {
    let slot_n = ((sample_rate * SLOT_MS as f32) / 1000.0) as usize;
    let guard_n = ((sample_rate * GUARD_MS as f32) / 1000.0) as usize;
    let fade_n = (slot_n / 8).max(1);
    for i in 0..slot_n {
        let t = i as f32 / sample_rate;
        let mut s = (2.0 * std::f32::consts::PI * freq * t).sin();
        if i < fade_n {
            s *= i as f32 / fade_n as f32;
        } else if i >= slot_n - fade_n {
            s *= (slot_n - i) as f32 / fade_n as f32;
        }
        out.push(s * 0.85);
    }
    out.resize(out.len() + guard_n, 0.0);
}

pub(crate) fn generate_beacon_signal(sample_rate: f32, nickname: &str) -> Vec<f32> {
    let nibbles = nickname_to_nibbles(nickname);
    let mut buf = Vec::new();
    for _ in 0..SYNC_SLOTS {
        render_tone_slot(sample_rate, tone_freq(0), &mut buf);
    }
    for &nib in &nibbles {
        render_tone_slot(sample_rate, tone_freq(1 + nib as usize), &mut buf);
    }
    buf
}

fn sync_score(
    samples: &[f32],
    start: usize,
    sample_rate: f32,
    period_n: usize,
    trim: usize,
    analyze_len: usize,
) -> Option<f32> {
    let mut sync_total = 0.0f32;
    let mut other_total = 0.0f32;
    for slot_idx in 0..SYNC_SLOTS {
        let slot_start = start + slot_idx * period_n + trim;
        if slot_start + analyze_len > samples.len() {
            return None;
        }
        let window = &samples[slot_start..slot_start + analyze_len];
        sync_total += crate::channel_check::goertzel_power(window, sample_rate, tone_freq(0));
        for tone_idx in 0..16 {
            other_total += crate::channel_check::goertzel_power(window, sample_rate, tone_freq(1 + tone_idx));
        }
    }
    let sync_avg = sync_total / SYNC_SLOTS as f32;
    let other_avg = other_total / (SYNC_SLOTS * 16) as f32;
    Some(sync_avg / other_avg.max(1e-12))
}

/// Ищет и декодирует все маячки, попавшие в буфер (может быть несколько от разных
/// устройств за один раунд). `noise_floor_rms` — широкополосный шумовой пол этого же
/// устройства (тишина перед раундом), опорный уровень для SNR декодированного маячка.
pub(crate) fn decode_beacons_from_buffer(
    samples: &[f32],
    sample_rate: f32,
    noise_floor_rms: f32,
) -> Vec<DecodedBeacon> {
    let slot_n = ((sample_rate * SLOT_MS as f32) / 1000.0) as usize;
    let guard_n = ((sample_rate * GUARD_MS as f32) / 1000.0) as usize;
    let period_n = slot_n + guard_n;
    let trim = (slot_n / 8).max(1);
    let analyze_len = slot_n.saturating_sub(2 * trim).max(1);
    let hop_n = ((sample_rate * 0.01) as usize).max(1);
    let beacon_len_n = period_n * BEACON_SLOTS;

    if samples.len() < beacon_len_n {
        return Vec::new();
    }

    let noise_power_ref = (noise_floor_rms * noise_floor_rms).max(1e-12);
    let mut consumed = vec![false; samples.len()];
    let mut results = Vec::new();

    loop {
        let mut best_score = 0.0f32;
        let mut best_pos = None;
        let mut pos = 0usize;
        while pos + beacon_len_n <= samples.len() {
            if !consumed[pos] {
                if let Some(score) = sync_score(samples, pos, sample_rate, period_n, trim, analyze_len) {
                    if score > best_score {
                        best_score = score;
                        best_pos = Some(pos);
                    }
                }
            }
            pos += hop_n;
        }

        let (Some(start), true) = (best_pos, best_score >= DOMINANCE_THRESHOLD) else {
            break;
        };

        let mut nibbles = Vec::with_capacity(DATA_SLOTS);
        let mut sync_power_sum = 0.0f32;
        for slot_idx in 0..BEACON_SLOTS {
            let slot_start = start + slot_idx * period_n + trim;
            let window = &samples[slot_start..slot_start + analyze_len];
            if slot_idx < SYNC_SLOTS {
                sync_power_sum += crate::channel_check::goertzel_power(window, sample_rate, tone_freq(0));
                continue;
            }
            let mut best_tone = 0usize;
            let mut best_power = f32::MIN;
            for tone_idx in 0..16 {
                let p = crate::channel_check::goertzel_power(window, sample_rate, tone_freq(1 + tone_idx));
                if p > best_power {
                    best_power = p;
                    best_tone = tone_idx;
                }
            }
            nibbles.push(best_tone as u8);
        }

        let nickname = nibbles_to_nickname(&nibbles);
        let sync_power_avg = sync_power_sum / SYNC_SLOTS as f32;
        let snr_db = 10.0 * (sync_power_avg / noise_power_ref).max(1e-6).log10();

        if !nickname.is_empty() {
            results.push(DecodedBeacon { nickname, snr_db });
        }

        // Маскируем с запасом в обе стороны: любая позиция, чьё окно анализа хоть
        // немного перекрывается с только что декодированным маячком, тоже "занята" —
        // иначе один и тот же физический сигнал при небольшом рассинхроне hop-сетки
        // декодируется повторно (с испорченными нибблами) как "другой" маячок.
        let mask_from = start.saturating_sub(beacon_len_n);
        let mask_to = (start + beacon_len_n).min(consumed.len());
        for slot in consumed.iter_mut().take(mask_to).skip(mask_from) {
            *slot = true;
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Простой линейный конгруэнтный генератор — без внешних крейтов для теста.
    struct Lcg(u64);
    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    fn awgn(len: usize, amplitude: f32, seed: u64) -> Vec<f32> {
        let mut rng = Lcg(seed);
        (0..len).map(|_| rng.next_f32() * amplitude).collect()
    }

    #[test]
    fn round_trip_clean() {
        let sample_rate = 48000.0;
        let signal = generate_beacon_signal(sample_rate, "Alex-1");
        let noise = awgn(signal.len() + 4800, 0.01, 42);
        let mut buf = noise;
        for (i, &s) in signal.iter().enumerate() {
            buf[2400 + i] += s;
        }
        let decoded = decode_beacons_from_buffer(&buf, sample_rate, 0.01);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].nickname, "ALEX1");
        assert!(decoded[0].snr_db > 10.0, "snr_db = {}", decoded[0].snr_db);
    }

    #[test]
    fn no_false_positive_on_pure_noise() {
        let sample_rate = 48000.0;
        let buf = awgn(96000, 0.02, 7);
        let decoded = decode_beacons_from_buffer(&buf, sample_rate, 0.02);
        assert!(decoded.is_empty(), "decoded {:?} from pure noise", decoded);
    }

    #[test]
    fn two_beacons_in_one_buffer() {
        let sample_rate = 48000.0;
        let a = generate_beacon_signal(sample_rate, "AAAAAA");
        let b = generate_beacon_signal(sample_rate, "BBBBBB");
        let total_len = a.len() + b.len() + 20000;
        let mut buf = awgn(total_len, 0.01, 99);
        for (i, &s) in a.iter().enumerate() {
            buf[1000 + i] += s;
        }
        let b_start = a.len() + 8000;
        for (i, &s) in b.iter().enumerate() {
            buf[b_start + i] += s;
        }
        let decoded = decode_beacons_from_buffer(&buf, sample_rate, 0.01);
        let mut names: Vec<String> = decoded.iter().map(|d| d.nickname.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["AAAAAA".to_string(), "BBBBBB".to_string()]);
    }

    #[test]
    fn canonicalize_matches_decode() {
        assert_eq!(canonicalize("alex!!"), "ALEX");
        assert_eq!(canonicalize("Sonic-Test-Long"), "SONICT");
    }
}
