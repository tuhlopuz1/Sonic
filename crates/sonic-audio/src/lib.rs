//! sonic-audio — cpal duplex-движок поверх [`sonic_protocol`].
//!
//! Здесь живёт весь код, зависящий от звукового железа: перечисление устройств,
//! одновременные input+output cpal-потоки и пайплайн, соединяющий сырые сэмплы с
//! модемом. DSP-математики тут нет — она вся в `sonic-protocol` (см. требование
//! «логика звука на стороне rust, слои разделены»).

pub mod device;
pub mod duplex;
pub mod pipeline;

pub use device::{list_devices, DeviceList};
pub use duplex::{DuplexEngine, EngineConfig};
pub use pipeline::{RxDemodulator, RxEvent, Transmitter};
