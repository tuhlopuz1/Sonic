package com.tuhlopuz.sonic

import android.content.Context

/**
 * Мост в Rust для того, что Tauri за нас не делает.
 *
 * Единственная задача — отдать Rust-стороне `JavaVM` и `Context`: cpal работает на Android
 * через Oboe, а тот ищет их в глобальном `ndk_context`, который в Tauri-приложении не
 * заполняет никто (см. `src-tauri/src/android_ctx.rs`). Без этого первое же обращение к
 * звуку паникует и роняет процесс.
 *
 * Нативная библиотека грузится здесь явно: `initAndroidContext` вызывается ДО
 * `super.onCreate()`, а Tauri свой `System.loadLibrary` делает только внутри него.
 * Повторный `loadLibrary` того же имени безвреден.
 */
object SonicNative {
    init {
        System.loadLibrary("tauri_app_lib")
    }

    @JvmStatic
    external fun initAndroidContext(context: Context)
}
