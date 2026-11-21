# MalasDoh on Android

MalasDoh runs on Android 8 and newer (phones, tablets, Chromebooks). It is
the same app as on the desktop, in a window Android gives it through its
own `NativeActivity`, with a little Java on top for the keyboard, the
clipboard and the file picker.

**Status: early.** It has run in the Android emulator (phone and tablet) but
not yet on a real device.

On a phone it uses its phone layout: the connection list fills the screen
until you pick one, and inside a connection Terminals, Files and This
device take turns. Taps open things (a long press shows the menu), and the
terminal has a row of the keys phone keyboards lack: Esc, Tab, Ctrl, the
arrows, `| ~ / - :`, Home, End, PgUp, PgDn and Paste.

In the terminal, swipe to scroll back (or to scroll a full-screen program),
pinch to zoom, and press and hold to start selecting text; Copy then
appears in the row of keys.

While connections are open, MalasDoh keeps running in the background with a
notification ("2 connections open"), so switching apps does not drop them.
Android's battery saving can still pause it after a long while; MalasDoh
then reconnects when you come back. For long jobs, use a lasting terminal
(… menu): it keeps running on the server whatever happens to the phone.

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

## Lock, share, hardware keyboard

- **App lock:** Settings → General → "Lock with fingerprint, face or PIN".
  MalasDoh then asks when it starts and when you come back after half a
  minute away; open connections stay open behind the lock.
- **Share:** in "This device", Share… on a file hands it to another app
  (mail, a viewer, a chat).
- **Back** (Android) closes a menu, dialog or window, and from a connection
  goes back to the list. It never quits the app.
- **A hardware keyboard** (a tablet's) hides the terminal's row of extra keys.

## How it fits together

- `crates/malasdoh-android`: the native library, `libmalasdoh_android.so`:
  eframe's Android window, the app, and the glue: the window insets (status
  bar, navigation bar, keyboard) through JNI, the keyboard, the clipboard
  and the file picker.
- `android/java/…/MainActivity.java`: NativeActivity plus an input view
  with a real connection to the on-screen keyboard (suggestions, swipe
  typing, composing input methods; in a terminal every key goes straight
  through), the clipboard, and the system file picker.
- `android/AndroidManifest.xml` and `android/res/`: the manifest and icon.
  `android/build-apk.sh` builds the library for each ABI and the Java, packs
  them with the manifest, aligns and signs the APK.
- Passwords you ask it to remember are kept in the Android Keystore.
- Files you pick are copied into the app; downloads and saved files go to
  its Documents folder, where "This device" starts.

CI builds the APK and runs it in a phone and a tablet emulator on every pull
request, tapping through the search box (with the keyboard up) and the new
connection form (`.github/workflows/mobile.yml`).

## Not there yet

- **Downloads you can open from other apps:** they stay in the app's own
  storage for now (a Share button is next).
- Not yet tried on a real phone or tablet.
