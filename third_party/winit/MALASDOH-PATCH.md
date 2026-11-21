# winit 0.30.13, patched for MalasDoh

This is winit 0.30.13 from crates.io (Apache-2.0, see `LICENSE`), used in
place of it through `[patch.crates-io]` in the workspace's `Cargo.toml`.

One change, in `src/platform_impl/android/mod.rs`: winit 0.30 never reports
modifier keys on Android, so with a hardware keyboard Ctrl+C typed "c" and
every Ctrl, Alt and Shift shortcut was lost. Each key event's meta state now
becomes a `WindowEvent::ModifiersChanged` before the key, as on the other
platforms. Every other platform's code is unchanged.

Drop this copy once a winit release that eframe uses reports them itself.
