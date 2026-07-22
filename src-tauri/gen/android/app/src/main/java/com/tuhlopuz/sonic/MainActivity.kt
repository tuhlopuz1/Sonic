package com.tuhlopuz.sonic

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    // СТРОГО до super.onCreate(): внутри него стартует Rust-сторона Tauri, а она сразу
    // же перечисляет аудио-устройства через cpal/Oboe — а тот берёт JavaVM и Context
    // только из ndk_context, который Tauri не заполняет (см. src-tauri/src/android_ctx.rs).
    // applicationContext доступен уже здесь: базовый контекст прикручен в attach(),
    // до onCreate. Берём именно его, а не Activity — он живёт всё время работы процесса.
    SonicNative.initAndroidContext(applicationContext)

    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // Проверка канала использует микрофон (cpal/AAudio) — разрешение на запись звука
    // не может быть получено из чистого Rust на Android, запрашиваем его здесь при старте.
    if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
        != PackageManager.PERMISSION_GRANTED
    ) {
      ActivityCompat.requestPermissions(this, arrayOf(Manifest.permission.RECORD_AUDIO), 4242)
    }
  }
}
