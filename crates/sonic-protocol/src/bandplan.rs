//! План спектра и шов дуплекса.
//!
//! **Рабочий режим по умолчанию — полудуплекс (TDD, [`Tdd`]) в ОДНОЙ общей полосе внутри
//! «рабочей зоны» железа (~0.5–3.7 кГц).** Так сделано осознанно после того, как выяснилось,
//! почему связь между устройствами не работала: прежняя FDD-схема ставила обратное
//! направление (и все ACK) в верхнюю полосу 7.8–15 кГц, которую реальные динамики/микрофоны
//! почти не воспроизводят (см. `sim::hardware`, тест `hardware_link`). Из-за этого ACK не
//! доходили ни в какую сторону и связь рушилась при зелёной (спектрально-плоской) симуляции.
//!
//! Полудуплекс в общей нижней полосе решает это радикально: оба направления в одной хорошо
//! воспроизводимой зоне, нет одновременного TX+RX (значит нет самоподавления/интермодуляции
//! динамика в свою же полосу приёма), а loopback-самотест гоняет ровно тот же тракт, что и
//! реальная связь. Разделяет свои/чужие кадры бит направления в заголовке (роль A/B), а
//! приёмник глушится на время собственной передачи (см. `sonic-audio::duplex`).
//!
//! FDD ([`Fdd`]) сохранён для ультразвукового профиля/ESP32 (там отдельный тракт), но в
//! слышимом профиле не используется.
//!
//! Ключевая абстракция — [`DuplexScheme`]: модуляторы (CSS/OFDM) не знают про TDD vs FDD,
//! они лишь спрашивают `tx_band()/rx_band()/sample_rate()`.

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

/// Параметры плана спектра для профиля: общая полудуплексная полоса `data` (основной режим),
/// пара FDD-полос `lower`/`upper` (только для ультразвукового тракта) и sample rate.
#[derive(Debug, Clone, Copy)]
pub struct BandPlan {
    /// Общая полоса полудуплекса (TDD) — оба устройства TX и RX здесь. Выбрана в «рабочей
    /// зоне» железа, чтобы гарантированно проходить на любых динамиках/микрофонах.
    pub data: SubBand,
    pub lower: SubBand,
    pub upper: SubBand,
    pub sample_rate: u32,
}

impl Profile {
    /// Границы полос. Слышимый профиль: общая полудуплексная полоса **504–3696 Гц**
    /// (center 2100, BW 3200) — вся энергия кадра лежит в зоне, которую воспроизводит
    /// практически любой динамик/микрофон, включая голосовой тракт телефона (срез ~3.8 кГц).
    /// Это на порядок надёжнее прежних 0.5–15 кГц: верхняя половина того диапазона на
    /// реальном железе почти не звучала, из-за чего связь и ACK не проходили.
    ///
    /// Ультразвук (для ESP32-тракта, plan.md §2) сохраняет FDD-разнос — там отдельные
    /// преобразователи; на штатном железе профиль скрыт.
    pub fn band_plan(self) -> BandPlan {
        match self {
            Profile::Audible => BandPlan {
                // Полудуплекс: одна общая полоса в «рабочей зоне» железа.
                data: SubBand::from_edges(504.0, 3696.0),
                // FDD-полосы для слышимого профиля больше не используются (оставлены для
                // обратной совместимости API/тестов), но приведены в ту же рабочую зону.
                lower: SubBand::from_edges(504.0, 3696.0),
                upper: SubBand::from_edges(504.0, 3696.0),
                sample_rate: 48_000,
            },
            Profile::Ultrasonic => BandPlan {
                data: SubBand::from_edges(21_000.0, 24_000.0),
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

/// Полудуплекс (TDD) в ОДНОЙ общей полосе — рабочая схема слышимого профиля. Оба
/// устройства передают и принимают в `data`-полосе; направление своё/чужое различает бит
/// направления в заголовке (роль A/B), а приёмник глушится на время своей передачи
/// (`sonic-audio::duplex`), поэтому одновременного TX+RX в одной полосе не возникает.
///
/// Почему не FDD: см. модульный комментарий и `sim::hardware`. Коротко — вторая FDD-полоса
/// неизбежно попадала в диапазон, который железо не воспроизводит, и обратный канал (ACK)
/// умирал. В общей нижней полосе оба направления одинаково надёжны.
#[derive(Debug, Clone, Copy)]
pub struct Tdd {
    pub role: Role,
    pub profile: Profile,
}

impl Tdd {
    pub fn new(role: Role, profile: Profile) -> Self {
        Tdd { role, profile }
    }
}

impl DuplexScheme for Tdd {
    fn tx_band(&self) -> SubBand {
        self.profile.band_plan().data
    }
    fn rx_band(&self) -> SubBand {
        self.profile.band_plan().data
    }
    fn sample_rate(&self) -> u32 {
        self.profile.band_plan().sample_rate
    }
    fn role(&self) -> Role {
        self.role
    }
    fn echo_canceller(&self) -> Box<dyn EchoCanceller> {
        // Полудуплекс: во время передачи приём заглушен (engine), поэтому собственного эха
        // в приёмном тракте нет — AEC не нужен. Reference всё равно прокидывается (шов).
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
        // FDD-разнос сохраняется для ультразвукового профиля (отдельный ESP32-тракт).
        let a = Fdd::new(Role::Initiator, Profile::Ultrasonic);
        let b = Fdd::new(Role::Responder, Profile::Ultrasonic);
        // Инициатор передаёт там, где респондер принимает, и наоборот.
        assert_eq!(a.tx_band(), b.rx_band());
        assert_eq!(a.rx_band(), b.tx_band());
        // Полосы не перекрываются (guard band между ними).
        assert!(a.tx_band().high_hz() < a.rx_band().low_hz());
    }

    #[test]
    fn tdd_shares_one_band_both_directions_and_roles() {
        // Полудуплекс: оба устройства и оба направления — в одной общей полосе.
        let a = Tdd::new(Role::Initiator, Profile::Audible);
        let b = Tdd::new(Role::Responder, Profile::Audible);
        assert_eq!(a.tx_band(), a.rx_band());
        assert_eq!(a.tx_band(), b.tx_band());
        assert_eq!(a.rx_band(), b.rx_band());
        // Различает пиров только бит направления (роль), а не полоса.
        assert_ne!(a.role().direction_bit(), b.role().direction_bit());
    }

    #[test]
    fn audible_data_band_sits_in_hardware_sweet_spot() {
        // Вся полоса — внутри зоны, которую воспроизводит любое железо (включая голосовой
        // тракт телефона со срезом ~3.8 кГц). Это и есть корневой фикс связи.
        let plan = Profile::Audible.band_plan();
        assert!(plan.data.low_hz() >= 400.0, "низ {} слишком низкий", plan.data.low_hz());
        assert!(plan.data.high_hz() <= 3800.0, "верх {} выше рабочей зоны", plan.data.high_hz());
        assert_eq!(plan.sample_rate, 48_000);
    }

    #[test]
    fn ultrasonic_stays_below_nyquist() {
        let plan = Profile::Ultrasonic.band_plan();
        assert!(plan.upper.high_hz() <= (plan.sample_rate / 2) as f32);
    }
}
