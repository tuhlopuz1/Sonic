//! Диагностика аудио-железа: какие устройства видны, что у них с форматами и частотами.
//!
//! Запуск: `cargo run --example audio_devices -p sonic-audio`
//!
//! Полезно, когда сессия не стартует или звук не декодируется: сразу видно, какой
//! формат/частоту реально отдаёт железо и есть ли вообще 48 кГц.

use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let host = cpal::default_host();
    let default_in = host.default_input_device().and_then(|d| d.name().ok());
    let default_out = host.default_output_device().and_then(|d| d.name().ok());

    println!("Хост: {:?}\n", host.id());

    println!("== Микрофоны ==");
    match host.input_devices() {
        Ok(devs) => {
            for d in devs {
                let name = d.name().unwrap_or_else(|_| "<без имени>".into());
                let mark = if Some(&name) == default_in.as_ref() { " (по умолчанию)" } else { "" };
                println!("  • {name}{mark}");
                if let Ok(cfg) = d.default_input_config() {
                    println!("      дефолт: {:?} @ {} Гц, {} кан.", cfg.sample_format(), cfg.sample_rate().0, cfg.channels());
                }
                print_ranges(d.supported_input_configs().ok());
            }
        }
        Err(e) => println!("  не удалось перечислить: {e}"),
    }

    println!("\n== Динамики ==");
    match host.output_devices() {
        Ok(devs) => {
            for d in devs {
                let name = d.name().unwrap_or_else(|_| "<без имени>".into());
                let mark = if Some(&name) == default_out.as_ref() { " (по умолчанию)" } else { "" };
                println!("  • {name}{mark}");
                if let Ok(cfg) = d.default_output_config() {
                    println!("      дефолт: {:?} @ {} Гц, {} кан.", cfg.sample_format(), cfg.sample_rate().0, cfg.channels());
                }
                print_ranges(d.supported_output_configs().ok());
            }
        }
        Err(e) => println!("  не удалось перечислить: {e}"),
    }
}

fn print_ranges(ranges: Option<impl Iterator<Item = cpal::SupportedStreamConfigRange>>) {
    let Some(ranges) = ranges else { return };
    let mut any = false;
    for r in ranges {
        if !any {
            println!("      поддерживает:");
            any = true;
        }
        let has48 = r.min_sample_rate().0 <= 48_000 && 48_000 <= r.max_sample_rate().0;
        println!(
            "        {:?}, {}–{} Гц, {} кан.{}",
            r.sample_format(),
            r.min_sample_rate().0,
            r.max_sample_rate().0,
            r.channels(),
            if has48 { "  ← есть 48 кГц" } else { "" }
        );
    }
}
