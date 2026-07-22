//! План спектра и шов дуплекса.
//!
//! Full-duplex через один акустический эфир получаем частотным разделением (FDD,
//! PROTOCOL.md §2): каждое направление — своя непересекающаяся полоса, поэтому эхо
//! собственного сигнала отфильтровывается полосовым фильтром на приёме, без AEC.
//!
//! Ключевая абстракция — [`DuplexScheme`]: модуляторы (CSS/OFDM) никогда не знают про
//! FDD vs shared-band, они лишь спрашивают `tx_band()/rx_band()/sample_rate()`. Это
//! чистый шов, чтобы позже добавить общую полосу + адаптивный AEC (`SharedBandAec`),
//! не переписывая модемы (plan.md §2). [`EchoCanceller`] сейчас no-op, но точка отвода
//! reference-сигнала в пайплайне заложена с самого начала.

use serde::{Deserialize, Serialize};

/// Профиль диапазона. Оба используют один протокол/кадр и оба режима модуляции —
/// различаются только границами полос и (для ультразвука) форсированным sample rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Слышимый профиль (по умолчанию): 500–15000 Гц, штатные mic/speaker.
    Audible,
    /// Ультразвуковой профиль: 21–24 кГц, для отдельного трека на ESP32-модулях.
    /// На штатном железе часто не тянется (plan.md риск №3) — скрыт в UI до Фазы 7.
    Ultrasonic,
}

/// Роль устройства в сессии. Раз штатных каналов связи нет, авто-согласовать некому —
/// это явный выбор пользователя при старте (plan.md §2). Инициатор всегда получает
/// «нижнюю» полосу на передачу, респондер — «верхнюю» (PROTOCOL.md §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Сторона A — инициатор, TX в нижней полосе.
    Initiator,
    /// Сторона B — респондер, TX в верхней полосе.
    Responder,
}

impl Role {
    /// Направление в заголовке кадра (PROTOCOL.md §6.1, поле Direction).
    pub fn direction_bit(self) -> u8 {
        match self {
            Role::Initiator => 0, // A→B
            Role::Responder => 1, // B→A
        }
    }
}

/// Непрерывная под-полоса частот, [center-bw/2, center+bw/2].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubBand {
    pub center_hz: f32,
    pub bandwidth_hz: f32,
}

impl SubBand {
    pub const fn from_edges(low_hz: f32, high_hz: f32) -> Self {
        SubBand {
            center_hz: (low_hz + high_hz) * 0.5,
            bandwidth_hz: high_hz - low_hz,
        }
    }
    pub fn low_hz(&self) -> f32 {
        self.center_hz - self.bandwidth_hz * 0.5
    }
    pub fn high_hz(&self) -> f32 {
        self.center_hz + self.bandwidth_hz * 0.5
    }
}

/// Параметры плана спектра для профиля: две рабочие полосы (нижняя/верхняя) и sample rate.
#[derive(Debug, Clone, Copy)]
pub struct BandPlan {
    pub lower: SubBand,
    pub upper: SubBand,
    pub sample_rate: u32,
}

impl Profile {
    /// Границы полос из PROTOCOL.md §2.2 (слышимый) и plan.md §2 (ультразвук).
    /// Ультразвук обязательно 48 кГц (Nyquist=24кГц), верхняя граница у самого Nyquist —
    /// на реальном ESP32-железе проверять эмпирически, не считать данностью.
    pub fn band_plan(self) -> BandPlan {
        match self {
            Profile::Audible => BandPlan {
                // Полоса A: 500–7200, guard 7200–7800, полоса B: 7800–15000.
                lower: SubBand::from_edges(500.0, 7200.0),
                upper: SubBand::from_edges(7800.0, 15000.0),
                sample_rate: 48_000,
            },
            Profile::Ultrasonic => BandPlan {
                lower: SubBand::from_edges(21_000.0, 22_400.0),
                upper: SubBand::from_edges(22_600.0, 24_000.0),
                sample_rate: 48_000,
            },
        }
    }
}

/// Абстракция дуплексной схемы. Модуляторы работают только через неё, поэтому переход
/// FDD → shared-band+AEC не требует правок в CSS/OFDM (plan.md §2).
pub trait DuplexScheme: Send {
    /// Полоса, в которой ЭТО устройство передаёт.
    fn tx_band(&self) -> SubBand;
    /// Полоса, в которой это устройство принимает (полоса передачи пира).
    fn rx_band(&self) -> SubBand;
    fn sample_rate(&self) -> u32;
    fn role(&self) -> Role;
    /// Эхоподавитель для приёмного тракта. Сейчас всегда [`NoopEchoCanceller`];
    /// в shared-band версии — реальный NLMS-фильтр.
    fn echo_canceller(&self) -> Box<dyn EchoCanceller>;
}

/// Частотное разделение: нижняя полоса — инициатору, верхняя — респондеру.
#[derive(Debug, Clone, Copy)]
pub struct Fdd {
    pub role: Role,
    pub profile: Profile,
}

impl Fdd {
    pub fn new(role: Role, profile: Profile) -> Self {
        Fdd { role, profile }
    }
}

impl DuplexScheme for Fdd {
    fn tx_band(&self) -> SubBand {
        let plan = self.profile.band_plan();
        match self.role {
            Role::Initiator => plan.lower,
            Role::Responder => plan.upper,
        }
    }
    fn rx_band(&self) -> SubBand {
        let plan = self.profile.band_plan();
        match self.role {
            Role::Initiator => plan.upper,
            Role::Responder => plan.lower,
        }
    }
    fn sample_rate(&self) -> u32 {
        self.profile.band_plan().sample_rate
    }
    fn role(&self) -> Role {
        self.role
    }
    fn echo_canceller(&self) -> Box<dyn EchoCanceller> {
        // FDD не нужен AEC — эхо режется полосовым фильтром. Но пайплайн всё равно
        // прокидывает reference-сигнал, чтобы shared-band версия встала без переделки.
        Box::new(NoopEchoCanceller)
    }
}

/// Эхоподавитель приёмного тракта. `reference` — точная копия того, что мы сейчас
/// воспроизводим (см. plan.md §2/§3): даже no-op обязан её принимать, иначе позже
/// придётся переделывать пайплайн, а не только подставлять NLMS.
pub trait EchoCanceller: Send {
    /// Обработать блок принятых сэмплов, зная воспроизводимый reference того же интервала.
    /// Возвращает «очищенный» сигнал (для FDD — без изменений).
    fn process(&mut self, captured: &mut [f32], reference: &[f32]);
}

/// No-op: для FDD эхо и так вне полосы приёма. Reference принимается и игнорируется —
/// это осознанный шов, а не забытая заглушка (plan.md §2).
pub struct NoopEchoCanceller;

impl EchoCanceller for NoopEchoCanceller {
    fn process(&mut self, _captured: &mut [f32], _reference: &[f32]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fdd_bands_do_not_overlap_and_swap_by_role() {
        let a = Fdd::new(Role::Initiator, Profile::Audible);
        let b = Fdd::new(Role::Responder, Profile::Audible);
        // Инициатор передаёт там, где респондер принимает, и наоборот.
        assert_eq!(a.tx_band(), b.rx_band());
        assert_eq!(a.rx_band(), b.tx_band());
        // Полосы не перекрываются (guard band между ними).
        assert!(a.tx_band().high_hz() < a.rx_band().low_hz());
    }

    #[test]
    fn audible_within_hardware_limits() {
        let plan = Profile::Audible.band_plan();
        assert!(plan.lower.low_hz() >= 500.0);
        assert!(plan.upper.high_hz() <= 15_000.0);
        assert_eq!(plan.sample_rate, 48_000);
    }

    #[test]
    fn ultrasonic_stays_below_nyquist() {
        let plan = Profile::Ultrasonic.band_plan();
        assert!(plan.upper.high_hz() <= (plan.sample_rate / 2) as f32);
    }
}
