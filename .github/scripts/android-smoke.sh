#!/bin/sh
# Installs the APK in the running emulator, starts it, and checks that it is
# still running 20 seconds later; prints its log and a screenshot.
set -eu
package=io.github.nasi_lemak.malasdoh
apk=$(ls target/android/*.apk)
adb install -r "$apk"
adb logcat -c
adb shell am start -W -n "$package/android.app.NativeActivity"
sleep 20
echo "--- log"
adb logcat -d -s malasdoh:V RustStdoutStderr:V AndroidRuntime:E DEBUG:V libc:F | tail -n 150
adb exec-out screencap -p > target/android/screen.png
python3 .github/scripts/show-screenshot.py target/android/screen.png
pid=$(adb shell pidof "$package" || true)
if [ -z "$pid" ]; then
    echo "MalasDoh is not running: it crashed or did not start."
    exit 1
fi
echo "MalasDoh is running (pid $pid)."
