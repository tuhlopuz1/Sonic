//! Состояние приложения Tauri: активная сессия мессенджера под мьютексом
//! (`tauri::State`, ср. PROTOCOL.md §11.3).

use crate::session::SessionHandle;
use std::sync::Mutex;

#[derive(Default)]
pub struct AppState {
    /// Активная сессия (None — сессия не запущена).
    pub session: Mutex<Option<SessionHandle>>,
}
