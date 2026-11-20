# MalasDoh on iPhone and iPad

MalasDoh runs on iOS and iPadOS 15 and newer. It is the same app as on the
desktop, drawn with Metal through wgpu; it keeps clear of the notch and the
home indicator, and the on-screen keyboard comes up when you tap a text
field.

**Status: early.** The layout is still the desktop one, so it is at home on
an iPad, best with a keyboard and trackpad.

Building needs a Mac with Xcode, and the Rust targets:

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
```

## In the Simulator

```sh
ios/build-app.sh
open -a Simulator          # start an iPhone or iPad in it
xcrun simctl install booted target/ios/simulator/MalasDoh.app
xcrun simctl launch booted io.github.nasi-lemak.malasdoh
```

## On your iPhone or iPad

Apple only lets signed apps onto a device. You need an Apple developer
account (the free one works, with apps that expire after 7 days; the paid
one, for the App Store and TestFlight, is 99 USD a year).

1. In Xcode, Settings → Accounts, add your Apple ID. That gives you a
   signing identity; `security find-identity -v -p codesigning` shows its
   name.
2. Make a development provisioning profile for the bundle ID
   `io.github.nasi-lemak.malasdoh` (or your own) that includes your device:
   at developer.apple.com, or by letting Xcode make one for an empty app with
   that bundle ID.
3. Build, sign and install:

   ```sh
   MALASDOH_IOS_IDENTITY="Apple Development: Your Name (TEAMID)" \
   MALASDOH_IOS_PROFILE=~/Downloads/MalasDoh.mobileprovision \
   ios/build-app.sh --device
   xcrun devicectl device install app --device <name or id> target/ios/device/MalasDoh.app
   ```

   With your own bundle ID, also set `MALASDOH_IOS_BUNDLE_ID`.

## How it fits together

- `crates/malasdoh-ios`: the app's program: eframe's iOS window and the app.
- `ios/Info.plist` and `ios/icons/`: the bundle's description and icons.
  `ios/build-app.sh` builds the program and assembles and signs
  `MalasDoh.app` around it.
- Passwords you ask it to remember are kept in the iOS Keychain.

CI builds the app for devices and for the Simulator, and starts it in an
iPad Simulator on every pull request (`.github/workflows/mobile.yml`).

## Not there yet

- **A phone layout.** On an iPhone the panels are cramped.
- **The keyboard can cover** the bottom of the window while it is up.
- **File pickers.** The "Browse…" buttons do nothing yet; type the path.
- **Clipboard** between MalasDoh and other apps.
- **App Store packaging** (an `.ipa` and its review): later.
