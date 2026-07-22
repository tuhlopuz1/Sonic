//! Телеметрия качества связи — прокидывается из DSP/MAC-слоёв в UI (событие
//! `link-quality`, PROTOCOL.md §11.3). Чистые данные, без логики.

use crate::framing::PhyMode;
use serde::{Deserialize, Serialize};

/// Мгновенный снимок качества канала для отображения в UI.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LinkQuality {
    /// Оценка SNR по преамбуле/пилотам последнего кадра, дБ.
    pub snr_db: f32,
    /// Текущий активный режим модуляции.
    pub mode: PhyMode,
    /// Сколько ретрансмиссий накопилось (нарастающим итогом за сессию).
    pub retransmits: u32,
    /// Оценка RTT (скользящее среднее), мс.
    pub rtt_ms: f32,
    /// Сколько кадров успешно принято за сессию.
    pub frames_ok: u32,
    /// Сколько кадров отброшено по CRC/таймауту.
    pub frames_bad: u32,
}

impl Default for LinkQuality {
    fn default() -> Self {
        LinkQuality {
            snr_db: 0.0,
            mode: PhyMode::Css,
            retransmits: 0,
            rtt_ms: 0.0,
            frames_ok: 0,
            frames_bad: 0,
        }
    }
}

impl LinkQuality {
    /// Packet Error Rate за сессию (для критерия «устойчивость», PROTOCOL.md §12).
    pub fn per(&self) -> f32 {
        let total = self.frames_ok + self.frames_bad;
        if total == 0 {
            0.0
        } else {
            self.frames_bad as f32 / total as f32
        }
    }

    /// Обновление оценки RTT скользящим средним, как в TCP (PROTOCOL.md §7.3).
    pub fn update_rtt(&mut self, sample_ms: f32) {
        if self.rtt_ms == 0.0 {
            self.rtt_ms = sample_ms;
        } else {
            self.rtt_ms = 0.875 * self.rtt_ms + 0.125 * sample_ms;
        }
    }
}
