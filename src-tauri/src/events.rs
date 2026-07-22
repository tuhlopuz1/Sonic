//! Имена событий Tauri, которые эмитит сессия (PROTOCOL.md §11.3). Фронтенд слушает их
//! через `@tauri-apps/api/event.listen`. Собственно эмиссия — в `session.rs`; здесь —
//! единый контракт имён, чтобы Rust и TS не разъезжались.

/// Пришло целиком собранное входящее сообщение: `{ text }`.
pub const MESSAGE_RECEIVED: &str = "message-received";
/// Статус исходящего сообщения: `{ msg_id, status: "sent"|"delivered", text }`.
pub const MESSAGE_STATUS: &str = "message-status";
/// Телеметрия качества связи (SNR, режим, ретраи, RTT, PER).
pub const LINK_QUALITY: &str = "link-quality";
/// Смена состояния сессии: `{ state: "up"|"down" }`.
pub const SESSION_STATE_CHANGED: &str = "session-state-changed";
