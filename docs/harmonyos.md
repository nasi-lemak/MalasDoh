# MalasDoh on HarmonyOS

MalasDoh can be built as a HarmonyOS app for **PCs and tablets** (HarmonyOS
5 and newer, HarmonyOS 7 included). It is the same app as on the desktop:
the same Rust code draws the same window, with HarmonyOS giving it the
surface, touch, mouse, keyboard and on-screen keyboard.

**Status: early.** It builds, and the native library links only against
HarmonyOS system libraries, but it has not yet been run on a device or the
emulator. Expect the first run to need fixes.

## How it fits together

- `crates/malasdoh-ohos`: the native module, `libmalasdoh_ohos.so`. It
  uses [openharmony-ability](https://github.com/harmony-contrib/openharmony-ability)
  for the lifecycle and input, EGL and OpenGL ES 3 to draw, and the
  `malasdoh` app library (built without its desktop parts) for everything
  else.
- `ohos/`: the DevEco Studio project. Its `EntryAbility` extends the
  `NativeAbility` of `@ohos-rs/ability`, which loads the module.
  `ohos/build-native.sh` builds the module into `ohos/entry/libs/arm64-v8a/`,
  where DevEco Studio packs it into the app.

## What you need

- [DevEco Studio](https://developer.huawei.com/consumer/en/deveco-studio/)
  with its HarmonyOS SDK, and a Huawei developer account (to sign the app
  and run it on a device).
- Rust 1.89 or newer and the HarmonyOS target:
  `rustup target add aarch64-unknown-linux-ohos`
- Linux or macOS to build the native module. On Windows, use WSL with the
  Linux command-line tools SDK (DevEco's Windows SDK has Windows clang, which
  the script does not drive).

## Build and run

1. Build the native module. `OHOS_NDK_HOME` is the SDK folder that holds
   `native/` (in DevEco Studio's SDK: `sdk/default/openharmony`):

   ```sh
   OHOS_NDK_HOME=/path/to/sdk/default/openharmony ohos/build-native.sh
   ```

   `--debug` builds a debug library instead. Run it again whenever the Rust
   code changes.

2. Open the `ohos/` folder in DevEco Studio. Let it sync: it downloads
   `@ohos-rs/ability` from ohpm. Its version must stay the same as the
   `openharmony-ability` crate in `crates/malasdoh-ohos/Cargo.toml`.
3. File → Project Structure → Signing Configs → "Automatically generate
   signature" (sign in with your Huawei ID). This writes your signing
   details into `ohos/build-profile.json5`; do not commit them.
4. If DevEco Studio offers to upgrade `targetSdkVersion` or the project's
   `modelVersion` to its own SDK, accept.
5. Pick a HarmonyOS PC or tablet, or the emulator, and press Run.

Logs: `hdc hilog | grep -i malasdoh`, or the Log pane in DevEco Studio.

## Not there yet

- **Saved passwords and passphrases.** There is no keychain to keep them in
  yet (HarmonyOS's Asset Store is the plan), so you type them each time.
- **File pickers.** The "Browse…" buttons do nothing yet; type the path.
- **Clipboard** between MalasDoh and other apps.
- **Phones.** The layout needs a wide screen and a keyboard, so the app is
  listed for PCs and tablets only.
- **Scrolling direction** with a mouse wheel or touchpad is a guess until
  it is tried on a device.
- Files you open live in the app's own sandbox; "Local files" sees only that
  sandbox.
