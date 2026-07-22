//! Построение cpal-потоков для ЛЮБОГО формата сэмплов устройства.
//!
//! Железо отдаёт сэмплы в разных форматах (F32, I16, U16, U8, I32, F64…). Раньше
//! поддерживалась только тройка F32/I16/U16, и ноутбук с U8-конфигурацией просто падал
//! с «Неподдерживаемый формат динамика: U8». Здесь формат снимается один раз обобщённо
//! через `cpal::FromSample`, поэтому добавление нового формата не требует правок логики.
//!
//! Наружу отдаётся простой интерфейс: вход — колбэк «получен моно-сэмпл f32», выход —
//! колбэк «дай следующий f32». Конвертация в формат устройства — внутри.
//!
//! Паника внутри аудио-колбэка на Android разворачивается через C++-границу (Oboe) —
//! это UB и мгновенный `abort()` процесса, поэтому тело каждого колбэка обёрнуто в
//! `catch_unwind`.

use cpal::traits::DeviceTrait;
use cpal::{FromSample, Sample, SizedSample, Stream, SupportedStreamConfig};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Приоритет формата при выборе конфигурации: чем меньше, тем предпочтительнее.
/// F32 — родной для DSP (без потерь на квантование), U8 — крайний случай (8 бит!).
pub fn format_rank(f: cpal::SampleFormat) -> u8 {
    match f {
        cpal::SampleFormat::F32 => 0,
        cpal::SampleFormat::I16 => 1,
        cpal::SampleFormat::I32 => 2,
        cpal::SampleFormat::U16 => 3,
        cpal::SampleFormat::F64 => 4,
        cpal::SampleFormat::I64 => 5,
        cpal::SampleFormat::U32 => 6,
        cpal::SampleFormat::U64 => 7,
        cpal::SampleFormat::I8 => 8,
        cpal::SampleFormat::U8 => 9,
        _ => 10,
    }
}

/// Открывает поток записи; `on_mono` получает уже сведённый в моно `f32`-сэмпл.
pub fn build_input_stream(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    on_mono: impl FnMut(f32) + Send + 'static,
) -> Result<Stream, String> {
    use cpal::SampleFormat as F;
    match config.sample_format() {
        F::I8 => input_of::<i8>(device, config, on_mono),
        F::I16 => input_of::<i16>(device, config, on_mono),
        F::I32 => input_of::<i32>(device, config, on_mono),
        F::I64 => input_of::<i64>(device, config, on_mono),
        F::U8 => input_of::<u8>(device, config, on_mono),
        F::U16 => input_of::<u16>(device, config, on_mono),
        F::U32 => input_of::<u32>(device, config, on_mono),
        F::U64 => input_of::<u64>(device, config, on_mono),
        F::F32 => input_of::<f32>(device, config, on_mono),
        F::F64 => input_of::<f64>(device, config, on_mono),
        other => Err(format!("Неподдерживаемый формат микрофона: {other:?}")),
    }
}

/// Открывает поток воспроизведения; `next_sample` отдаёт очередной `f32` в [-1, 1].
pub fn build_output_stream(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    next_sample: impl FnMut() -> f32 + Send + 'static,
) -> Result<Stream, String> {
    use cpal::SampleFormat as F;
    match config.sample_format() {
        F::I8 => output_of::<i8>(device, config, next_sample),
        F::I16 => output_of::<i16>(device, config, next_sample),
        F::I32 => output_of::<i32>(device, config, next_sample),
        F::I64 => output_of::<i64>(device, config, next_sample),
        F::U8 => output_of::<u8>(device, config, next_sample),
        F::U16 => output_of::<u16>(device, config, next_sample),
        F::U32 => output_of::<u32>(device, config, next_sample),
        F::U64 => output_of::<u64>(device, config, next_sample),
        F::F32 => output_of::<f32>(device, config, next_sample),
        F::F64 => output_of::<f64>(device, config, next_sample),
        other => Err(format!("Неподдерживаемый формат динамика: {other:?}")),
    }
}

fn input_of<T>(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    mut on_mono: impl FnMut(f32) + Send + 'static,
) -> Result<Stream, String>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let channels = config.channels().max(1) as usize;
    let stream_config = config.config();
    device
        .build_input_stream(
            &stream_config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    for frame in data.chunks(channels) {
                        // Сведение в моно: среднее по каналам.
                        let sum: f32 = frame.iter().map(|&s| f32::from_sample(s)).sum();
                        on_mono(sum / frame.len().max(1) as f32);
                    }
                }));
            },
            |e| eprintln!("sonic-audio: input stream error: {e}"),
            None,
        )
        .map_err(|e| format!("Открытие потока записи: {e}"))
}

fn output_of<T>(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    mut next_sample: impl FnMut() -> f32 + Send + 'static,
) -> Result<Stream, String>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let channels = config.channels().max(1) as usize;
    let stream_config = config.config();
    device
        .build_output_stream(
            &stream_config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    for frame in data.chunks_mut(channels) {
                        let v = T::from_sample(next_sample().clamp(-1.0, 1.0));
                        for c in frame.iter_mut() {
                            *c = v;
                        }
                    }
                }));
            },
            |e| eprintln!("sonic-audio: output stream error: {e}"),
            None,
        )
        .map_err(|e| format!("Открытие потока воспроизведения: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_is_preferred_over_u8() {
        assert!(format_rank(cpal::SampleFormat::F32) < format_rank(cpal::SampleFormat::U8));
        assert!(format_rank(cpal::SampleFormat::I16) < format_rank(cpal::SampleFormat::U8));
        assert!(format_rank(cpal::SampleFormat::F32) < format_rank(cpal::SampleFormat::I16));
    }
}
