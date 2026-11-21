#!/bin/sh
# Installs the APK in the running emulator, starts it, and checks that it is
# still running 20 seconds later; then taps through a short tour (search
# with the on-screen keyboard, the new-connection form) and prints a
# screenshot of each step.
set -eu
package=io.github.nasi_lemak.malasdoh
apk=$(ls target/android/*.apk)
adb install -r "$apk"
# The notification for open connections: allowed up front, so Android's
# question does not cover the app mid-test.
adb shell pm grant "$package" android.permission.POST_NOTIFICATIONS || true
adb logcat -c
if [ "${E2E:-0}" = 1 ]; then
    # The end-to-end run's connection, put in place before the app starts
    # (the CI build is debuggable, so run-as may write the app's files).
    cat > target/android/profiles.toml <<'TOML'
[[profile]]
id = "6f1d2c3e-1111-4a5b-9c1d-0123456789ab"
name = "ci"
host = "10.0.2.2"
port = 2222
user = "demo"
TOML
    adb shell "run-as $package sh -c 'mkdir -p files/malasdoh; cat > files/malasdoh/profiles.toml'" \
        < target/android/profiles.toml
    adb shell "run-as $package cat files/malasdoh/profiles.toml"
fi
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

# The whole way, on a phone, against the SSH server the workflow started on
# the host (10.0.2.2 from the emulator): make a connection with the form,
# connect, trust the host key, log in with a password, run something, swipe
# back through it, leave the app for a while and come back still connected.
key() { adb shell input keyevent "$@"; sleep 1; }
# What the shell in the app did shows on the server, this machine: a file
# made there proves the keys got through and the connection is up.
expect_file() {
    for _ in $(seq 1 15); do
        if sudo test -f "/home/demo/$1"; then
            echo "ok: the app's shell made $1 on the server"
            return 0
        fi
        sleep 1
    done
    echo "FAILED: no $1 on the server ($2)"
    diagnose
    exit 1
}
text() { adb shell input text "$1"; sleep 1; }
end_to_end() {
    echo "=== end to end"
    # The connection was put in place before the app started (seed below):
    # find it, and Enter connects to it.
    tap 120 "$search_y"; text ci; key 66
    sleep 4
    shot e2e-hostkey
    key 61; key 66            # Tab to Trust and connect, Enter
    sleep 4
    shot e2e-password
    text malasdoh-ci-only; key 66
    sleep 6
    alive
    text 'touch%se2e-connected'; key 66
    expect_file e2e-connected "connecting, logging in and typing in the terminal"
    text 'seq%s1%s200'; key 66
    sleep 2
    shot e2e-terminal
    # A swipe down the terminal goes back through what scrolled away.
    adb shell input swipe 540 1000 540 1900 500; sleep 2
    shot e2e-swiped
    alive
    # Away for a while: the connection should stay up meanwhile.
    key 3                     # Home
    sleep 45
    alive
    if adb shell dumpsys activity services "$package" | grep -q "isForeground=true"; then
        echo "ok: kept running in the background by its foreground service"
    else
        echo "FAILED: no foreground service while in the background"
        adb shell dumpsys activity services "$package" | head -n 40
        diagnose
        exit 1
    fi
    adb shell am start -n "$package/.MainActivity" >/dev/null
    sleep 5
    adb shell input tap 540 1500; sleep 1   # the terminal, to type into it
    text 'touch%se2e-still-connected'; key 66
    expect_file e2e-still-connected "the same connection after 45 seconds in the background"
    shot e2e-back
    alive
    # Back to the connection list for the tour.
    key 4; key 4
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

if [ "${E2E:-0}" = 1 ]; then
    end_to_end
fi

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
    tap 160 232 # Host
    adb shell input text "example.com"
    sleep 2
    shot form-typing
    alive
fi

echo "--- log"
adb logcat -d -s malasdoh:V RustStdoutStderr:V AndroidRuntime:E DEBUG:V libc:F | tail -n 80
echo "MalasDoh is running (pid $pid)."
