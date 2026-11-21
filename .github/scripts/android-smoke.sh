#!/bin/sh
# Installs the APK in the running emulator, starts it, and checks that it is
# still running 20 seconds later; then taps through a short tour (search
# with the on-screen keyboard, the new-connection form) and prints a
# screenshot of each step.
set -eu
package=io.github.nasi_lemak.malasdoh
apk=$(ls target/android/*.apk)
adb install -r "$apk"
adb logcat -c
adb shell am start -W -n "$package/.MainActivity"
sleep 20

# What went on, when something fails.
diagnose() {
    echo "--- activities"
    adb shell dumpsys activity activities | grep -iE "resumed|focused|malasdoh" | head -n 20 || true
    echo "--- window focus"
    adb shell dumpsys window | grep -E "mCurrentFocus|mFocusedApp" || true
    echo "--- system log"
    adb logcat -d -s ActivityTaskManager:I ActivityManager:I WindowManager:W malasdoh:V RustStdoutStderr:V \
        AndroidRuntime:E DEBUG:V libc:F | tail -n 120
}
shot() {
    echo "--- screenshot: $1"
    adb exec-out screencap -p > "target/android/$1.png"
    if ! python3 .github/scripts/show-screenshot.py "target/android/$1.png" "$1"; then
        diagnose
        exit 1
    fi
}
alive() {
    pid=$(adb shell pidof "$package" || true)
    if [ -z "$pid" ]; then
        echo "MalasDoh is not running: it crashed or did not start."
        diagnose
        exit 1
    fi
    if adb logcat -d | grep -q "ANR in $package"; then
        echo "MalasDoh stopped responding (ANR)."
        adb logcat -d | grep -A 20 "ANR in $package" | head -n 40
        diagnose
        exit 1
    fi
}

alive
shot start

# Where things are, in points from the top left of the app (see the
# layouts in crates/malasdoh/src/app.rs); the app's top inset comes from
# its log, the density from the device.
density=$(adb shell wm density | sed -n 's/.*: //p' | tail -n 1 | tr -d '\r')
scale=$(awk "BEGIN { print $density / 160 }")
top=$(adb logcat -d -s malasdoh:I | sed -n 's/.*insets (left, top, right, bottom): \[[0-9]*, \([0-9]*\),.*/\1/p' | tail -n 1)
top=${top:-0}
width=$(adb shell wm size | sed -n 's/.*: \([0-9]*\)x.*/\1/p' | tail -n 1 | tr -d '\r')
wide=$(awk "BEGIN { print ($width / $scale >= 640) ? 1 : 0 }")
tap() { # x y in points
    adb shell input tap "$(awk "BEGIN { printf \"%d\", $1 * $scale }")" "$(awk "BEGIN { printf \"%d\", $top + $2 * $scale }")"
    sleep 2
}
echo "density $density, top inset $top px, width $width px, wide layout: $wide"

if [ "$wide" = 1 ]; then search_y=96; else search_y=102; fi
tap 120 "$search_y"
adb shell input text "web"
sleep 2
shot keyboard
alive

adb shell input keyevent 4 # Back: the keyboard goes away
sleep 1
tap 70 175 # New connection…
shot form
alive
if [ "$wide" = 0 ]; then
    tap 160 222 # Host
    adb shell input text "example.com"
    sleep 2
    shot form-typing
    alive
fi

echo "--- log"
adb logcat -d -s malasdoh:V RustStdoutStderr:V AndroidRuntime:E DEBUG:V libc:F | tail -n 80
echo "MalasDoh is running (pid $pid)."
