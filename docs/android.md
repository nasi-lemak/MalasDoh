# MalasDoh on Android

MalasDoh runs on Android 8 and newer (phones, tablets, Chromebooks). It is
the same app as on the desktop, in a window Android gives it through its own
`NativeActivity`: there is no Java or Kotlin code.

**Status: early.** The layout is still the desktop one, so it is at home on
a tablet or a phone held sideways, with a keyboard if you have one.

## Install

Build the APK (below), copy it to the device, and open it; Android asks you
to allow installing apps from that source. Or, with USB debugging on:

```sh
adb install target/android/malasdoh-0.2.0-android.apk
```

## Build

You need:

- The Android SDK, with build-tools 35 or newer, a platform (android-35 or
  newer) and the NDK. Android Studio installs them (Settings → Languages &
  Frameworks → Android SDK; the NDK is under SDK Tools), or the command-line
  tools' `sdkmanager`.
- Rust 1.89 or newer and the Android targets:
  `rustup target add aarch64-linux-android x86_64-linux-android`
- A JDK (Android Studio has one), for signing.

Then:

```sh
ANDROID_HOME=~/Android/Sdk android/build-apk.sh
```

It builds for ARM phones and tablets and for x86_64 (the emulator,
Chromebooks); `ABIS=arm64-v8a` builds only the first. The APK is signed with
the usual debug key, which is fine for trying it. For a store, sign with your
own key: `MALASDOH_KEYSTORE`, `MALASDOH_KEYSTORE_PASS` and
`MALASDOH_KEY_ALIAS`. The script runs on Linux and macOS; on Windows, use WSL.

Logs: `adb logcat -s malasdoh`.

## How it fits together

- `crates/malasdoh-android`: the native library, `libmalasdoh_android.so`:
  eframe's Android window, the app, and a little glue to keep the app clear
  of the status bar, the navigation bar and the on-screen keyboard.
- `android/AndroidManifest.xml` and `android/res/`: the app's manifest and
  icon. `android/build-apk.sh` builds the library for each ABI, packs it with
  the manifest, aligns and signs the APK.
- Passwords you ask it to remember are kept in the Android Keystore.

CI builds the APK and starts it in an Android tablet emulator on every pull
request (`.github/workflows/mobile.yml`).

## Not there yet

- **A phone layout.** On a phone held upright the panels are cramped.
- **The on-screen keyboard** types plain characters; word suggestions,
  swipe typing and some languages' input methods do not work yet.
- **File pickers.** The "Browse…" buttons do nothing yet; type the path.
- **Clipboard** between MalasDoh and other apps.
- "Local files" sees only the app's own storage.
