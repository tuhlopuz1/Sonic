//! FEC полезной нагрузки: Reed-Solomon (GF256) + блочный интерливер.
//!
//! [`FecCodec`] режет payload на блоки по `k` байт, кодирует каждый в `k+nsym` байт и
//! перемешивает слова интерливером. Число блоков восстанавливается на приёме из длины
//! потока (n фиксирован), поэтому FEC-слой сам себя не описывает — истинную длину
//! payload несёт заголовок кадра ([`crate::framing`], поле PayloadLen).

pub mod galois;
pub mod interleave;
pub mod reed_solomon;

pub use reed_solomon::{RsCodec, RsError};

/// Ошибка декодирования FEC-слоя.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FecError {
    /// Длина потока не кратна длине кодового слова — поток бит; кадр битый.
    Malformed,
    /// Один из RS-блоков неисправим (см. [`RsError`]).
    Rs(RsError),
}

/// FEC-кодек с фиксированной геометрией блока: `k` информационных байт, `nsym`
/// проверочных, длина слова `n = k + nsym`.
#[derive(Debug, Clone)]
pub struct FecCodec {
    k: usize,
    rs: RsCodec,
}

impl FecCodec {
    /// `k` — данных на блок, `nsym` — проверочных байт (t = nsym/2 исправляемых ошибок).
    pub fn new(k: usize, nsym: usize) -> Self {
        assert!(k > 0 && k + nsym <= 255, "RS block geometry out of range");
        FecCodec {
            k,
            rs: RsCodec::new(nsym),
        }
    }

    pub fn block_data_len(&self) -> usize {
        self.k
    }
    pub fn block_code_len(&self) -> usize {
        self.k + self.rs.nsym()
    }

    /// Сколько байт FEC-потока получится из payload данной длины (для оценок скорости).
    pub fn encoded_len(&self, payload_len: usize) -> usize {
        let blocks = payload_len.div_ceil(self.k).max(1);
        blocks * self.block_code_len()
    }

    /// Кодирует payload: паддинг нулями до кратности k, RS по блокам, интерливер.
    pub fn encode(&self, payload: &[u8]) -> Vec<u8> {
        let num_blocks = payload.len().div_ceil(self.k).max(1);
        let mut padded = payload.to_vec();
        padded.resize(num_blocks * self.k, 0);

        let words: Vec<Vec<u8>> = padded
            .chunks(self.k)
            .map(|chunk| self.rs.encode(chunk))
            .collect();
        interleave::interleave(&words)
    }

    /// Декодирует FEC-поток → блоки данных (num_blocks·k байт, с паддингом).
    /// Возвращает столько байт, сколько было после паддинга; вызывающий обрезает по
    /// истинной длине из заголовка кадра.
    pub fn decode(&self, coded: &[u8]) -> Result<Vec<u8>, FecError> {
        let n = self.block_code_len();
        if coded.is_empty() || coded.len() % n != 0 {
            return Err(FecError::Malformed);
        }
        let num_blocks = coded.len() / n;
        let words = interleave::deinterleave(coded, num_blocks, n);
        let mut out = Vec::with_capacity(num_blocks * self.k);
        for word in &words {
            let data = self.rs.decode(word).map_err(FecError::Rs)?;
            out.extend_from_slice(&data);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_no_errors() {
        let fec = FecCodec::new(32, 16);
        let payload = b"The quick brown fox jumps over the lazy dog. ADL/1 acoustic link test.";
        let coded = fec.encode(payload);
        let decoded = fec.decode(&coded).unwrap();
        assert_eq!(&decoded[..payload.len()], payload);
    }

    #[test]
    fn wide_burst_survives_thanks_to_interleaving() {
        // Всплеск шире t в одном слове, но интерливер разносит его так, что каждое
        // слово видит ≤ t ошибок и RS всё чинит.
        let fec = FecCodec::new(32, 16); // t=8 на слово
        let payload: Vec<u8> = (0..128).map(|i| (i * 5 + 1) as u8).collect();
        let mut coded = fec.encode(&payload);
        // 4 блока → всплеск до 4·8 = 32 подряд символов терпим.
        for x in coded.iter_mut().skip(20).take(30) {
            *x ^= 0x5A;
        }
        let decoded = fec.decode(&coded).unwrap();
        assert_eq!(&decoded[..payload.len()], &payload[..]);
    }

    #[test]
    fn malformed_length_is_rejected() {
        let fec = FecCodec::new(16, 8);
        let coded = fec.encode(b"hi");
        let truncated = &coded[..coded.len() - 1];
        assert_eq!(fec.decode(truncated), Err(FecError::Malformed));
    }
}
