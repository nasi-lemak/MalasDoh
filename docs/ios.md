# MalasDoh on iPhone and iPad

MalasDoh runs on iOS and iPadOS 15 and newer. It is the same app as on the
desktop, drawn with Metal through wgpu; it keeps clear of the notch, the
home indicator and the on-screen keyboard, and the keyboard comes up when
you tap a text field or the terminal.

**Status: early.** It has run in the iPad Simulator but not yet on a real
device.

On an iPhone it uses its phone layout: the connection list fills the screen
until you pick one, and inside a connection Terminals, Files and This
device take turns. Taps open things, the terminal has a row of the keys an
iPhone keyboard lacks (Esc, Tab, Ctrl, arrows, Paste…), and a text field
being typed in has a Paste button under it.

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

- `crates/malasdoh-ios`: the app's program: eframe's iOS window, the app,
  and the glue: the keyboard's frame, the clipboard (UIPasteboard) and the
  document picker.
- `ios/Info.plist` and `ios/icons/`: the bundle's description and icons.
  `ios/build-app.sh` builds the program and assembles and signs
  `MalasDoh.app` around it.
- Passwords you ask it to remember are kept in the iOS Keychain.
- Files you pick are copied into the app; downloads and saved files go to
  its Documents folder, which the Files app shows under "On My iPhone".

CI builds the app for devices and for the Simulator, and starts it in an
iPad Simulator on every pull request (`.github/workflows/mobile.yml`).

## Not there yet

- **App Store packaging** (an `.ipa` and its review): later.
- Not yet tried on a real iPhone or iPad.
