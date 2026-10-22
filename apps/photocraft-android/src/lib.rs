//! PhotoCraft for Android (`libphotocraft_android.so`).
//!
//! Runs the full [`photocraft_ui_egui::PhotocraftApp`] on Android with:
//! - **Hardware GPU Acceleration** ([`gpu_startup`]): `wgpu` Vulkan 1.1+ primary backend with
//!   OpenGL ES 3.0+ fallback, unlocking hardware adapter texture limits for [`photocraft_ui_egui::gpu_canvas::GpuCanvas`]
//!   and the `photocraft-gpu` layer stack compositor.
//! - **File Permissions & Storage Access for Import/Export** ([`permissions`], [`file_dialog`],
//!   [`services`]): Runtime storage permission management (`READ_EXTERNAL_STORAGE`,
//!   `WRITE_EXTERNAL_STORAGE`, `READ_MEDIA_IMAGES`, `MANAGE_EXTERNAL_STORAGE`), Android Storage
//!   Access Framework (SAF) system file/save picker bridge, touch-friendly in-app file browser,
//!   automatic scoped-storage fallback to `Download` / `Exports`, and `MediaScannerConnection`
//!   indexing.
//! - **Touch & Stylus Input** ([`shell`]): Android touch-twin deduplication and S-Pen / stylus
//!   pressure, tilt, and orientation axes.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

#[cfg(target_os = "android")]
mod android_entry;
pub mod file_dialog;
pub mod gpu_startup;
pub mod permissions;
pub mod services;
pub mod shell;

// On Windows GNU, a `cdylib` with zero exported symbols causes `ld.exe` to enable
// `--export-all-symbols`, exceeding the 65,535 PE ordinal limit. Exporting one symbol on host
// targets keeps the host `cdylib` export table minimal during `cargo test`.
#[cfg(not(target_os = "android"))]
#[allow(unsafe_code)]
// SAFETY: Unmangled C-ABI stub with a crate-prefixed name so host `cdylib` links cleanly.
#[unsafe(no_mangle)]
pub extern "C" fn photocraft_android_abi_version() -> u32 {
    1
}
