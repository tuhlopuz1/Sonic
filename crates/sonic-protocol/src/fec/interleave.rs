//! Блочный интерливер поперёк RS-кодовых слов.
//!
//! RS чинит до `t` символьных ошибок в ОДНОМ кодовом слове. Широкий акустический
//! всплеск (щелчок, скрип стула) может испортить подряд больше `t` символов — если бы
//! они лежали в одном слове, RS бы не справился. Интерливер раскладывает слова строками
//! и передаёт по столбцам: подряд идущие в эфире символы попадают в разные слова, и
//! всплеск длиной B бьёт по каждому слову максимум ceil(B / число_слов) раз (plan.md §2).

/// Столбцовое чтение: слова (строки) → плоский поток (по столбцам).
/// Все слова обязаны быть одной длины.
pub fn interleave(blocks: &[Vec<u8>]) -> Vec<u8> {
    if blocks.is_empty() {
        return Vec::new();
    }
    let rows = blocks.len();
    let cols = blocks[0].len();
    debug_assert!(blocks.iter().all(|b| b.len() == cols));
    let mut out = Vec::with_capacity(rows * cols);
    for col in 0..cols {
        for block in blocks.iter() {
            out.push(block[col]);
        }
    }
    out
}

/// Обратная операция: плоский поток → `num_blocks` слов длины `block_len`.
pub fn deinterleave(flat: &[u8], num_blocks: usize, block_len: usize) -> Vec<Vec<u8>> {
    debug_assert_eq!(flat.len(), num_blocks * block_len);
    let mut blocks = vec![Vec::with_capacity(block_len); num_blocks];
    for col in 0..block_len {
        for (row, block) in blocks.iter_mut().enumerate() {
            block.push(flat[col * num_blocks + row]);
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_deinterleave_identity() {
        let blocks = vec![
            vec![1u8, 2, 3, 4],
            vec![10, 20, 30, 40],
            vec![100, 101, 102, 103],
        ];
        let flat = interleave(&blocks);
        let back = deinterleave(&flat, 3, 4);
        assert_eq!(blocks, back);
    }

    #[test]
    fn burst_is_spread_across_words() {
        // 4 слова по 8 символов; всплеск из 4 подряд символов в эфире должен задеть
        // каждое слово не больше одного раза.
        let blocks: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; 8]).collect();
        let mut flat = interleave(&blocks);
        for x in flat.iter_mut().skip(5).take(4) {
            *x = 0xFF; // 4-символьный всплеск
        }
        let back = deinterleave(&flat, 4, 8);
        for word in &back {
            let corrupted = word.iter().filter(|&&b| b != 0).count();
            assert!(corrupted <= 1, "burst hit one word {corrupted} times");
        }
    }
}
