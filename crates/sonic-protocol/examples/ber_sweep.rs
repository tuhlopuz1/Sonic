//! Свип BER/throughput по SNR для CSS и OFDM — предъявляемое жюри доказательство
//! «эффективности/устойчивости» (plan.md §2, критерии PROTOCOL.md §12).
//!
//! Запуск: `cargo run --release --example ber_sweep -p sonic-protocol`
//!
//! Для каждого режима × SNR прогоняет N кадров через AWGN-канал, считает битовый BER
//! (по восстановленным байтам до FEC) и долю обнаруженных кадров, печатает таблицу.

use sonic_protocol::bandplan::{DuplexScheme, Fdd, Profile, Role};
use sonic_protocol::modem::qam::Modulation;
use sonic_protocol::modem::{CssModem, MfskModem, Modem, OfdmModem};
use sonic_protocol::sim::Rng;

const TRIALS: usize = 40;
const PAYLOAD_LEN: usize = 40;

fn main() {
    let fdd = Fdd::new(Role::Initiator, Profile::Audible);
    let band = fdd.tx_band();
    let sr = fdd.sample_rate();

    let css = CssModem::with_defaults(band, sr);
    let mfsk = MfskModem::new(band, sr);
    let ofdm_qpsk = OfdmModem::new(band, sr, Modulation::Qpsk);
    let ofdm_16 = OfdmModem::new(band, sr, Modulation::Qam16);

    let modems: [(&str, &dyn Modem); 4] = [
        ("CSS (SF8)", &css),
        ("MFSK (M16)", &mfsk),
        ("OFDM-QPSK", &ofdm_qpsk),
        ("OFDM-16QAM", &ofdm_16),
    ];

    println!("ADL/1 BER / throughput sweep — AWGN, {TRIALS} кадров/точку, payload {PAYLOAD_LEN} байт\n");
    for (name, modem) in modems {
        let bitrate = raw_bitrate(modem, sr);
        println!("== {name}  (сырая скорость модема ≈ {bitrate:.0} бит/с) ==");
        println!("  SNR,дБ |    BER    | детект. кадров");
        println!("  -------+-----------+----------------");
        for snr_db in [0, 3, 6, 9, 12, 15, 18, 21, 24] {
            let (ber, detect) = measure(modem, snr_db as f32);
            println!("   {snr_db:>4}  | {ber:>9.2e} |   {:>5.1}%", detect * 100.0);
        }
        println!();
    }
}

/// Сырая скорость модема (бит полезного кадра / длительность кадра), бит/с.
fn raw_bitrate(modem: &dyn Modem, sr: u32) -> f32 {
    let samples = modem.frame_samples(PAYLOAD_LEN);
    let bits = (PAYLOAD_LEN * 8) as f32;
    bits / (samples as f32 / sr as f32)
}

/// Возвращает (средний битовый BER, доля обнаруженных кадров) для данного SNR.
fn measure(modem: &dyn Modem, snr_db: f32) -> (f32, f32) {
    let mut rng = Rng::new(0xC0FFEE ^ (snr_db as u64).wrapping_mul(2654435761));
    let mut total_bits = 0u64;
    let mut error_bits = 0u64;
    let mut detected = 0usize;

    for _ in 0..TRIALS {
        let payload: Vec<u8> = (0..PAYLOAD_LEN).map(|_| (rng.next_f32() * 256.0) as u8).collect();
        let tx = modem.modulate(&payload);

        // Кадр с лид/хвостом тишины; AWGN на весь буфер, sigma по мощности сигнала.
        let sig_power: f32 = tx.iter().map(|x| x * x).sum::<f32>() / tx.len() as f32;
        let sigma = (sig_power / 10f32.powf(snr_db / 10.0)).sqrt();
        let mut buf: Vec<f32> = vec![0.0; 2000];
        buf.extend_from_slice(&tx);
        buf.extend(std::iter::repeat(0.0).take(2000));
        for x in buf.iter_mut() {
            *x += rng.next_gaussian() * sigma;
        }

        if let Some(d) = modem.demodulate(&buf) {
            detected += 1;
            for i in 0..PAYLOAD_LEN {
                let got = d.bytes.get(i).copied().unwrap_or(0);
                error_bits += (payload[i] ^ got).count_ones() as u64;
                total_bits += 8;
            }
        }
    }

    let ber = if total_bits > 0 {
        error_bits as f32 / total_bits as f32
    } else {
        0.5
    };
    (ber, detected as f32 / TRIALS as f32)
}
