#!/bin/sh
# Builds the Rust native module and puts it where DevEco Studio packs it:
# ohos/entry/libs/arm64-v8a/libmalasdoh_ohos.so
#
#   OHOS_NDK_HOME=/path/to/sdk/default/openharmony ohos/build-native.sh [--debug]
#
# OHOS_NDK_HOME is the SDK folder that holds native/ (in DevEco Studio's SDK:
# sdk/default/openharmony). Needs Rust's HarmonyOS target once:
#   rustup target add aarch64-unknown-linux-ohos
# Runs on Linux and macOS; on Windows, run it in WSL with the Linux SDK.
set -eu

: "${OHOS_NDK_HOME:?set OHOS_NDK_HOME to the SDK folder that holds native/}"
native="$OHOS_NDK_HOME/native"
[ -x "$native/llvm/bin/clang" ] || { echo "no $native/llvm/bin/clang: is OHOS_NDK_HOME right?" >&2; exit 1; }

profile=release
flag=--release
if [ "${1:-}" = "--debug" ]; then profile=debug; flag=; fi

here=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$here")

# Cargo calls the C compiler and linker by one path, so give clang its
# target and sysroot through small wrappers.
wrap=$(mktemp -d)
trap 'rm -rf "$wrap"' EXIT
for t in clang clang++; do
    cat > "$wrap/$t" <<SH
#!/bin/sh
exec "$native/llvm/bin/$t" --target=aarch64-linux-ohos --sysroot="$native/sysroot" -D__MUSL__ "\$@"
SH
    chmod +x "$wrap/$t"
done

export OHOS_NDK_HOME
export CC_aarch64_unknown_linux_ohos="$wrap/clang"
export CXX_aarch64_unknown_linux_ohos="$wrap/clang++"
export AR_aarch64_unknown_linux_ohos="$native/llvm/bin/llvm-ar"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_OHOS_LINKER="$wrap/clang"

cd "$root"
# shellcheck disable=SC2086 # $flag is empty or one word
cargo build $flag --target aarch64-unknown-linux-ohos -p malasdoh-ohos

mkdir -p "$here/entry/libs/arm64-v8a"
cp "target/aarch64-unknown-linux-ohos/$profile/libmalasdoh_ohos.so" "$here/entry/libs/arm64-v8a/"
echo "Built ohos/entry/libs/arm64-v8a/libmalasdoh_ohos.so ($profile). Now open ohos/ in DevEco Studio."
