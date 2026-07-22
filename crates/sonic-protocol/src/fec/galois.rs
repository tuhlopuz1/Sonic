//! Арифметика поля Галуа GF(2^8) для Reed-Solomon.
//!
//! Примитивный многочлен 0x11D (x^8+x^4+x^3+x^2+1), генератор α=2 — стандартная связка
//! (та же, что в QR-кодах/DVB). exp/log таблицы строятся один раз лениво.
//! Многочлены везде хранятся старшим коэффициентом вперёд (index 0 = высшая степень).

use std::sync::OnceLock;

const PRIM: u16 = 0x11D;

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for i in 0..255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= PRIM;
            }
        }
        // Удвоенная exp-таблица — чтобы складывать логарифмы без взятия по модулю.
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        Tables { exp, log }
    })
}

#[inline]
pub fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let t = tables();
    t.exp[t.log[a as usize] as usize + t.log[b as usize] as usize]
}

#[inline]
pub fn div(a: u8, b: u8) -> u8 {
    debug_assert!(b != 0, "division by zero in GF(256)");
    if a == 0 {
        return 0;
    }
    let t = tables();
    t.exp[(t.log[a as usize] as usize + 255 - t.log[b as usize] as usize) % 255]
}

#[inline]
pub fn inverse(a: u8) -> u8 {
    debug_assert!(a != 0, "inverse of zero in GF(256)");
    let t = tables();
    t.exp[255 - t.log[a as usize] as usize]
}

/// α^power для генератора α=2, power может быть отрицательным (для Форни).
#[inline]
pub fn pow(x: u8, power: i32) -> u8 {
    let t = tables();
    if x == 0 {
        return 0;
    }
    let idx = ((t.log[x as usize] as i32 * power).rem_euclid(255)) as usize;
    t.exp[idx]
}

// --- операции над многочленами (старший коэффициент вперёд) ---

pub fn poly_scale(p: &[u8], x: u8) -> Vec<u8> {
    p.iter().map(|&c| mul(c, x)).collect()
}

pub fn poly_add(p: &[u8], q: &[u8]) -> Vec<u8> {
    let len = p.len().max(q.len());
    let mut r = vec![0u8; len];
    for (i, &c) in p.iter().enumerate() {
        r[i + len - p.len()] = c;
    }
    for (i, &c) in q.iter().enumerate() {
        r[i + len - q.len()] ^= c;
    }
    r
}

pub fn poly_mul(p: &[u8], q: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; p.len() + q.len() - 1];
    for (j, &qj) in q.iter().enumerate() {
        for (i, &pi) in p.iter().enumerate() {
            r[i + j] ^= mul(pi, qj);
        }
    }
    r
}

/// Схема Горнера: значение многочлена в точке `x`.
pub fn poly_eval(p: &[u8], x: u8) -> u8 {
    let mut y = p[0];
    for &c in &p[1..] {
        y = mul(y, x) ^ c;
    }
    y
}

/// Деление многочленов, возвращает (частное, остаток). Синтетическое деление в GF(256).
pub fn poly_div(dividend: &[u8], divisor: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut out = dividend.to_vec();
    for i in 0..(dividend.len() - (divisor.len() - 1)) {
        let coef = out[i];
        if coef != 0 {
            for j in 1..divisor.len() {
                if divisor[j] != 0 {
                    out[i + j] ^= mul(divisor[j], coef);
                }
            }
        }
    }
    let sep = out.len() - (divisor.len() - 1);
    let remainder = out[sep..].to_vec();
    let quotient = out[..sep].to_vec();
    (quotient, remainder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_are_inverse() {
        for a in 1u8..=255 {
            for b in 1u8..=255 {
                let p = mul(a, b);
                assert_eq!(div(p, b), a);
            }
        }
    }

    #[test]
    fn inverse_is_correct() {
        for a in 1u8..=255 {
            assert_eq!(mul(a, inverse(a)), 1);
        }
    }

    #[test]
    fn pow_matches_repeated_mul() {
        let mut acc = 1u8;
        for p in 0..20 {
            assert_eq!(pow(2, p), acc);
            acc = mul(acc, 2);
        }
        // Отрицательная степень = обратный элемент.
        assert_eq!(pow(2, -1), inverse(2));
    }
}
