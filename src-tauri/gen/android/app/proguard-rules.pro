# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile

# JNI ищет реализацию по ИМЕНИ класса и метода (Java_com_tuhlopuz_sonic_SonicNative_...),
# поэтому переименовывать их R8 нельзя — иначе UnsatisfiedLinkError при старте.
# Автогенерируемый proguard-wry.pro сейчас накрывает это правилом на весь пакет, но
# полагаться на чужой сгенерированный файл для СВОЕГО кода не стоит.
-keep class com.tuhlopuz.sonic.SonicNative {
  public <init>(...);
  native <methods>;
  public static <methods>;
}