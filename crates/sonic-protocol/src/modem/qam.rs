//! QPSK / 16-QAM (де)маппер с кодом Грея (PROTOCOL.md §5.2).
//!
//! Грей-кодирование: соседние точки созвездия отличаются одним битом, поэтому одиночная
//! ошибка решения в шумном канале портит минимум бит. Жёсткие решения на приёме
//! (ближайшая точка) — дальше их подхватывает Reed-Solomon ([`crate::fec`]).

use num_complex::Complex32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modulation {
    Qpsk,
    Qam16,
}

impl Modulation {
    pub fn bits_per_symbol(self) -> usize {
        match self {
            Modulation::Qpsk => 2,
            Modulation::Qam16 => 4,
        }
    }
}

// Грей-порядок амплитуд для 2 бит: 00→-3, 01→-1, 11→+1, 10→+3.
const GRAY2_TO_LEVEL: [f32; 4] = [-3.0, -1.0, 3.0, 1.0]; // индекс = биты b1b0
const QAM16_SCALE: f32 = 0.316_227_77; // 1/sqrt(10)
const QPSK_SCALE: f32 = 0.707_106_77; // 1/sqrt(2)

/// 2 бита (b1,b0) → уровень по одной оси (Грей).
fn gray2_level(b1: u8, b0: u8) -> f32 {
    GRAY2_TO_LEVEL[((b1 << 1) | b0) as usize]
}

/// Ближайший уровень {-3,-1,1,3} → 2 бита (обратный Грей).
fn level_to_gray2(v: f32) -> (u8, u8) {
    // Решаем по ближайшему из {-3,-1,1,3}, затем обратный маппинг.
    let level = if v < -2.0 {
        -3.0
    } else if v < 0.0 {
        -1.0
    } else if v < 2.0 {
        1.0
    } else {
        3.0
    };
    let idx = GRAY2_TO_LEVEL.iter().position(|&x| x == level).unwrap() as u8;
    (idx >> 1, idx & 1)
}

/// Маппинг потока бит (0/1) в комплексные символы созвездия. Длина `bits` должна быть
/// кратна `bits_per_symbol` (вызывающий добивает нулями).
pub fn map(bits: &[u8], modulation: Modulation) -> Vec<Complex32> {
    match modulation {
        Modulation::Qpsk => bits
            .chunks(2)
            .map(|c| {
                let i = if c[0] == 0 { 1.0 } else { -1.0 };
                let q = if c.get(1).copied().unwrap_or(0) == 0 { 1.0 } else { -1.0 };
                Complex32::new(i * QPSK_SCALE, q * QPSK_SCALE)
            })
            .collect(),
        Modulation::Qam16 => bits
            .chunks(4)
            .map(|c| {
                let g = |a: usize, b: usize| gray2_level(c[a], *c.get(b).unwrap_or(&0));
                Complex32::new(g(0, 1) * QAM16_SCALE, g(2, 3) * QAM16_SCALE)
            })
            .collect(),
    }
}

/// Обратный маппинг символов в биты (жёсткие решения).
pub fn demap(symbols: &[Complex32], modulation: Modulation) -> Vec<u8> {
    let mut bits = Vec::with_capacity(symbols.len() * modulation.bits_per_symbol());
    match modulation {
        Modulation::Qpsk => {
            for s in symbols {
                bits.push(if s.re >= 0.0 { 0 } else { 1 });
                bits.push(if s.im >= 0.0 { 0 } else { 1 });
            }
        }
        Modulation::Qam16 => {
            for s in symbols {
                let (i1, i0) = level_to_gray2(s.re / QAM16_SCALE);
                let (q1, q0) = level_to_gray2(s.im / QAM16_SCALE);
                bits.extend_from_slice(&[i1, i0, q1, q0]);
            }
        }
    }
    bits
}

/// Утилиты упаковки байт ↔ биты (MSB-first) — общие для OFDM.
pub fn bytes_to_bits(bytes: &[u8]) -> Vec<u8> {
    let mut bits = Vec::with_capacity(bytes.len() * 8);
    for &b in bytes {
        for i in (0..8).rev() {
            bits.push((b >> i) & 1);
        }
    }
    bits
}

pub fn bits_to_bytes(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| {
            let mut b = 0u8;
            for (i, &bit) in c.iter().enumerate() {
                b |= (bit & 1) << (7 - i);
            }
            b
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qpsk_roundtrip_clean() {
        let bits: Vec<u8> = (0..64).map(|i| (i % 2) as u8).collect();
        let syms = map(&bits, Modulation::Qpsk);
        assert_eq!(demap(&syms, Modulation::Qpsk), bits);
    }

    #[test]
    fn qam16_roundtrip_clean() {
        let bytes = b"16-QAM constellation test payload";
        let bits = bytes_to_bits(bytes);
        let syms = map(&bits, Modulation::Qam16);
        let back = bits_to_bytes(&demap(&syms, Modulation::Qam16));
        assert_eq!(&back[..bytes.len()], bytes);
    }

    #[test]
    fn qam16_is_gray_coded() {
        // Соседние по амплитуде точки должны отличаться одним битом.
        let levels: Vec<(f32, (u8, u8))> = GRAY2_TO_LEVEL
            .iter()
            .map(|&v| (v, level_to_gray2(v)))
            .collect();
        let mut sorted = levels.clone();
        sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        for w in sorted.windows(2) {
            let (b1a, b0a) = w[0].1;
            let (b1b, b0b) = w[1].1;
            let diff = (b1a ^ b1b) + (b0a ^ b0b);
            assert_eq!(diff, 1, "adjacent levels differ by !=1 bit");
        }
    }

    #[test]
    fn qam16_average_power_normalized() {
        // Все 16 точек: средняя мощность ≈ 1.
        let mut all_bits = Vec::new();
        for v in 0..16u8 {
            for i in (0..4).rev() {
                all_bits.push((v >> i) & 1);
            }
        }
        let syms = map(&all_bits, Modulation::Qam16);
        let avg_power: f32 = syms.iter().map(|c| c.norm_sqr()).sum::<f32>() / syms.len() as f32;
        assert!((avg_power - 1.0).abs() < 0.05, "avg power {avg_power}");
    }
}
