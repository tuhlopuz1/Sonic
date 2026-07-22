//! Заполнение `ndk-context` — глобального «где тут JavaVM и Context» для Rust-крейтов.
//!
//! Через него cpal находит Java-часть звука на Android: перечисление устройств
//! (`AudioManager.getDevices`) и подбор размера буфера (`AudioRecord.getMinBufferSize`) —
//! это вызовы Java-API, и `oboe` (backend cpal под Android) берёт VM с контекстом
//! ТОЛЬКО из `ndk_context`.
//!
//! Обычно `ndk_context` заполняет рантайм (`ndk-glue`), но Tauri/tao держат собственный
//! android-контекст и `ndk_context::initialize_android_context` не зовут нигде — во всём
//! дереве зависимостей нет ни одного вызова. Из-за этого `ndk_context::android_context()`
//! паникует («android context was not initialized») на первом же обращении к звуку:
//! `list_audio_devices` при старте UI, `check_channel`, `discover_devices`,
//! `start_session`. Синхронная tauri-команда выполняется прямо в JNI-кадре
//! (`extern "system"`), поэтому такая паника разворачивается через границу FFI и роняет
//! процесс — приложение «просит микрофон и падает».
//!
//! Поэтому заполняем контекст сами: `MainActivity` зовёт
//! `SonicNative.initAndroidContext(applicationContext)` ДО `super.onCreate()`, то есть до
//! старта Rust-стороны Tauri и до первого обращения к cpal.

use jni::objects::{GlobalRef, JClass, JObject};
use jni::JNIEnv;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// `ndk_context` хранит только сырые указатели и владельцем объекта не является —
/// глобальную ссылку держим здесь, иначе GC заберёт Context из-под Oboe.
static CONTEXT: OnceLock<GlobalRef> = OnceLock::new();
static READY: AtomicBool = AtomicBool::new(false);

/// Готов ли JNI-контекст. Пока не готов, любое обращение к cpal на Android паникует,
/// поэтому вызывающая сторона обязана вернуть ошибку, а не лезть в звук.
pub fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Реализация `com.tuhlopuz.sonic.SonicNative.initAndroidContext(Context)`.
#[no_mangle]
pub extern "system" fn Java_com_tuhlopuz_sonic_SonicNative_initAndroidContext(
    env: JNIEnv,
    _class: JClass,
    context: JObject,
) {
    // Паника, вылетевшая наружу через `extern "system"`, — abort всего процесса.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| store(&env, &context)));
}

fn store(env: &JNIEnv, context: &JObject) {
    let (Ok(vm), Ok(global)) = (env.get_java_vm(), env.new_global_ref(context)) else {
        return;
    };
    let vm_ptr = vm.get_java_vm_pointer().cast();

    // Activity пересоздают (поворот экрана, смена темы), а повторный
    // `initialize_android_context` падает по внутреннему assert — поэтому право на
    // инициализацию получает только тот вызов, который выиграл `OnceLock::set`.
    if CONTEXT.set(global).is_err() {
        return;
    }
    let ctx_ptr = CONTEXT
        .get()
        .expect("ссылка только что положена")
        .as_obj()
        .as_raw()
        .cast();

    unsafe { ndk_context::initialize_android_context(vm_ptr, ctx_ptr) };
    READY.store(true, Ordering::Release);
}
