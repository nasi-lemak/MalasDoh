//! MalasDoh on iOS and iPadOS: the same app as on the desktop, in eframe's
//! iOS window. egui keeps clear of the notch and the home indicator, winit
//! shows the on-screen keyboard while a text field has focus, and this file
//! keeps the app clear of that keyboard and connects the clipboard.

#[cfg(target_os = "ios")]
mod ios;

#[cfg(target_os = "ios")]
fn main() {
    ios::main();
}

#[cfg(not(target_os = "ios"))]
fn main() {
    eprintln!("This is MalasDoh's iOS program; build it on a Mac with ios/build-app.sh.");
}
