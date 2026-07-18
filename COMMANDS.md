# Сборка и запуск по платформам

Перед первым запуском в новом терминале (Windows PowerShell), если `cargo`/`rustc` не находятся:

```powershell
$env:Path += ";$env:USERPROFILE\.cargo\bin"
```

Переменные `JAVA_HOME`, `ANDROID_HOME`, `NDK_HOME` прописаны в пользовательских env-переменных — нужен новый терминал, чтобы они подхватились (или см. блок Android ниже).

## Windows / Linux / macOS (desktop)

```bash
npm install              # один раз, после клонирования репо
npm run tauri dev        # запуск в dev-режиме с hot reload
npm run tauri build       # релизная сборка + инсталляторы (msi/nsis на Windows, dmg на macOS, deb/AppImage на Linux)
```

Собранные бинарники: `src-tauri/target/release/`
Инсталляторы: `src-tauri/target/release/bundle/`

## Android

Нужны: Android SDK + NDK (уже настроено), JDK 17+, подключённое устройство/эмулятор или запущенный AVD.

```bash
npm run tauri android dev     # запуск на подключённом устройстве/эмуляторе с hot reload
npm run tauri android build   # релизный APK/AAB
```

Если переменные окружения не подхватились в текущей сессии PowerShell:

```powershell
$env:JAVA_HOME = "C:\Program Files\Android\Android Studio\jbr"
$env:ANDROID_HOME = "$env:LOCALAPPDATA\Android\Sdk"
$env:NDK_HOME = "$env:ANDROID_HOME\ndk\28.2.13676358"
```

Собранные APK/AAB: `src-tauri/gen/android/app/build/outputs/`

## iOS

Требует macOS с установленным Xcode — недоступно на этой (Windows) машине. На Mac:

```bash
npm run tauri ios init    # один раз, генерирует src-tauri/gen/apple
npm run tauri ios dev     # запуск в симуляторе/на устройстве
npm run tauri ios build   # релизная сборка (ipa)
```
