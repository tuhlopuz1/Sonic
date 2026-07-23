//! RECORD_AUDIO — на Android это опасное runtime-разрешение (API 23+), и голый Rust
//! не может его запросить сам (нужен Activity/Java-side вызов) — см. `plan.md` риск №1
//! и `tauri-apps/tauri#10846`. Сам запрос диалога сделан в Kotlin
//! (`MainActivity.onCreate`, на главном потоке — это Android-требование для
//! `requestPermissions`). Отсюда только проверяем текущее состояние через JNI
//! (`checkSelfPermission` можно звать из любого потока) перед тем как трогать cpal/AAudio.

#[cfg(target_os = "android")]
pub fn ensure_record_audio_permission() -> Result<bool, String> {
    use jni::objects::{JObject, JValue};
    use jni::JavaVM;

    const PERMISSION_GRANTED: i32 = 0;

    // `ndk_context::android_context()` не возвращает ошибку, а паникует, если контекст не
    // заполнен (см. `android_ctx`), — а паника отсюда прилетает в JNI-кадр tauri-команды
    // и убивает процесс. Поэтому сначала проверяем готовность своим флагом.
    if !crate::android_ctx::is_ready() {
        return Err(
            "JNI-контекст Android не инициализирован — SonicNative.initAndroidContext не был вызван"
                .to_string(),
        );
    }

    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }
        .map_err(|e| format!("JavaVM::from_raw: {e}"))?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("attach_current_thread: {e}"))?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    let permission = env
        .new_string("android.permission.RECORD_AUDIO")
        .map_err(|e| format!("new_string: {e}"))?;

    let result = env
        .call_method(
            &activity,
            "checkSelfPermission",
            "(Ljava/lang/String;)I",
            &[JValue::Object(&permission)],
        )
        .map_err(|e| format!("checkSelfPermission: {e}"))?
        .i()
        .map_err(|e| format!("checkSelfPermission return value: {e}"))?;

    Ok(result == PERMISSION_GRANTED)
}

#[cfg(not(target_os = "android"))]
pub fn ensure_record_audio_permission() -> Result<bool, String> {
    Ok(true)
}
