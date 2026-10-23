//! Android platform services: file import/export with permission fallback, SAF dialog, crash
//! recovery, preferences, and brush preset storage.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use photocraft_codecs::{ChannelLayout, EncodeOptions, Image};
use photocraft_doc::Document;
use photocraft_format::RecoveryStore;
use photocraft_ui_egui::{Inbox, Recoverable, Services};

use crate::file_dialog;
use crate::permissions;

/// Global inbox for files opened via Android `Intent.ACTION_VIEW` / `Intent.ACTION_EDIT` /
/// `Intent.ACTION_SEND` or asynchronous SAF delivery.
static GLOBAL_INBOX: OnceLock<Inbox> = OnceLock::new();
static GLOBAL_PLACE_INBOX: OnceLock<Inbox> = OnceLock::new();

pub fn global_inbox() -> Inbox {
    GLOBAL_INBOX.get_or_init(|| Arc::new(Mutex::new(Vec::new()))).clone()
}

pub fn global_place_inbox() -> Inbox {
    GLOBAL_PLACE_INBOX.get_or_init(|| Arc::new(Mutex::new(Vec::new()))).clone()
}

/// Push a file (`name`, `bytes`) from an Android Intent or JNI callback into the open queue.
pub fn enqueue_incoming_file(name: String, bytes: Vec<u8>, place_on_canvas: bool) {
    let queue = if place_on_canvas { global_place_inbox() } else { global_inbox() };
    queue.lock().unwrap_or_else(PoisonError::into_inner).push((name, bytes));
}

/// Per-user settings directory on Android (`internal_data` from `AndroidApp`, or
/// `PHOTOCRAFT_CONFIG_DIR`).
pub fn config_dir() -> PathBuf {
    permissions::current_dirs().internal_data
}

pub fn prefs_file() -> PathBuf {
    config_dir().join("preferences.json")
}

pub fn presets_dir() -> PathBuf {
    config_dir().join("Presets")
}

pub fn recovery_dir() -> PathBuf {
    config_dir().join("Recovery")
}

/// Write `bytes` crash-safely to `path`, automatically falling back to the writable Android
/// `Download` or `Exports` directory if Android scoped storage restricts direct writes to `path`,
/// and notifying the Android `MediaScannerConnection` so exported images appear in the Gallery.
pub fn write_with_android_permissions(path: &Path, bytes: &[u8], progress: &dyn Fn(f32)) -> Result<Vec<String>, String> {
    let dirs = permissions::current_dirs();
    let (target, fallback_note) = permissions::resolve_export_target(path, &dirs);
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
    }
    photocraft_format::atomic_write_progress(&target, bytes, progress).map_err(|e| e.to_string())?;
    if let Some(target_str) = target.to_str() {
        file_dialog::notify_media_scanner(target_str);
    }
    Ok(fallback_note.into_iter().collect())
}

const ENCODE_SHARE: f32 = 0.85;

fn save_file(
    doc: &Document,
    path: &str,
    settings: &photocraft_ui_egui::ExportSettings,
    ctl: &photocraft_ui_egui::jobs_ui::SaveCtl,
) -> Result<Vec<String>, String> {
    ctl.progress(0.0, "Encoding");
    let opts = settings.io_options();
    let cancelled = || ctl.check().is_err();
    let progress = |f: f32| ctl.progress(f * ENCODE_SHARE, "");
    let interrupt = photocraft_raster::Interrupt::new(&cancelled, &progress);
    let mut r = photocraft_io::export_with(doc, path, &opts, &interrupt).map_err(|e| e.to_string())?;
    ctl.commit()?;
    ctl.progress(ENCODE_SHARE, "Writing");
    let notes = write_with_android_permissions(
        Path::new(path),
        &r.bytes,
        &|f| ctl.progress(ENCODE_SHARE + (0.99 - ENCODE_SHARE) * f, ""),
    )?;
    r.warnings.extend(notes);
    ctl.progress(1.0, "");
    Ok(r.warnings)
}

type SharedRecovery = Rc<RefCell<Option<RecoveryStore>>>;

fn with_store<R>(store: &SharedRecovery, f: impl FnOnce(&mut RecoveryStore) -> R) -> Result<R, String> {
    let mut slot = store.try_borrow_mut().map_err(|_| "crash recovery is busy".to_string())?;
    Ok(f(slot.as_mut().ok_or("no config directory")?))
}

fn recovery_services(dir: PathBuf) -> Services {
    let recover_dir = dir.clone();
    let store: SharedRecovery = Rc::new(RefCell::new(Some(RecoveryStore::new(dir))));
    let (s1, s2, s4, s5) = (store.clone(), store.clone(), store.clone(), store.clone());
    Services {
        autosave: Some(Box::new(move |doc: &Arc<Document>, revision: u64, path: Option<&str>| {
            with_store(&s1, |s| s.autosave_checked(doc, revision, path.map(str::to_string)))?
        })),
        autosave_results: Some(Box::new(move || with_store(&s4, |s| s.take_completed()).unwrap_or_default())),
        flush_autosaves: Some(Box::new(move || with_store(&s5, RecoveryStore::flush).unwrap_or_default())),
        discard_autosave: Some(Box::new(move |id: u64| {
            let _ = with_store(&s2, |s| s.discard(id));
        })),
        recover: Some(Box::new(move || {
            photocraft_format::list_recovery(&recover_dir)
                .into_iter()
                .map(|e| Recoverable {
                    key: e.info.key.clone(),
                    name: e.info.document_name.clone(),
                    path: e.info.original_path.clone(),
                    load: Box::new(move || photocraft_format::recover(&e).map_err(|e| e.to_string())),
                })
                .collect()
        })),
        adopt_autosave: Some(Box::new(move |id: u64, key: &str| {
            let _ = with_store(&store, |s| s.adopt(id, key));
        })),
        ..Default::default()
    }
}

/// Construct the Android [`Services`] table for [`photocraft_ui_egui::PhotocraftApp`].
pub fn android_services() -> Services {
    let inbox = global_inbox();
    let place_inbox = global_place_inbox();
    Services {
        import: Some(Box::new(|name: &str, bytes: &[u8], opts: &photocraft_io::ImportOptions| {
            photocraft_io::import_with_options(name, bytes, opts)
                .map(|r| (r.document, r.warnings))
                .map_err(|e| e.to_string())
        })),
        export: Some(Box::new(|doc: &Document, path: &str, settings: &photocraft_ui_egui::ExportSettings| {
            let opts = settings.io_options();
            photocraft_io::export(doc, path, &opts).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
        })),
        save_file: Some(Arc::new(save_file)),
        file_dialog: Some(Box::new(file_dialog::show_file_dialog)),
        write: Some(Box::new(|path: &str, bytes: &[u8]| {
            write_with_android_permissions(Path::new(path), bytes, &|_| {}).map(|_| ())
        })),
        file_stamp: Some(Box::new(|path: &str| {
            let meta = std::fs::metadata(path).ok()?;
            Some(photocraft_ui_egui::FileStamp { len: meta.len(), modified: meta.modified().ok() })
        })),
        encode_png: Some(Box::new(|w, h, rgba| {
            let img = Image::from_u8(w, h, ChannelLayout::Rgba, rgba.to_vec()).map_err(|e| e.to_string())?;
            photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &EncodeOptions::default()).map_err(|e| e.to_string())
        })),
        inbox: Some(inbox),
        place_inbox: Some(place_inbox),
        load_prefs: Some(Box::new(|| std::fs::read_to_string(prefs_file()).ok())),
        save_prefs: Some(Box::new(|text: &str| {
            let file = prefs_file();
            if let Some(parent) = file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            photocraft_format::atomic_write(&file, text.as_bytes()).map_err(|e| e.to_string())
        })),
        append_text: Some(Box::new(|path: &str, text: &str| {
            use std::io::Write;
            let dirs = permissions::current_dirs();
            let (target, _) = permissions::resolve_export_target(Path::new(path), &dirs);
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(target).map_err(|e| e.to_string())?;
            f.write_all(text.as_bytes()).map_err(|e| e.to_string())
        })),
        ..recovery_services(recovery_dir())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_color::{ColorMode, SampleType};
    use photocraft_doc::{Layer, Size};
    use photocraft_geom::Rect;

    #[test]
    fn android_services_roundtrip_export_and_import_with_permissions() {
        let dir = std::env::temp_dir().join(format!("photocraft-android-svc-test-{}", std::process::id()));
        let internal = dir.join("internal");
        let external = dir.join("external");
        let shared = dir.join("shared");
        let _ = std::fs::create_dir_all(shared.join("Download"));
        permissions::init_dirs(Some(internal), Some(external));

        let mut svcs = android_services();
        let mut doc = Document::new("AndroidTest", Size::new(16, 16), ColorMode::Rgb, SampleType::U8);
        let mut layer = Layer::raster("Background", doc.pixel_format());
        let px = vec![0.5f32; 16 * 16 * 4];
        if let Some(surf) = layer.surface_mut() {
            surf.write_region(Rect::from_xywh(0, 0, 16, 16), &px);
        }
        doc.layers.push(layer);

        let out_path = shared.join("Download").join("android_export.png");
        let out_str = out_path.to_string_lossy().into_owned();
        let settings = photocraft_ui_egui::ExportSettings::default();

        let (bytes, warnings) = (svcs.export.as_ref().ok_or("no export").unwrap())(&doc, &out_str, &settings).unwrap();
        assert!(warnings.is_empty());
        (svcs.write.as_mut().ok_or("no write").unwrap())(&out_str, &bytes).unwrap();
        assert!(out_path.is_file(), "exported PNG should be written to Download directory");

        let read_back = std::fs::read(&out_path).unwrap();
        let (imported, _) = (svcs.import.as_ref().ok_or("no import").unwrap())("android_export.png", &read_back, 64).unwrap();
        assert_eq!(imported.size, Size::new(16, 16));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
