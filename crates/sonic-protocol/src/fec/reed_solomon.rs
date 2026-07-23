//! Reed-Solomon над GF(256) — систематический кодер + исправляющий ошибки декодер
//! (синдромы → Берлекэмп-Мэсси → поиск Ченя → Форни).
//!
//! Плановое решение (plan.md §2/§6): именно RS, не свёрточный код — ошибки демодуляции
//! в акустике пакетные на уровне символа (плохой FFT-пик CSS, битая поднесущая OFDM),
//! а RS(GF256) естественно чинит это как один символ. Реализация своя (не внешний
//! крейт), т.к. крейты с настоящей коррекцией ошибок менее зрелы (plan.md риск №4), а
//! контроль над «t ошибок чиним, t+1 → явный отказ» здесь критичен для ARQ.
//!
//! `nsym` = число проверочных байт = 2·t, где t — максимум исправляемых символьных ошибок.

use super::galois as gf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsError {
    /// Берлекэмп-Мэсси/Чень определили, что ошибок больше, чем можно исправить.
    TooManyErrors,
    /// Коррекция применена, но пере-проверка синдромов не сошлась — молчаливого
    /// неверного декодирования не допускаем (plan.md §6, от этого зависит ARQ).
    Uncorrectable,
}

/// Кодек RS с фиксированным числом проверочных символов `nsym`.
#[derive(Debug, Clone)]
pub struct RsCodec {
    nsym: usize,
    gen: Vec<u8>,
}

impl RsCodec {
    /// `nsym` проверочных байт → исправляем до `nsym/2` символьных ошибок.
    pub fn new(nsym: usize) -> Self {
        assert!(nsym > 0 && nsym < 255, "nsym out of range");
        RsCodec {
            nsym,
            gen: generator_poly(nsym),
        }
    }

    pub fn nsym(&self) -> usize {
        self.nsym
    }

    /// Максимум исправляемых символьных ошибок t = nsym/2.
    pub fn max_correctable(&self) -> usize {
        self.nsym / 2
    }

    /// Систематическое кодирование: `msg` (k байт) → k+nsym байт (msg || parity).
    pub fn encode(&self, msg: &[u8]) -> Vec<u8> {
        assert!(msg.len() + self.nsym <= 255, "RS block too long");
        let gen = &self.gen;
        let mut out = msg.to_vec();
        out.extend(std::iter::repeat(0).take(self.nsym));
        for i in 0..msg.len() {
            let coef = out[i];
            if coef != 0 {
                for j in 1..gen.len() {
                    out[i + j] ^= gf::mul(gen[j], coef);
                }
            }
        }
        // Синтетическое деление затирает часть msg-байт — восстанавливаем.
        out[..msg.len()].copy_from_slice(msg);
        out
    }

    /// Декодирование n=k+nsym байт → k байт данных, либо явный отказ.
    pub fn decode(&self, received: &[u8]) -> Result<Vec<u8>, RsError> {
        assert!(received.len() >= self.nsym);
        let synd = self.calc_syndromes(received);
        if synd.iter().all(|&s| s == 0) {
            // Синдромы нулевые — ошибок нет.
            return Ok(received[..received.len() - self.nsym].to_vec());
        }
        let err_loc = self.find_error_locator(&synd)?;
        let err_loc_rev: Vec<u8> = err_loc.iter().rev().copied().collect();
        let err_pos = self.find_errors(&err_loc_rev, received.len())?;
        let corrected = correct_errata(received, &synd, &err_pos);
        // Пере-проверка: если синдромы не обнулились — считаем кадр неисправимым.
        let synd2 = self.calc_syndromes(&corrected);
        if synd2.iter().any(|&s| s != 0) {
            return Err(RsError::Uncorrectable);
        }
        Ok(corrected[..corrected.len() - self.nsym].to_vec())
    }

    /// [0, S_0, S_1, ..., S_{nsym-1}], S_i = R(α^i). Ведущий 0 нужен алгоритму Форни.
    fn calc_syndromes(&self, msg: &[u8]) -> Vec<u8> {
        let mut synd = vec![0u8; self.nsym + 1];
        for i in 0..self.nsym {
            synd[i + 1] = gf::poly_eval(msg, gf::pow(2, i as i32));
        }
        synd
    }

    /// Берлекэмп-Мэсси: многочлен локаторов ошибок.
    fn find_error_locator(&self, synd: &[u8]) -> Result<Vec<u8>, RsError> {
        let nsym = self.nsym;
        let mut err_loc = vec![1u8];
        let mut old_loc = vec![1u8];
        let synd_shift = synd.len() - nsym; // = 1 (ведущий 0)

        for i in 0..nsym {
            let kk = i + synd_shift;
            let mut delta = synd[kk];
            for j in 1..err_loc.len() {
                delta ^= gf::mul(err_loc[err_loc.len() - 1 - j], synd[kk - j]);
            }
            old_loc.push(0);
            if delta != 0 {
                if old_loc.len() > err_loc.len() {
                    let new_loc = gf::poly_scale(&old_loc, delta);
                    old_loc = gf::poly_scale(&err_loc, gf::inverse(delta));
                    err_loc = new_loc;
                }
                err_loc = gf::poly_add(&err_loc, &gf::poly_scale(&old_loc, delta));
            }
        }
        while !err_loc.is_empty() && err_loc[0] == 0 {
            err_loc.remove(0);
        }
        let errs = err_loc.len() - 1;
        if errs * 2 > nsym {
            return Err(RsError::TooManyErrors);
        }
        Ok(err_loc)
    }

    /// Поиск Ченя: позиции ошибок как корни локаторного многочлена.
    fn find_errors(&self, err_loc_rev: &[u8], nmess: usize) -> Result<Vec<usize>, RsError> {
        let errs = err_loc_rev.len() - 1;
        let mut err_pos = Vec::new();
        for i in 0..nmess {
            if gf::poly_eval(err_loc_rev, gf::pow(2, i as i32)) == 0 {
                err_pos.push(nmess - 1 - i);
            }
        }
        if err_pos.len() != errs {
            // Найдено не столько корней, сколько степень локатора — кадр неисправим.
            return Err(RsError::Uncorrectable);
        }
        Ok(err_pos)
    }
}

fn generator_poly(nsym: usize) -> Vec<u8> {
    let mut g = vec![1u8];
    for i in 0..nsym {
        g = gf::poly_mul(&g, &[1, gf::pow(2, i as i32)]);
    }
    g
}

/// Алгоритм Форни: вычисляет и вычитает величины ошибок в найденных позициях.
fn correct_errata(msg: &[u8], synd: &[u8], err_pos: &[usize]) -> Vec<u8> {
    let nlen = msg.len();
    let coef_pos: Vec<usize> = err_pos.iter().map(|&p| nlen - 1 - p).collect();
    let err_loc = errata_locator(&coef_pos);

    let synd_rev: Vec<u8> = synd.iter().rev().copied().collect();
    let mut err_eval = error_evaluator(&synd_rev, &err_loc, err_loc.len() - 1);
    err_eval.reverse();

    // X_i = α^{coef_pos_i} — локаторы ошибок.
    let x: Vec<u8> = coef_pos
        .iter()
        .map(|&cp| gf::pow(2, -(255 - cp as i32)))
        .collect();

    let mut e = vec![0u8; nlen];
    let err_eval_rev: Vec<u8> = err_eval.iter().rev().copied().collect();
    for (i, &xi) in x.iter().enumerate() {
        let xi_inv = gf::inverse(xi);
        // Производная локатора по Форни: Π_{j≠i}(1 − X_i^{-1}·X_j).
        let mut err_loc_prime = 1u8;
        for (j, &xj) in x.iter().enumerate() {
            if j != i {
                err_loc_prime = gf::mul(err_loc_prime, 1 ^ gf::mul(xi_inv, xj));
            }
        }
        let y = gf::mul(xi, gf::poly_eval(&err_eval_rev, xi_inv));
        e[err_pos[i]] = gf::div(y, err_loc_prime);
    }
    gf::poly_add(msg, &e)
}

fn errata_locator(e_pos: &[usize]) -> Vec<u8> {
    let mut e_loc = vec![1u8];
    for &i in e_pos {
        e_loc = gf::poly_mul(&e_loc, &[gf::pow(2, i as i32), 1]);
    }
    e_loc
}

fn error_evaluator(synd: &[u8], err_loc: &[u8], nsym: usize) -> Vec<u8> {
    let mut divisor = vec![0u8; nsym + 2];
    divisor[0] = 1; // делим на x^{nsym+1}
    let product = gf::poly_mul(synd, err_loc);
    let (_, remainder) = gf::poly_div(&product, &divisor);
    remainder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_no_errors() {
        let rs = RsCodec::new(10);
        let msg = b"HELLO ADL/1 ACOUSTIC LINK";
        let enc = rs.encode(msg);
        assert_eq!(enc.len(), msg.len() + 10);
        let dec = rs.decode(&enc).unwrap();
        assert_eq!(dec, msg);
    }

    #[test]
    fn corrects_up_to_t_errors() {
        let rs = RsCodec::new(16); // t = 8
        let msg: Vec<u8> = (0..100).map(|i| (i * 7 + 3) as u8).collect();
        let mut enc = rs.encode(&msg);
        // Вносим ровно t=8 ошибок в разные позиции.
        for (k, pos) in [3usize, 10, 20, 33, 50, 60, 80, 99].iter().enumerate() {
            enc[*pos] ^= (k as u8).wrapping_add(1).wrapping_mul(37);
        }
        let dec = rs.decode(&enc).unwrap();
        assert_eq!(dec, msg);
    }

    #[test]
    fn corrects_burst_within_t() {
        let rs = RsCodec::new(16); // t = 8
        let msg: Vec<u8> = (0..200).map(|i| (i % 251) as u8).collect();
        let mut enc = rs.encode(&msg);
        for pos in 40..48 {
            // 8-символьный пакетный всплеск
            enc[pos] ^= 0xA5;
        }
        assert_eq!(rs.decode(&enc).unwrap(), msg);
    }

    #[test]
    fn detects_uncorrectable_beyond_t() {
        // t+1 ошибок должны давать ЯВНЫЙ отказ, а не тихое неверное декодирование.
        let rs = RsCodec::new(8); // t = 4
        let msg: Vec<u8> = (0..60).map(|i| (i * 3) as u8).collect();
        let mut any_wrong_silent = false;
        for trial in 0..40u8 {
            let mut enc = rs.encode(&msg);
            // 5 ошибок (> t=4).
            for e in 0..5usize {
                let pos = (trial as usize * 7 + e * 11) % enc.len();
                enc[pos] ^= (e as u8 + 1).wrapping_mul(53).wrapping_add(trial);
            }
            match rs.decode(&enc) {
                Err(_) => {}                          // корректно: отказ
                Ok(dec) if dec == msg => {}           // редко: случайно исправилось верно
                Ok(_) => any_wrong_silent = true,     // недопустимо: тихая неверная выдача
            }
        }
        assert!(!any_wrong_silent, "RS silently produced wrong data beyond t errors");
    }
}
