//! Перечисление cpal-устройств и подбор конфигурации ввода/вывода.
//!
//! Две вещи, критичные для работы на реальном железе:
//! 1. **Формат сэмплов**: берём не первый попавшийся, а лучший из поддерживаемых
//!    (F32 → I16 → …, см. [`crate::streams::format_rank`]). Иначе на ноутбуке, где
//!    первым в списке идёт U8-диапазон, поток открывался в 8-битном формате (или падал).
//! 2. **Частоты могут не совпадать**: в shared-режиме WASAPI каждое устройство залочено
//!    на свою частоту из настроек ОС (микрофон 44100, динамик 48000 — обычное дело), и
//!    общей частоты может не быть вообще. Поэтому здесь НЕ требуется одинаковый rate:
//!    каждое устройство открывается на своей лучшей частоте (по возможности целевой), а
//!    приведение к канонической частоте DSP делает [`crate::resample`].

use crate::streams::format_rank;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, SupportedStreamConfig, SupportedStreamConfigRange};

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

/// Открытые устройства и их конфигурации для дуплекса. Частоты входа и выхода могут
/// различаться — их приводит к канонической ресемплер.
pub struct IoConfig {
    pub input_device: Device,
    pub input_config: SupportedStreamConfig,
    pub output_device: Device,
    pub output_config: SupportedStreamConfig,
}

impl IoConfig {
    pub fn input_rate(&self) -> u32 {
        self.input_config.sample_rate().0
    }
    pub fn output_rate(&self) -> u32 {
        self.output_config.sample_rate().0
    }
}

/// Лучшая конфигурация из диапазонов, поддерживающая ровно `rate` (или `None`).
fn pick_at_rate(ranges: &[SupportedStreamConfigRange], rate: u32) -> Option<SupportedStreamConfig> {
    ranges
        .iter()
        .filter(|r| r.min_sample_rate().0 <= rate && rate <= r.max_sample_rate().0)
        .min_by_key(|r| format_rank(r.sample_format()))
        .map(|r| r.clone().with_sample_rate(cpal::SampleRate(rate)))
}

/// Лучшая конфигурация устройства: по возможности на `target_rate`, иначе — дефолтная
/// конфигурация устройства (её частоту потом подгонит ресемплер).
fn best_config(
    ranges: &[SupportedStreamConfigRange],
    target_rate: u32,
    default: Result<SupportedStreamConfig, cpal::DefaultStreamConfigError>,
    what: &str,
) -> Result<SupportedStreamConfig, String> {
    if let Some(cfg) = pick_at_rate(ranges, target_rate) {
        return Ok(cfg);
    }
    default.map_err(|e| format!("Конфигурация {what}: {e}"))
}

/// Ищет устройство по имени среди перечисленных; если названного нет (например, его
/// отключили между запусками), молча откатываемся на системное по умолчанию — это
/// надёжнее, чем отказать в старте сессии.
fn resolve_device(
    listed: Option<impl Iterator<Item = Device>>,
    fallback: Option<Device>,
    name: Option<&str>,
    what: &str,
) -> Result<Device, String> {
    if let Some(name) = name.filter(|n| !n.is_empty()) {
        if let Some(mut devs) = listed {
            if let Some(d) = devs.find(|d| d.name().map(|n| n == name).unwrap_or(false)) {
                return Ok(d);
            }
        }
        eprintln!("sonic-audio: {what} «{name}» не найден, беру системный по умолчанию");
    }
    fallback.ok_or_else(|| format!("{what} не найден"))
}

/// Открывает микрофон по имени (`None` — системный по умолчанию) и подбирает лучшую
/// конфигурацию (предпочитая `target_rate` и хороший формат сэмплов).
pub fn open_input(
    target_rate: u32,
    name: Option<&str>,
) -> Result<(Device, SupportedStreamConfig), String> {
    let host = cpal::default_host();
    let device = resolve_device(
        host.input_devices().ok(),
        host.default_input_device(),
        name,
        "Микрофон",
    )?;
    let ranges: Vec<_> = device
        .supported_input_configs()
        .map(|it| it.collect())
        .unwrap_or_default();
    let config = best_config(
        &ranges,
        target_rate,
        device.default_input_config(),
        "микрофона",
    )?;
    Ok((device, config))
}

/// То же для динамика.
pub fn open_output(
    target_rate: u32,
    name: Option<&str>,
) -> Result<(Device, SupportedStreamConfig), String> {
    let host = cpal::default_host();
    let device = resolve_device(
        host.output_devices().ok(),
        host.default_output_device(),
        name,
        "Динамик",
    )?;
    let ranges: Vec<_> = device
        .supported_output_configs()
        .map(|it| it.collect())
        .unwrap_or_default();
    let config = best_config(
        &ranges,
        target_rate,
        device.default_output_config(),
        "динамика",
    )?;
    Ok((device, config))
}

/// Открывает пару устройств по именам (`None` — системные по умолчанию).
pub fn io_config(
    target_rate: u32,
    input_name: Option<&str>,
    output_name: Option<&str>,
) -> Result<IoConfig, String> {
    let (input_device, input_config) = open_input(target_rate, input_name)?;
    let (output_device, output_config) = open_output(target_rate, output_name)?;
    Ok(IoConfig {
        input_device,
        input_config,
        output_device,
        output_config,
    })
}

/// Системные устройства по умолчанию.
pub fn default_io_config(target_rate: u32) -> Result<IoConfig, String> {
    io_config(target_rate, None, None)
}
