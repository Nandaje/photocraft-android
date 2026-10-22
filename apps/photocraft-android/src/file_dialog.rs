//! Touch-friendly Android file/folder/save dialog with Storage Access Framework (SAF) and runtime
//! storage permission integration.
//!
//! When [`photocraft_ui_egui::PhotocraftApp`] requests an Open, Save, or Folder dialog via
//! [`photocraft_ui_egui::Services::file_dialog`], this module opens an interactive modal dialog
//! that supports both:
//! 1. **Direct Android storage browsing** (`Downloads`, `Pictures`, `Documents`, `App External`,
//!    `App Internal`) when storage permissions are granted.
//! 2. **Android Storage Access Framework (SAF)** system picker (`ACTION_OPEN_DOCUMENT`,
//!    `ACTION_CREATE_DOCUMENT`, `ACTION_OPEN_DOCUMENT_TREE`) and runtime permission requests via
//!    JNI hooks.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use photocraft_ui_egui::{FileDialogAnswer, FileDialogReply, FileDialogRequest};

use crate::permissions::{self, AndroidDirs};

/// All file extensions supported by PhotoCraft File › Open.
pub const OPEN_EXTS: &[&str] = &[
    "pcraft", "pdn", "ora", "xcf", "xcfgz", "gz", "kra", "krz", "psd", "psb", "psdt", "png", "jpg", "jpeg", "tif", "tiff", "webp", "gif", "bmp", "tga", "ico",
    "qoi", "exr", "hdr", "pbm", "pgm", "ppm", "pam", "pfm", "pcx", "dds", "xpm", "fits", "fit", "fts", "dpx", "cin", "heic", "heif", "hif", "dng", "cr2",
    "cr3", "nef", "nrw", "arw", "pef", "orf", "rw2", "raf", "abr", "grd", "svg", "svgz", "aco", "act", "ase", "kys", "af", "afdesign", "afphoto", "afpub",
];

/// Save As / Export formats offered in the Android Save dialog.
pub const SAVE_FORMATS: &[(&str, &str)] = &[
    ("PSD Document (.psd)", "psd"),
    ("Large Document Format (.psb)", "psb"),
    ("PhotoCraft (.pcraft)", "pcraft"),
    ("PNG (.png)", "png"),
    ("JPEG (.jpg)", "jpg"),
    ("WebP (.webp)", "webp"),
    ("TIFF (.tif)", "tif"),
    ("OpenRaster (.ora)", "ora"),
    ("PDF (.pdf)", "pdf"),
    ("BMP (.bmp)", "bmp"),
    ("GIF (.gif)", "gif"),
    ("Targa (.tga)", "tga"),
    ("ICO (.ico)", "ico"),
    ("OpenEXR (.exr)", "exr"),
    ("DDS (.dds)", "dds"),
];

/// Optional platform bridge callbacks registered by `android_entry` when running inside the
/// Android JNI runtime.
pub struct PlatformBridge {
    /// Request runtime storage permissions (`READ_MEDIA_IMAGES` / `READ_EXTERNAL_STORAGE` /
    /// `WRITE_EXTERNAL_STORAGE` / `MANAGE_EXTERNAL_STORAGE`).
    pub request_permissions: Box<dyn Fn(bool) + Send + Sync>,
    /// Launch Android SAF `ACTION_OPEN_DOCUMENT` picker (`multiple`).
    pub launch_saf_open: Box<dyn Fn(bool) + Send + Sync>,
    /// Launch Android SAF `ACTION_CREATE_DOCUMENT` picker (`suggested_name`).
    pub launch_saf_save: Box<dyn Fn(&str) + Send + Sync>,
    /// Launch Android SAF `ACTION_OPEN_DOCUMENT_TREE` folder picker.
    pub launch_saf_folder: Box<dyn Fn() + Send + Sync>,
    /// Notify Android `MediaScannerConnection` so an exported file appears in Gallery/Photos.
    pub scan_media_file: Box<dyn Fn(&str) + Send + Sync>,
}

static BRIDGE: OnceLock<Mutex<Option<PlatformBridge>>> = OnceLock::new();

fn bridge_lock() -> &'static Mutex<Option<PlatformBridge>> {
    BRIDGE.get_or_init(|| Mutex::new(None))
}

/// Register the Android JNI bridge callbacks.
pub fn set_platform_bridge(bridge: PlatformBridge) {
    *bridge_lock().lock().unwrap_or_else(PoisonError::into_inner) = Some(bridge);
}

/// Ask Android to request runtime storage permissions (`all_files`: also open the
/// `MANAGE_EXTERNAL_STORAGE` settings page on Android 11+).
pub fn request_storage_permissions(all_files: bool) {
    if let Some(b) = bridge_lock().lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        (b.request_permissions)(all_files);
    }
    permissions::refresh_permission_probe();
}

/// Notify Android MediaScanner that a file was exported so it appears in the system Gallery.
pub fn notify_media_scanner(path: &str) {
    if let Some(b) = bridge_lock().lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        (b.scan_media_file)(path);
    }
}

/// Active file dialog state shared between the `Services::file_dialog` callback, JNI SAF callbacks,
/// and the `AndroidShell` UI.
struct ActiveDialog {
    request: FileDialogRequest,
    reply: FileDialogReply,
    current_dir: PathBuf,
    entries: Vec<DirEntryItem>,
    selected_paths: Vec<PathBuf>,
    file_name_input: String,
    selected_format_idx: usize,
    new_folder_name: String,
    show_new_folder: bool,
    error_message: Option<String>,
}

#[derive(Clone, Debug)]
struct DirEntryItem {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
}

static ACTIVE_DIALOG: OnceLock<Mutex<Option<ActiveDialog>>> = OnceLock::new();

fn dialog_lock() -> &'static Mutex<Option<ActiveDialog>> {
    ACTIVE_DIALOG.get_or_init(|| Mutex::new(None))
}

/// `Services::file_dialog` implementation for Android.
pub fn show_file_dialog(request: FileDialogRequest, _parent: Option<&eframe::Frame>, reply: FileDialogReply) {
    let dirs = permissions::current_dirs();
    permissions::refresh_permission_probe();

    let (current_dir, file_name_input, selected_format_idx) = match &request {
        FileDialogRequest::Open { initial_dir, .. } | FileDialogRequest::Folder { initial_dir } => {
            let dir = initial_dir
                .as_deref()
                .map(PathBuf::from)
                .filter(|d| d.is_dir() && permissions::is_dir_readable(d))
                .unwrap_or_else(|| dirs.default_open_dir());
            (dir, String::new(), 0)
        }
        FileDialogRequest::Save { suggested } => {
            let path = Path::new(suggested);
            let dir = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty() && p.is_dir() && permissions::is_dir_writable(p))
                .map(Path::to_path_buf)
                .unwrap_or_else(|| dirs.default_save_dir());
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled-1.psd".into());
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_else(|| "psd".into());
            let fmt_idx = SAVE_FORMATS.iter().position(|(_, e)| *e == ext).unwrap_or(0);
            (dir, name, fmt_idx)
        }
    };

    let mut active = ActiveDialog {
        request,
        reply,
        current_dir,
        entries: Vec::new(),
        selected_paths: Vec::new(),
        file_name_input,
        selected_format_idx,
        new_folder_name: String::new(),
        show_new_folder: false,
        error_message: None,
    };
    refresh_entries(&mut active);
    *dialog_lock().lock().unwrap_or_else(PoisonError::into_inner) = Some(active);
}

/// Complete the active open dialog with file contents returned from the Android SAF picker.
pub fn deliver_saf_open_contents(files: Vec<(String, Vec<u8>)>) {
    let Some(active) = dialog_lock().lock().unwrap_or_else(PoisonError::into_inner).take() else {
        return;
    };
    let answer = match files.len() {
        0 => None,
        1 => files.into_iter().next().map(|(name, bytes)| FileDialogAnswer::Contents(name, bytes)),
        _ => Some(FileDialogAnswer::ContentsMany(files)),
    };
    active.reply.send(answer);
}

/// Complete the active save dialog with a path returned from the Android SAF picker.
pub fn deliver_saf_save_path(path: String) {
    let Some(active) = dialog_lock().lock().unwrap_or_else(PoisonError::into_inner).take() else {
        return;
    };
    let answer = (!path.trim().is_empty()).then_some(FileDialogAnswer::SaveTo(path));
    active.reply.send(answer);
}

/// Complete the active folder dialog with a directory path returned from Android SAF.
pub fn deliver_saf_folder_path(path: String) {
    let Some(active) = dialog_lock().lock().unwrap_or_else(PoisonError::into_inner).take() else {
        return;
    };
    let answer = (!path.trim().is_empty()).then_some(FileDialogAnswer::FolderTo(path));
    active.reply.send(answer);
}

/// Apply the selected format extension to `name` when saving.
pub fn apply_save_extension(name: &str, ext: &str) -> String {
    let trimmed = name.trim();
    let base = if trimmed.is_empty() { "Untitled-1" } else { trimmed };
    let p = Path::new(base);
    match p.extension().and_then(|e| e.to_str()) {
        Some(existing) if existing.eq_ignore_ascii_case(ext) => base.to_string(),
        Some(_) => p.with_extension(ext).to_string_lossy().into_owned(),
        None => format!("{base}.{ext}"),
    }
}

fn matches_extension_filter(path: &Path, allowed: Option<&[String]>) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let lower = ext.to_ascii_lowercase();
    match allowed {
        Some(list) if !list.is_empty() => list.iter().any(|a| a.eq_ignore_ascii_case(&lower)),
        _ => OPEN_EXTS.iter().any(|a| *a == lower),
    }
}

fn refresh_entries(active: &mut ActiveDialog) {
    active.entries.clear();
    active.error_message = None;
    let rd = match std::fs::read_dir(&active.current_dir) {
        Ok(rd) => rd,
        Err(e) => {
            active.error_message = Some(format!(
                "Cannot read {}: {e}. Tap 'Grant Storage Access' or 'System File Picker (SAF)' above, or select an App Storage bookmark.",
                active.current_dir.display()
            ));
            return;
        }
    };
    let filter_exts = match &active.request {
        FileDialogRequest::Open { extensions, .. } => extensions.as_deref(),
        _ => None,
    };
    let folder_only = matches!(active.request, FileDialogRequest::Folder { .. });
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir) || path.is_dir();
        if !is_dir {
            if folder_only {
                continue;
            }
            if matches!(active.request, FileDialogRequest::Open { .. }) && !matches_extension_filter(&path, filter_exts) {
                continue;
            }
        }
        let size = meta.as_ref().map_or(0, std::fs::Metadata::len);
        active.entries.push(DirEntryItem { name, path, is_dir, size });
    }
    active.entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
    });
}

/// Render the touch-friendly Android File Dialog modal if one is currently open.
pub fn render_modal(ctx: &egui::Context) {
    let mut guard = dialog_lock().lock().unwrap_or_else(PoisonError::into_inner);
    let Some(active) = guard.as_mut() else {
        return;
    };

    let dirs: AndroidDirs = permissions::current_dirs();
    let perm_status = permissions::current_status();
    let title = match &active.request {
        FileDialogRequest::Open { multiple: true, .. } => "Import / Open Files (Android)",
        FileDialogRequest::Open { multiple: false, .. } => "Import / Select File (Android)",
        FileDialogRequest::Folder { .. } => "Select Folder (Android)",
        FileDialogRequest::Save { .. } => "Export / Save File (Android)",
    };

    let mut finish_answer: Option<Option<FileDialogAnswer>> = None;

    egui::Window::new(title)
        .id(egui::Id::new("android_file_dialog_modal"))
        .collapsible(false)
        .resizable(true)
        .default_size([720.0, 480.0])
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            // Top bar: Storage Permission status + Grant Permission & SAF System Picker buttons
            ui.horizontal_wrapped(|ui| {
                ui.label(perm_status.summary());
                if ui.button("Grant Storage Access").clicked() {
                    request_storage_permissions(false);
                    refresh_entries(active);
                }
                if perm_status.sdk_int >= 30 && !perm_status.all_files_access && ui.button("All Files Access").clicked() {
                    request_storage_permissions(true);
                    refresh_entries(active);
                }
                let has_bridge = bridge_lock().lock().unwrap_or_else(PoisonError::into_inner).is_some();
                if has_bridge && ui.button("System File Picker (SAF)").clicked() {
                    if let Some(b) = bridge_lock().lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                        match &active.request {
                            FileDialogRequest::Open { multiple, .. } => (b.launch_saf_open)(*multiple),
                            FileDialogRequest::Save { .. } => (b.launch_saf_save)(&active.file_name_input),
                            FileDialogRequest::Folder { .. } => (b.launch_saf_folder)(),
                        }
                    }
                }
            });

            ui.separator();

            // Quick bookmarks bar
            ui.horizontal_wrapped(|ui| {
                ui.strong("Locations:");
                for (label, target_dir) in dirs.bookmarks() {
                    let selected = active.current_dir == target_dir;
                    if ui.selectable_label(selected, label).clicked() {
                        let _ = std::fs::create_dir_all(&target_dir);
                        active.current_dir = target_dir;
                        active.selected_paths.clear();
                        refresh_entries(active);
                    }
                }
                if let Some(parent) = active.current_dir.parent().map(Path::to_path_buf)
                    && ui.button("⬆ Up").clicked()
                {
                    active.current_dir = parent;
                    active.selected_paths.clear();
                    refresh_entries(active);
                }
                if ui.button("⟳ Refresh").clicked() {
                    permissions::refresh_permission_probe();
                    refresh_entries(active);
                }
                if ui.button("+ New Folder").clicked() {
                    active.show_new_folder = !active.show_new_folder;
                }
            });

            if active.show_new_folder {
                ui.horizontal(|ui| {
                    ui.label("Folder name:");
                    ui.text_edit_singleline(&mut active.new_folder_name);
                    if ui.button("Create").clicked() && !active.new_folder_name.trim().is_empty() {
                        let new_dir = active.current_dir.join(active.new_folder_name.trim());
                        match std::fs::create_dir_all(&new_dir) {
                            Ok(()) => {
                                active.new_folder_name.clear();
                                active.show_new_folder = false;
                                active.current_dir = new_dir;
                                refresh_entries(active);
                            }
                            Err(e) => active.error_message = Some(format!("Could not create folder: {e}")),
                        }
                    }
                });
            }

            ui.label(format!("Current folder: {}", active.current_dir.display()));

            if let Some(err) = &active.error_message {
                ui.colored_label(egui::Color32::from_rgb(240, 140, 100), err);
            }

            ui.separator();

            // File list
            let mut navigate_to: Option<PathBuf> = None;
            let mut double_clicked_file: Option<PathBuf> = None;
            let allow_multiple = matches!(active.request, FileDialogRequest::Open { multiple: true, .. });

            egui::ScrollArea::vertical()
                .max_height(260.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if active.entries.is_empty() && active.error_message.is_none() {
                        ui.label("No matching files in this folder.");
                    }
                    for entry in &active.entries {
                        let is_selected = active.selected_paths.contains(&entry.path);
                        let label = if entry.is_dir {
                            format!("📁 {}", entry.name)
                        } else {
                            format!("📄 {} ({:.1} KB)", entry.name, entry.size as f64 / 1024.0)
                        };
                        let resp = ui.selectable_label(is_selected, label);
                        if resp.clicked() {
                            if entry.is_dir {
                                navigate_to = Some(entry.path.clone());
                            } else {
                                if matches!(active.request, FileDialogRequest::Save { .. }) {
                                    active.file_name_input = entry.name.clone();
                                }
                                if allow_multiple {
                                    if is_selected {
                                        active.selected_paths.retain(|p| p != &entry.path);
                                    } else {
                                        active.selected_paths.push(entry.path.clone());
                                    }
                                } else {
                                    active.selected_paths.clear();
                                    active.selected_paths.push(entry.path.clone());
                                }
                            }
                        }
                        if resp.double_clicked() && !entry.is_dir {
                            double_clicked_file = Some(entry.path.clone());
                        }
                    }
                });

            if let Some(dir) = navigate_to {
                active.current_dir = dir;
                active.selected_paths.clear();
                refresh_entries(active);
            }
            if let Some(file) = double_clicked_file
                && matches!(active.request, FileDialogRequest::Open { .. })
            {
                finish_answer = Some(Some(FileDialogAnswer::Paths(vec![file.to_string_lossy().into_owned()])));
            }

            ui.separator();

            // Save filename & format selector
            if matches!(active.request, FileDialogRequest::Save { .. }) {
                ui.horizontal(|ui| {
                    ui.label("File name:");
                    ui.text_edit_singleline(&mut active.file_name_input);
                });
                ui.horizontal(|ui| {
                    ui.label("Format:");
                    let current_fmt = SAVE_FORMATS.get(active.selected_format_idx).copied().unwrap_or(SAVE_FORMATS[0]);
                    egui::ComboBox::from_id_salt("android_save_format_combo")
                        .selected_text(current_fmt.0)
                        .show_ui(ui, |ui| {
                            for (idx, (label, ext)) in SAVE_FORMATS.iter().enumerate() {
                                if ui.selectable_value(&mut active.selected_format_idx, idx, *label).clicked() {
                                    active.file_name_input = apply_save_extension(&active.file_name_input, ext);
                                }
                            }
                        });
                });
            }

            // Bottom action buttons
            ui.horizontal(|ui| {
                match &active.request {
                    FileDialogRequest::Open { .. } => {
                        let can_open = !active.selected_paths.is_empty();
                        if ui.add_enabled(can_open, egui::Button::new("Import / Open")).clicked() {
                            let paths = active
                                .selected_paths
                                .iter()
                                .map(|p| p.to_string_lossy().into_owned())
                                .collect();
                            finish_answer = Some(Some(FileDialogAnswer::Paths(paths)));
                        }
                    }
                    FileDialogRequest::Folder { .. } => {
                        if ui.button("Use This Folder").clicked() {
                            finish_answer = Some(Some(FileDialogAnswer::FolderTo(
                                active.current_dir.to_string_lossy().into_owned(),
                            )));
                        }
                    }
                    FileDialogRequest::Save { .. } => {
                        if ui.button("Export / Save Here").clicked() {
                            let (_, ext) = SAVE_FORMATS.get(active.selected_format_idx).copied().unwrap_or(SAVE_FORMATS[0]);
                            let name = apply_save_extension(&active.file_name_input, ext);
                            let full_path = active.current_dir.join(name);
                            finish_answer = Some(Some(FileDialogAnswer::SaveTo(
                                full_path.to_string_lossy().into_owned(),
                            )));
                        }
                    }
                }
                if ui.button("Cancel").clicked() {
                    finish_answer = Some(None);
                }
            });
        });

    if let Some(answer) = finish_answer
        && let Some(done) = guard.take()
    {
        done.reply.send(answer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_save_extension_preserves_or_updates_extension_cleanly() {
        assert_eq!(apply_save_extension("Untitled-1.psd", "png"), "Untitled-1.png");
        assert_eq!(apply_save_extension("photo", "jpg"), "photo.jpg");
        assert_eq!(apply_save_extension("artwork.PNG", "png"), "artwork.PNG");
        assert_eq!(apply_save_extension("", "pcraft"), "Untitled-1.pcraft");
    }

    #[test]
    fn extension_filter_matches_supported_formats_case_insensitively() {
        assert!(matches_extension_filter(Path::new("/storage/emulated/0/Download/test.PSD"), None));
        assert!(matches_extension_filter(Path::new("/storage/emulated/0/Pictures/photo.JPG"), None));
        assert!(!matches_extension_filter(Path::new("/storage/emulated/0/Download/notes.txt"), None));
        let custom = vec!["abr".to_string()];
        assert!(matches_extension_filter(Path::new("/storage/emulated/0/Download/brushes.ABR"), Some(&custom)));
        assert!(!matches_extension_filter(Path::new("/storage/emulated/0/Download/photo.png"), Some(&custom)));
    }
}
