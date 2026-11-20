#!/bin/sh
# Starts an iPad Simulator, installs and starts the app, and checks that it
# is still running 20 seconds later; prints its output and a screenshot.
set -eu
bundle=io.github.nasi-lemak.malasdoh
udid=$(xcrun simctl list devices available -j | python3 -c '
import json, sys
devices = json.load(sys.stdin)["devices"]
for runtime in sorted(devices, reverse=True):
    if ".iOS-" in runtime:
        for d in devices[runtime]:
            if d["name"].startswith("iPad"):
                print(d["udid"])
                sys.exit()
')
[ -n "$udid" ] || { echo "no iPad Simulator"; xcrun simctl list devices available; exit 1; }
xcrun simctl boot "$udid"
xcrun simctl bootstatus "$udid" -b
xcrun simctl install "$udid" target/ios/simulator/MalasDoh.app
out=$PWD/target/ios/out.log
xcrun simctl launch --stdout="$out" --stderr="$out" "$udid" "$bundle"
sleep 20
echo "--- output"
tail -n 150 "$out" || true
xcrun simctl io "$udid" screenshot target/ios/screen.png
python3 .github/scripts/show-screenshot.py target/ios/screen.png
if ! xcrun simctl spawn "$udid" launchctl list | grep -q "$bundle"; then
    echo "MalasDoh is not running: it crashed or did not start."
    xcrun simctl spawn "$udid" log show --last 2m --style compact --predicate 'process == "MalasDoh"' | tail -n 80 || true
    exit 1
fi
echo "MalasDoh is running."
