#!/bin/sh
# Builds the iOS / iPadOS app bundle, on a Mac with Xcode.
#
#   ios/build-app.sh                 # for the Simulator: target/ios/simulator/MalasDoh.app
#   ios/build-app.sh --device        # for an iPhone or iPad: target/ios/device/MalasDoh.app
#
# Needs the Rust targets, once:
#   rustup target add aarch64-apple-ios aarch64-apple-ios-sim
#
# --device signs the app, so it needs your Apple developer identity and a
# development provisioning profile for the bundle ID below (Xcode makes both:
# Settings > Accounts; or developer.apple.com):
#   MALASDOH_IOS_IDENTITY="Apple Development: Your Name (TEAMID)"
#   MALASDOH_IOS_PROFILE=/path/to/profile.mobileprovision
#   MALASDOH_IOS_BUNDLE_ID=com.example.malasdoh   (if the profile's ID differs)
set -eu

kind=simulator
[ "${1:-}" = "--device" ] && kind=device
min_ios=15.0
bundle_id=${MALASDOH_IOS_BUNDLE_ID:-io.github.nasi-lemak.malasdoh}

here=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$here")
cd "$root"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)

if [ $kind = device ]; then
    target=aarch64-apple-ios
    platform=iPhoneOS
    : "${MALASDOH_IOS_IDENTITY:?set MALASDOH_IOS_IDENTITY to your signing identity}"
    : "${MALASDOH_IOS_PROFILE:?set MALASDOH_IOS_PROFILE to your .mobileprovision}"
else
    platform=iPhoneSimulator
    case $(uname -m) in arm64) target=aarch64-apple-ios-sim ;; *) target=x86_64-apple-ios ;; esac
fi

export IPHONEOS_DEPLOYMENT_TARGET=$min_ios
cargo build --release --target $target -p malasdoh-ios

app=target/ios/$kind/MalasDoh.app
rm -rf "$app"
mkdir -p "$app"
cp "target/$target/release/MalasDoh" "$app/"
cp "$here"/icons/*.png "$app/"
sed -e "s/@VERSION@/$version/g" -e "s/@PLATFORM@/$platform/" -e "s/@MIN_IOS@/$min_ios/" \
    -e "s/@BUNDLE_ID@/$bundle_id/" "$here/Info.plist" > "$app/Info.plist"
plutil -lint "$app/Info.plist" >/dev/null
printf 'APPL????' > "$app/PkgInfo"

if [ $kind = device ]; then
    cp "$MALASDOH_IOS_PROFILE" "$app/embedded.mobileprovision"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    security cms -D -i "$MALASDOH_IOS_PROFILE" > "$tmp/profile.plist"
    /usr/libexec/PlistBuddy -x -c 'Print :Entitlements' "$tmp/profile.plist" > "$tmp/entitlements.plist"
    codesign --force --sign "$MALASDOH_IOS_IDENTITY" --entitlements "$tmp/entitlements.plist" "$app"
    echo "Built $app. Install it with: xcrun devicectl device install app --device <device> $app"
else
    # The Simulator on Apple silicon runs only signed code; an ad-hoc
    # signature is enough.
    codesign --force --sign - "$app"
    echo "Built $app. Run it with:"
    echo "  open -a Simulator    # then start an iPhone or iPad in it"
    echo "  xcrun simctl install booted $app && xcrun simctl launch booted $bundle_id"
fi
