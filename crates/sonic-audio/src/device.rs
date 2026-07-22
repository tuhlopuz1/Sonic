//! Перечисление cpal-устройств и подбор конфигурации ввода/вывода.
//!
//! Протокол требует 48 кГц (PROTOCOL.md §2.2, ультразвук — обязательно), поэтому по
//! возможности форсируем именно эту частоту дискретизации; иначе берём дефолт устройства
//! и сообщаем реальную частоту наверх (сессия учитывает её при создании модемов).

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, SupportedStreamConfig};

/// Имена доступных устройств ввода/вывода — для показа в UI.
#[derive(Debug, Clone, Default)]
pub struct DeviceList {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

pub fn list_devices() -> Result<DeviceList, String> {
    let host = cpal::default_host();
    let mut list = DeviceList::default();

    if let Ok(devs) = host.input_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                list.inputs.push(name);
            }
        }
    }
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                list.outputs.push(name);
            }
        }
    }
    list.default_input = host.default_input_device().and_then(|d| d.name().ok());
    list.default_output = host.default_output_device().and_then(|d| d.name().ok());
    Ok(list)
}

/// Открытые устройства и их конфигурации для дуплекса.
pub struct IoConfig {
    pub input_device: Device,
    pub input_config: SupportedStreamConfig,
    pub output_device: Device,
    pub output_config: SupportedStreamConfig,
}

/// Подбирает устройства по умолчанию и конфигурации, стараясь выйти на `target_rate`.
pub fn default_io_config(target_rate: u32) -> Result<IoConfig, String> {
    let host = cpal::default_host();
    let input_device = host
        .default_input_device()
        .ok_or_else(|| "Микрофон не найден".to_string())?;
    let output_device = host
        .default_output_device()
        .ok_or_else(|| "Динамик не найден".to_string())?;

    let input_config = pick_input_config(&input_device, target_rate)?;
    let output_config = pick_output_config(&output_device, target_rate)?;

    Ok(IoConfig {
        input_device,
        input_config,
        output_device,
        output_config,
    })
}

fn pick_input_config(device: &Device, target_rate: u32) -> Result<SupportedStreamConfig, String> {
    if let Ok(ranges) = device.supported_input_configs() {
        for range in ranges {
            if range.min_sample_rate().0 <= target_rate && target_rate <= range.max_sample_rate().0 {
                return Ok(range.with_sample_rate(cpal::SampleRate(target_rate)));
            }
        }
    }
    device
        .default_input_config()
        .map_err(|e| format!("Конфигурация микрофона: {e}"))
}

fn pick_output_config(device: &Device, target_rate: u32) -> Result<SupportedStreamConfig, String> {
    if let Ok(ranges) = device.supported_output_configs() {
        for range in ranges {
            if range.min_sample_rate().0 <= target_rate && target_rate <= range.max_sample_rate().0 {
                return Ok(range.with_sample_rate(cpal::SampleRate(target_rate)));
            }
        }
    }
    device
        .default_output_config()
        .map_err(|e| format!("Конфигурация динамика: {e}"))
}
