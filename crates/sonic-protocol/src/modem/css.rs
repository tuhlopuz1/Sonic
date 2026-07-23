//! CSS — Chirp Spread Spectrum (LoRa-style), надёжный режим (PROTOCOL.md §4).
//!
//! Информация — в циклическом сдвиге линейного чирпа: символ `s` = базовый up-chirp,
//! сдвинутый по времени на `s·sps/2^SF`. Это даёт большой processing gain (энергия
//! размазана по полосе и времени), поэтому канал устойчив к узкополосным помехам,
//! щелчкам, голосу в комнате.
//!
//! Демодуляция: перемножить принятый символ с обратным чирпом (дечирп) и взять FFT —
//! позиция пика прямо даёт `s` (дёшево и устойчиво к шуму: интеграция по всей
//! длительности символа = накопленный gain).
//!
//! Работаем в комплексном baseband на исходном sample rate (без ресемплинга): при
//! `sps = round(fs·2^SF/BW)` дечирпнутый тон символа `s` попадает ровно в FFT-бин `s`.
//! Затем baseband апконвертится в под-полосу активной [`DuplexScheme`] через `iq`.

use super::{bytes_to_symbols, symbols_to_bytes, Demodulated, Modem};
use crate::bandplan::SubBand;
use crate::fft::FftEngine;
use crate::framing::PhyMode;
use crate::iq::{downconvert, upconvert, FirLowpass};
use num_complex::Complex32;
use std::f32::consts::PI;

/// Магическое значение sync-символа (PROTOCOL.md §4.3/§6.1) — защита от ложного
/// срабатывания на шуме/музыке.
const MAGIC_SYNC: u16 = 0x2B;

const PREAMBLE_UP: usize = 8; // грубая детекция + оценка сдвига
const PREAMBLE_DOWN: usize = 2; // маркер конца преамбулы
const SYNC_SYMS: usize = 1; // sync-символ = MAGIC_SYNC
const LEN_REPS: usize = 3; // длина кадра шлётся 3× с мажоритарным голосованием

/// Параметры CSS. SF=8 даёт 8 бит/символ и большой processing gain (integration по
/// всей длительности символа). Полоса BW определяет длительность символа:
/// `sps = fs·2^SF/BW`, символ = `sps/fs` секунд, скорость = `SF·BW/2^SF` бит/с.
///
/// По умолчанию BW=6000 Гц → sps=2048 @48 кГц → символ 42.7 мс, ~187 бит/с. Это на
/// порядок быстрее прежних 1600 Гц (50 бит/с, символ 160 мс), при котором даже короткое
/// сообщение занимало 6–12 с эфира и MAC-таймауты успевали объявить связь мёртвой ещё
/// до конца передачи кадра. BW ужимается под ширину под-полосы (см. `new`), поэтому
/// 6000 Гц влезает и в нижнюю (6700 Гц), и в верхнюю (7200 Гц) полосу FDD с запасом.
/// SF адаптивен 6–10: ниже SF — быстрее, выше — надёжнее.
#[derive(Debug, Clone, Copy)]
pub struct CssParams {
    pub sf: u32,
    pub bandwidth_hz: f32,
}

impl Default for CssParams {
    fn default() -> Self {
        CssParams {
            sf: 8,
            bandwidth_hz: 6000.0,
        }
    }
}

pub struct CssModem {
    center_hz: f32,
    sample_rate: f32,
    sf: u32,
    bw: f32,
    sps: usize,
    n_sym: usize,
    base_up: Vec<Complex32>,
    base_up_conj: Vec<Complex32>,
    fft: FftEngine,
}

impl CssModem {
    pub fn new(band: SubBand, sample_rate: u32, params: CssParams) -> Self {
        let sf = params.sf;
        let sr = sample_rate as f32;
        let n_sym = 1usize << sf;
        // КЛЮЧЕВОЙ инвариант CSS: дечирпнутый тон символа s попадает РОВНО в бин s только
        // если sps = fs·2^SF/BW — ЦЕЛОЕ, т.е. fs/BW целое (иначе округление sps смещает
        // пик и декод рассыпается). Поэтому BW выбираем не произвольным клиппингом под
        // полосу, а как fs/OSF при ЦЕЛОМ OSF = oversampling factor: тогда sps = OSF·2^SF
        // ровно. OSF — наименьшее целое, при котором BW = fs/OSF влезает и в под-полосу, и в
        // потолок params.bandwidth_hz. (В прежней широкой полосе это давало ровно OSF=8,
        // BW=6000 — поведение не изменилось; в узкой TDD-полосе — OSF=16, BW=3000.)
        let max_bw = (band.bandwidth_hz * 0.98).min(params.bandwidth_hz);
        let osf = (sr / max_bw).ceil().max(1.0) as usize;
        let bw = sr / osf as f32;
        let sps = osf * n_sym;

        // Базовый up-chirp в baseband: мгновенная частота линейно от −BW/2 до +BW/2.
        // Фаза (замкнутая форма интеграла): φ(n) = π·BW/fs · (n²/sps − n).
        let base_up: Vec<Complex32> = (0..sps)
            .map(|n| {
                let nf = n as f32;
                let phase = PI * bw / sr * (nf * nf / sps as f32 - nf);
                Complex32::new(phase.cos(), phase.sin())
            })
            .collect();
        let base_up_conj: Vec<Complex32> = base_up.iter().map(|c| c.conj()).collect();

        CssModem {
            center_hz: band.center_hz,
            sample_rate: sr,
            sf,
            bw,
            sps,
            n_sym,
            base_up,
            base_up_conj,
            fft: FftEngine::new(sps),
        }
    }

    pub fn with_defaults(band: SubBand, sample_rate: u32) -> Self {
        Self::new(band, sample_rate, CssParams::default())
    }

    pub fn samples_per_symbol(&self) -> usize {
        self.sps
    }

    /// Циклически сдвинутый up-chirp = символ `s` (в baseband).
    fn symbol_chirp(&self, s: u16, out: &mut Vec<Complex32>) {
        let shift = (s as usize * self.sps) / self.n_sym;
        for n in 0..self.sps {
            out.push(self.base_up[(n + shift) % self.sps]);
        }
    }

    fn header_slots(&self) -> usize {
        // после 8 up + 2 down: sync + LEN_REPS×len_syms
        PREAMBLE_UP + PREAMBLE_DOWN + SYNC_SYMS + LEN_REPS * self.len_syms()
    }

    fn len_syms(&self) -> usize {
        // сколько символов нужно на u16 длину кадра
        (16 + self.sf as usize - 1) / self.sf as usize
    }

    fn body_syms(&self, nbytes: usize) -> usize {
        (nbytes * 8 + self.sf as usize - 1) / self.sf as usize
    }

    /// Дечирп + FFT одного символа с baseband-смещения `off`. Возвращает
    /// (значение символа, магнитуда пика, средняя магнитуда шума).
    ///
    /// Сигнал передискретизирован в OSF = sps/2^SF раз, поэтому энергия символа `s`
    /// после дечирпа сидит не только в бине `s`, но и в его алиасах `s + k·2^SF`
    /// (циклически завёрнутая часть чирпа). Сворачиваем FFT по модулю 2^SF, чтобы
    /// собрать всю энергию символа в один бин `s` — иначе высокие символы «протекают».
    fn demod_symbol(&self, bb: &[Complex32], off: usize) -> (u16, f32, f32) {
        let mut buf: Vec<Complex32> = (0..self.sps)
            .map(|n| bb[off + n] * self.base_up_conj[n])
            .collect();
        self.fft.forward(&mut buf);

        let mut fold = vec![0.0f32; self.n_sym];
        for (bin, c) in buf.iter().enumerate() {
            fold[bin % self.n_sym] += c.norm_sqr();
        }

        let mut best_bin = 0usize;
        let mut best = f32::MIN;
        let mut sum = 0.0f32;
        for (bin, &m) in fold.iter().enumerate() {
            sum += m;
            if m > best {
                best = m;
                best_bin = bin;
            }
        }
        let peak = best.sqrt();
        let noise_mean = ((sum - best) / (self.n_sym as f32 - 1.0)).max(1e-20).sqrt();
        (best_bin as u16, peak, noise_mean)
    }

    /// Как [`demod_symbol`], но с СУБ-БИННОЙ (параболической) оценкой позиции пика — для
    /// точной временной синхронизации/оценки дрейфа по преамбуле. Возвращает
    /// (дробный бин, пик, шум) или `None`, если спектр пуст.
    fn demod_symbol_frac(&self, bb: &[Complex32], off: usize) -> Option<(f32, f32, f32)> {
        if off + self.sps > bb.len() {
            return None;
        }
        let mut buf: Vec<Complex32> = (0..self.sps)
            .map(|n| bb[off + n] * self.base_up_conj[n])
            .collect();
        self.fft.forward(&mut buf);

        let mut fold = vec![0.0f32; self.n_sym];
        for (bin, c) in buf.iter().enumerate() {
            fold[bin % self.n_sym] += c.norm_sqr();
        }
        let mut best_bin = 0usize;
        let mut best = f32::MIN;
        let mut sum = 0.0f32;
        for (bin, &m) in fold.iter().enumerate() {
            sum += m;
            if m > best {
                best = m;
                best_bin = bin;
            }
        }
        // Параболическая интерполяция по соседним (циклически) бинам: вершина параболы
        // через (b-1, b, b+1) даёт дробное смещение пика в пределах ±0.5 бина.
        let nm = self.n_sym;
        let alpha = fold[(best_bin + nm - 1) % nm];
        let beta = fold[best_bin];
        let gamma = fold[(best_bin + 1) % nm];
        let denom = alpha - 2.0 * beta + gamma;
        let frac = if denom.abs() > 1e-20 {
            (0.5 * (alpha - gamma) / denom).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let peak = beta.sqrt();
        let noise_mean = ((sum - beta) / (nm as f32 - 1.0)).max(1e-20).sqrt();
        Some((best_bin as f32 + frac, peak, noise_mean))
    }
}

impl Modem for CssModem {
    fn mode(&self) -> PhyMode {
        PhyMode::Css
    }

    fn modulate(&self, frame_bytes: &[u8]) -> Vec<f32> {
        let len = frame_bytes.len() as u16;
        let len_bytes = len.to_be_bytes();
        let len_syms = bytes_to_symbols(&len_bytes, self.sf);
        let body = bytes_to_symbols(frame_bytes, self.sf);

        let total = self.header_slots() + body.len();
        let mut bb: Vec<Complex32> = Vec::with_capacity(total * self.sps);

        // Каждый символ дописываем с ФАЗОВЫМ ВЫРАВНИВАНИЕМ к предыдущему: иначе на стыке
        // символов (у каждого своя стартовая фаза чирпа) фаза скакала → щелчок в динамике
        // на каждом символе, плюс спектральный всплеск, мешавший приёму. Поворот на
        // постоянную фазу не двигает пик дечирпа (демод работает по |FFT|²), поэтому это
        // безопасно для декодирования. Преамбула (повторы base_up) и так непрерывна.
        for _ in 0..PREAMBLE_UP {
            push_aligned(&self.base_up, &mut bb);
        }
        for _ in 0..PREAMBLE_DOWN {
            push_aligned(&self.base_up_conj, &mut bb); // down-chirp = conj(up-chirp)
        }
        let mut scratch = Vec::with_capacity(self.sps);
        let mut emit = |s: u16, bb: &mut Vec<Complex32>| {
            scratch.clear();
            self.symbol_chirp(s, &mut scratch);
            push_aligned(&scratch, bb);
        };
        emit(MAGIC_SYNC, &mut bb);
        for _ in 0..LEN_REPS {
            for &s in &len_syms {
                emit(s, &mut bb);
            }
        }
        for &s in &body {
            emit(s, &mut bb);
        }

        let mut passband = upconvert(&bb, self.sample_rate, self.center_hz, 0);
        // Запас до полной шкалы: на пике ~1.0 реальные динамики клиппят/искажают чирп —
        // по воздуху он рассыпался, хотя в симуляции всё идеально. Демод инвариантен к масштабу.
        super::normalize_peak(&mut passband, super::TX_PEAK);
        apply_edge_ramp(&mut passband, (self.sample_rate * 0.003) as usize);
        passband
    }

    fn demodulate(&self, samples: &[f32]) -> Option<Demodulated> {
        let min_frame = self.header_slots() * self.sps;
        if samples.len() < min_frame {
            return None;
        }

        // Даунконверсия всей записи в baseband; ФНЧ заодно режет чужую FDD-полосу (эхо).
        let mut lp = FirLowpass::new(self.bw * 0.6, self.sample_rate, 129);
        let bb = downconvert(samples, self.sample_rate, self.center_hz, 0, &mut lp);

        // 1. Грубая энергетическая детекция фронта преамбулы (коротким окном — острее).
        let edge = coarse_frame_edge(&bb, self.sps)?;

        // 2. Точная временная привязка + компенсация рассинхрона тактовой частоты (SFO).
        //    Сдвиг тайминга виден как сдвиг бина при дечирпе up-chirp преамбулы. Меряем
        //    смещение в НЕСКОЛЬКИХ окнах преамбулы с СУБ-БИННОЙ (параболической) точностью
        //    и делаем взвешенную линейную аппроксимацию: свободный член — начальное
        //    выравнивание, наклон — скорость дрейфа (шаг символа приёмника ≠ передатчика).
        //    Суб-биновая интерполяция обязательна: разрешение по целому бину — fs/BW = 8
        //    сэмплов, а дрейф за преамбулу — доли сэмпла, иначе его не увидеть. Наклон
        //    применяем осторожно (ограничен разумным диапазоном ppm), чтобы шумная оценка
        //    под реверберацией не сдвинула сетку сильнее, чем сам дрейф. Окна k — внутри
        //    up-chirp части (с запасом на неточность фронта), чтобы не зацепить down-chirp.
        let (mut sx, mut sy, mut sxx, mut sxy, mut n) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for k in 0..(PREAMBLE_UP - 2) {
            let off = edge + (k + 1) * self.sps;
            if off + self.sps > bb.len() {
                break;
            }
            let Some((frac_bin, peak, noise)) = self.demod_symbol_frac(&bb, off) else {
                continue;
            };
            if peak < noise * 4.0 {
                continue; // слот не похож на чистый up-chirp
            }
            let signed = if frac_bin > self.n_sym as f32 / 2.0 {
                frac_bin - self.n_sym as f32
            } else {
                frac_bin
            };
            let o = signed * self.sps as f32 / self.n_sym as f32; // смещение в сэмплах
            let x = (k + 1) as f32;
            sx += x;
            sy += o;
            sxx += x * x;
            sxy += x * o;
            n += 1.0;
        }
        if n < 1.0 {
            return None;
        }
        // o(x) = a + b·x: a = (edge − U) начальное смещение, b = −sps·ppm дрейф/символ.
        let (a, mut b) = if n >= 3.0 {
            let denom = n * sxx - sx * sx;
            if denom.abs() < 1e-3 {
                (sy / n, 0.0)
            } else {
                let slope = (n * sxy - sx * sy) / denom;
                ((sy - slope * sx) / n, slope)
            }
        } else {
            (sy / n, 0.0)
        };
        // Ограничиваем наклон эквивалентом ~±400 ppm: за пределами это уже не дрейф, а
        // шумовой выброс оценки (например, из-за реверберации), которому нельзя доверять.
        let max_b = self.sps as f32 * 400e-6;
        b = b.clamp(-max_b, max_b);
        let u = edge as f32 - a; // истинное начало преамбулы
        let sps_rx = self.sps as f32 - b; // дрифтованный шаг символа приёмника
        // Позиция символа с глобальным индексом j (0 = первый up-chirp), с учётом дрейфа.
        let read_pos = |j: usize| -> Option<usize> {
            let p = (u + j as f32 * sps_rx).round();
            if p < 0.0 || p as usize + self.sps > bb.len() {
                return None;
            }
            Some(p as usize)
        };

        // 3. Проверка sync-символа (защита от ложного захвата на шуме/музыке).
        let sync_j = PREAMBLE_UP + PREAMBLE_DOWN;
        let sync_off = read_pos(sync_j)?;
        let (sync_val, peak, noise) = self.demod_symbol(&bb, sync_off);
        if sync_val != MAGIC_SYNC {
            return None;
        }
        let snr_db = 20.0 * (peak / noise).log10();

        // 4. Длина кадра: LEN_REPS повторов, мажоритарное голосование по каждому символу.
        let len_syms = self.len_syms();
        let len_j0 = sync_j + SYNC_SYMS;
        let mut len_symbols = Vec::with_capacity(len_syms);
        for pos in 0..len_syms {
            let mut votes = [0u16; LEN_REPS];
            for (rep, vote) in votes.iter_mut().enumerate() {
                let off = read_pos(len_j0 + rep * len_syms + pos)?;
                *vote = self.demod_symbol(&bb, off).0;
            }
            len_symbols.push(majority(&votes));
        }
        let len_bytes = symbols_to_bytes(&len_symbols, self.sf, 2);
        if len_bytes.len() < 2 {
            return None;
        }
        let frame_len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
        if !(crate::framing::OVERHEAD..=8192).contains(&frame_len) {
            return None; // неправдоподобная длина — считаем захват ложным
        }

        // 5. Тело кадра со СЛЕЖЕНИЕМ ЗА ТАЙМИНГОМ (decision-directed timing recovery).
        //    Преамбула — слишком короткая база, чтобы точно оценить дрейф, влияющий на
        //    весь длинный кадр, поэтому здесь замыкаем петлю: символ CSS всегда садится на
        //    ЦЕЛЫЙ бин, значит дробный остаток пика (параболическая интерполяция) — это
        //    чистая ошибка тайминга. Ею подтягиваем позицию каждого следующего символа,
        //    не давая дрейфу накопиться сверх ±0.5 бина. Так CSS держит рассинхрон
        //    тактовой частоты (SFO) в сотни ppm, а не пару десятков.
        let body_j0 = self.header_slots();
        let body_syms = self.body_syms(frame_len);
        let mut body_symbols = Vec::with_capacity(body_syms);
        let mut pos = u + body_j0 as f32 * sps_rx; // старт тела по дрейф-сетке (feedforward)
        let mut last_off = sync_off;
        let scale = self.sps as f32 / self.n_sym as f32; // сэмплов на бин
        for _ in 0..body_syms {
            if pos < 0.0 || pos.round() as usize + self.sps > bb.len() {
                return None;
            }
            let off = pos.round() as usize;
            last_off = off;
            let Some((frac_bin, _, _)) = self.demod_symbol_frac(&bb, off) else {
                return None;
            };
            let sym = (frac_bin.round() as i64).rem_euclid(self.n_sym as i64) as u16;
            body_symbols.push(sym);
            // Ошибка тайминга = дробный остаток относительно целого бина символа: >0 —
            // читаем ПОЗЖЕ символа (нужно сдвинуться раньше), поэтому вычитаем коррекцию.
            let residual = frac_bin - frac_bin.round(); // бины, [-0.5, 0.5]
            pos += self.sps as f32 - 0.35 * residual * scale; // номинальный шаг + слежение
        }
        let bytes = symbols_to_bytes(&body_symbols, self.sf, frame_len);

        Some(Demodulated {
            bytes,
            start_sample: u.max(0.0) as usize,
            end_sample: last_off + self.sps,
            snr_db,
        })
    }

    fn frame_samples(&self, payload_len: usize) -> usize {
        let frame_len = crate::framing::OVERHEAD + payload_len;
        (self.header_slots() + self.body_syms(frame_len)) * self.sps
    }
}

/// Мажоритарное голосование по повторам (для служебной длины кадра).
fn majority(votes: &[u16]) -> u16 {
    let mut best = votes[0];
    let mut best_count = 0;
    for &v in votes {
        let count = votes.iter().filter(|&&x| x == v).count();
        if count > best_count {
            best_count = count;
            best = v;
        }
    }
    best
}

/// Ищет нарастающий фронт энергии — грубая привязка к началу преамбулы. Окно короткое
/// (≈ sps/16), чтобы фронт не размазывался на длину символа; порог — относительно и
/// пикового уровня, и самого тихого места записи (шумовой пол).
fn coarse_frame_edge(bb: &[Complex32], sps: usize) -> Option<usize> {
    let win = (sps / 16).max(64);
    let step = (win / 2).max(1);
    if bb.len() < sps * 2 {
        return None;
    }
    let mut rms = Vec::new();
    let mut i = 0;
    while i + win <= bb.len() {
        let e: f32 = bb[i..i + win].iter().map(|c| c.norm_sqr()).sum();
        rms.push(((e / win as f32).sqrt(), i));
        i += step;
    }
    let peak_rms = rms.iter().fold(0.0f32, |a, x| a.max(x.0));
    let floor_rms = rms.iter().fold(f32::MAX, |a, x| a.min(x.0));
    if peak_rms < 1e-4 {
        return None; // тишина
    }
    let threshold = (peak_rms * 0.35).max(floor_rms * 4.0);
    for &(r, idx) in &rms {
        if r >= threshold {
            return Some(idx);
        }
    }
    None
}

/// Дописывает символ `raw` в `bb`, повернув его на постоянную фазу так, чтобы фаза (и
/// мгновенная частота) на стыке с хвостом `bb` были непрерывны — убирает щелчок на
/// границе символов. Все сэмплы имеют единичный модуль (постоянная огибающая CSS),
/// поэтому выравнивание — это чистый фазовый поворот, не влияющий на дечирп.
fn push_aligned(raw: &[Complex32], bb: &mut Vec<Complex32>) {
    if raw.is_empty() {
        return;
    }
    let rot = if bb.len() >= 2 {
        let last = bb[bb.len() - 1];
        let last2 = bb[bb.len() - 2];
        // Ожидаемая следующая точка = продолжение фазы с той же мгновенной частотой:
        // phase = 2·arg(last) − arg(last2). Для единичных модулей это last²·conj(last2).
        let target = last * last * last2.conj();
        let tn = target.norm();
        let r0 = raw[0].norm();
        if tn > 1e-9 && r0 > 1e-9 {
            (target / tn) * (raw[0].conj() / r0)
        } else {
            Complex32::new(1.0, 0.0)
        }
    } else {
        Complex32::new(1.0, 0.0)
    };
    for &c in raw {
        bb.push(c * rot);
    }
}

/// Мягкий фронт/срез (raised-cosine) на краях, чтобы динамик не щёлкал.
fn apply_edge_ramp(signal: &mut [f32], ramp: usize) {
    let ramp = ramp.min(signal.len() / 2).max(1);
    let n = signal.len();
    for i in 0..ramp {
        let g = 0.5 - 0.5 * (PI * i as f32 / ramp as f32).cos();
        signal[i] *= g;
        signal[n - 1 - i] *= g;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bandplan::{Fdd, Profile, Role, DuplexScheme};

    struct Lcg(u64);
    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    fn modem() -> CssModem {
        let fdd = Fdd::new(Role::Initiator, Profile::Audible);
        CssModem::with_defaults(fdd.tx_band(), fdd.sample_rate())
    }

    #[test]
    fn core_symbol_mapping() {
        let m = modem();
        for &s in &[0u16, 1, 5, 43, 100, 255] {
            let mut bb = Vec::new();
            m.symbol_chirp(s, &mut bb);
            let (bin, _, _) = m.demod_symbol(&bb, 0);
            eprintln!("s={s} -> bin={bin}");
            assert_eq!(bin, s, "symbol {s} decoded to bin {bin}");
        }
    }

    #[test]
    fn periodic_preamble_offset_maps_to_bin() {
        // Окно, сдвинутое на φ сэмплов внутри периодической up-преамбулы, должно
        // дечирпиться в бин φ·2^SF/sps — на этом стоит временная синхронизация.
        let m = modem();
        let mut bb = Vec::new();
        for _ in 0..4 {
            bb.extend_from_slice(&m.base_up);
        }
        let phi = 300usize;
        let (bin, _, _) = m.demod_symbol(&bb, phi);
        let expect = (phi * m.n_sym / m.sps) as u16;
        eprintln!("phi={phi} -> bin={bin} (expect ~{expect})");
        assert!((bin as i32 - expect as i32).abs() <= 1);
    }

    #[test]
    fn zero_noise_frame_roundtrip() {
        let m = modem();
        let frame = b"\x2B\x10hello acoustic world via CSS chirp modem".to_vec();
        let tx = m.modulate(&frame);
        // Лид/хвост тишины, как в реальном захвате.
        let mut buf = vec![0.0f32; 2000];
        buf.extend_from_slice(&tx);
        buf.extend(std::iter::repeat(0.0).take(2000));
        let got = m.demodulate(&buf).expect("frame not demodulated");
        assert_eq!(got.bytes, frame);
    }

    #[test]
    fn survives_moderate_awgn() {
        let m = modem();
        let frame = b"ADL/1 CSS under noise 0123456789".to_vec();
        let tx = m.modulate(&frame);
        let mut rng = Lcg(12345);
        let sig_rms =
            (tx.iter().map(|x| x * x).sum::<f32>() / tx.len() as f32).sqrt();
        let noise_amp = sig_rms * 0.5; // существенный шум
        let mut buf = vec![0.0f32; 3000];
        buf.extend(tx.iter().map(|&s| s + rng.next_f32() * noise_amp));
        buf.extend((0..3000).map(|_| rng.next_f32() * noise_amp));
        let got = m.demodulate(&buf).expect("frame lost under AWGN");
        assert_eq!(got.bytes, frame);
    }

    #[test]
    fn no_false_frame_on_pure_noise() {
        let m = modem();
        let mut rng = Lcg(999);
        let buf: Vec<f32> = (0..80_000).map(|_| rng.next_f32() * 0.2).collect();
        assert!(m.demodulate(&buf).is_none());
    }
}
