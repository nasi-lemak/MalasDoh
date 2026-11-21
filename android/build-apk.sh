#!/bin/sh
# Builds the Android app: target/android/malasdoh-<version>-android.apk
#
#   ANDROID_HOME=/path/to/Android/Sdk android/build-apk.sh
#
# Needs, in the SDK: build-tools 35 or newer, a platform (android-35 or
# newer) and the NDK (ANDROID_NDK_HOME, else the newest under ndk/); a JDK
# (javac, keytool); and the Rust targets, once:
#   rustup target add aarch64-linux-android x86_64-linux-android
# DEBUGGABLE=1 marks the APK debuggable (CI's tests use it to seed data).
# ABIS="arm64-v8a" builds for phones only (default: arm64-v8a x86_64, the
# second for the emulator and Chromebooks).
#
# Signing: MALASDOH_KEYSTORE, MALASDOH_KEYSTORE_PASS and MALASDOH_KEY_ALIAS
# name a release key. Without them the APK is signed with the usual debug
# key (~/.android/debug.keystore, made if missing): fine for trying it, not
# for a store. Runs on Linux and macOS; on Windows, in WSL.
set -eu

: "${ANDROID_HOME:?set ANDROID_HOME to the Android SDK}"
newest() { ls -d "$1"/* 2>/dev/null | sort -V | tail -n 1; }
tools=$(newest "$ANDROID_HOME/build-tools")
platform=$(newest "$ANDROID_HOME/platforms")
ndk=${ANDROID_NDK_HOME:-$(newest "$ANDROID_HOME/ndk")}
[ -x "$tools/aapt2" ] || { echo "no build-tools in $ANDROID_HOME" >&2; exit 1; }
[ -f "$platform/android.jar" ] || { echo "no platform in $ANDROID_HOME" >&2; exit 1; }
case $(uname -s) in Darwin) host=darwin-x86_64 ;; *) host=linux-x86_64 ;; esac
llvm="$ndk/toolchains/llvm/prebuilt/$host/bin"
[ -d "$llvm" ] || { echo "no NDK (set ANDROID_NDK_HOME)" >&2; exit 1; }

min_sdk=26
target_sdk=35
abis=${ABIS:-arm64-v8a x86_64}

here=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$here")
cd "$root"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
code=$(echo "$version" | awk -F. '{ print $1 * 1000000 + $2 * 1000 + $3 }')

work=target/android/apk
rm -rf "$work"
mkdir -p "$work"

for abi in $abis; do
    case $abi in
        arm64-v8a) triple=aarch64-linux-android ;;
        x86_64) triple=x86_64-linux-android ;;
        *) echo "unknown ABI $abi" >&2; exit 1 ;;
    esac
    t=$(echo "$triple" | tr 'a-z-' 'A-Z_')
    u=$(echo "$triple" | tr '-' '_')
    # The NDK's clang for the oldest Android the app runs on.
    export "CC_$u=$llvm/$triple$min_sdk-clang"
    export "CXX_$u=$llvm/$triple$min_sdk-clang++"
    export "AR_$u=$llvm/llvm-ar"
    export "CARGO_TARGET_${t}_LINKER=$llvm/$triple$min_sdk-clang"
    # 16 KB pages, which Android 15 devices may use (and Google Play asks for).
    export "CARGO_TARGET_${t}_RUSTFLAGS=-C link-arg=-Wl,-z,max-page-size=16384"
    cargo build --release --target "$triple" -p malasdoh-android
    mkdir -p "$work/lib/$abi"
    cp "target/$triple/release/libmalasdoh_android.so" "$work/lib/$abi/"
done

"$tools/aapt2" compile --dir "$here/res" -o "$work/res.zip"
"$tools/aapt2" link -o "$work/unaligned.apk" -I "$platform/android.jar" \
    --manifest "$here/AndroidManifest.xml" "$work/res.zip" \
    --min-sdk-version "$min_sdk" --target-sdk-version "$target_sdk" \
    --version-code "$code" --version-name "$version" ${DEBUGGABLE:+--debug-mode}
# The Java part (MainActivity: the keyboard and the clipboard), compiled
# against the platform, then to Android's bytecode.
mkdir -p "$work/classes"
javac --release 11 -nowarn -cp "$platform/android.jar" -d "$work/classes" $(find "$here/java" -name '*.java')
"$tools/d8" --release --min-api "$min_sdk" --lib "$platform/android.jar" --output "$work" \
    $(find "$work/classes" -name '*.class')
(cd "$work" && zip -q unaligned.apk classes.dex)
# Native libraries go in uncompressed, so Android maps them straight from
# the APK (extractNativeLibs="false").
(cd "$work" && zip -q -0 -r unaligned.apk lib)
"$tools/zipalign" -P 16 -f 4 "$work/unaligned.apk" "$work/aligned.apk"

if [ -n "${MALASDOH_KEYSTORE:-}" ]; then
    ks=$MALASDOH_KEYSTORE
    pass=${MALASDOH_KEYSTORE_PASS:?set MALASDOH_KEYSTORE_PASS}
    alias=${MALASDOH_KEY_ALIAS:?set MALASDOH_KEY_ALIAS}
else
    ks=$HOME/.android/debug.keystore
    pass=android
    alias=androiddebugkey
    if [ ! -f "$ks" ]; then
        mkdir -p "$(dirname "$ks")"
        keytool -genkeypair -keystore "$ks" -storepass android -keypass android -alias androiddebugkey \
            -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US"
    fi
fi
out="target/android/malasdoh-$version-android.apk"
# The password goes through the environment, not the command line.
MALASDOH_SIGN_PASS=$pass "$tools/apksigner" sign --ks "$ks" --ks-key-alias "$alias" \
    --ks-pass env:MALASDOH_SIGN_PASS --key-pass env:MALASDOH_SIGN_PASS --out "$out" "$work/aligned.apk"
"$tools/apksigner" verify "$out"
echo "Built $out"
