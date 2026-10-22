# PhotoCraft for Android

This directory contains the Android application project (`ai.storyteller.photocraft`) that packages the Rust `photocraft-android` crate (`libphotocraft_android.so`) into an Android APK/AAB.

## Key Features

### 1. Hardware GPU Acceleration (`wgpu` Vulkan 1.1+ & OpenGL ES 3.0+)
- **Vulkan & OpenGL ES Backends**: `apps/photocraft-android/src/gpu_startup.rs` configures `wgpu` with `wgpu::Backends::VULKAN | wgpu::Backends::GL` and `PowerPreference::HighPerformance`, preferring hardware Vulkan first (Adreno, Mali, Immortalis, Xclipse) with automatic fallback to OpenGL ES 3.0+.
- **Full Hardware Texture Limits**: Calls `photocraft_ui_egui::gpu_canvas::use_adapter_limits_with` to unlock the device GPU's full `max_texture_dimension_2d` (8192–16384 px) for both `GpuCanvas` (tiled mipmap pyramid & colour-managed display shader) and `photocraft-gpu` (GPU layer compositor).
- **Android Physical RAM Detection**: `crates/ui-egui/src/gpu_canvas.rs` reads `/proc/meminfo` on `target_os = "android"` so GPU tile cache and document memory budgets scale accurately to the device's physical RAM.
- **Manifest Hardware Acceleration**: `AndroidManifest.xml` enables `android:hardwareAccelerated="true"`, `android:largeHeap="true"`, OpenGL ES 3.0+, and Vulkan hardware feature declarations.

### 2. File Permissions for Import & Export
- **Manifest & Runtime Permissions**:
  - `android.permission.READ_EXTERNAL_STORAGE` (Android 8–12L)
  - `android.permission.WRITE_EXTERNAL_STORAGE` (Android 8–10)
  - `android.permission.READ_MEDIA_IMAGES` (Android 13+)
  - `android.permission.MANAGE_EXTERNAL_STORAGE` (Android 11+ All Files Access for `.pcraft`, `.psd`, `.psb`, linked smart objects, and brush libraries)
- **Dual File Picker (In-App Touch Browser + Android SAF)**:
  - **In-App Android File Browser (`apps/photocraft-android/src/file_dialog.rs`)**: Quick bookmarks for `Downloads`, `Pictures`, `Documents`, `Shared Storage`, `App External`, `App Exports`, and `App Internal`, plus one-tap **Grant Storage Access**, **All Files Access**, and **System File Picker (SAF)** buttons.
  - **Storage Access Framework (`MainActivity.kt`)**: Bridges `ACTION_OPEN_DOCUMENT`, `ACTION_CREATE_DOCUMENT`, `ACTION_OPEN_DOCUMENT_TREE`, and incoming `ACTION_VIEW` / `ACTION_EDIT` / `ACTION_SEND` intents over JNI to Rust.
  - **Scoped-Storage Safe Exports & Gallery Indexing**: `services::write_with_android_permissions` writes atomically to the requested path (falling back automatically to writable `Download` or `App Exports` if scoped storage restricts a directory) and triggers `MediaScannerConnection.scanFile` so exported images immediately appear in the Android Gallery.

## Building

```bash
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk --locked
packaging/android/package.sh
```
