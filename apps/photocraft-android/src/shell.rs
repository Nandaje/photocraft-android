//! Android shell wrapping [`photocraft_ui_egui::PhotocraftApp`] with touch-twin filtering, GPU
//! canvas activation, and the Android file/permission modal overlay.

use std::sync::{Arc, Mutex, PoisonError};

use photocraft_engine::Session;
use photocraft_engine::prefs::{GpuBackend, RenderingMode};
use photocraft_ui_egui::PhotocraftApp;
use photocraft_ui_egui::theme::ThemeKind;

use crate::file_dialog;
use crate::gpu_startup::{self, Plan, SharedSentinel};
use crate::services;

/// The `eframe::App` wrapper for PhotoCraft on Android.
pub struct AndroidShell {
    pub app: PhotocraftApp,
}

impl AndroidShell {
    /// Construct the Android shell inside `eframe::run_native`'s creation callback, enabling the
    /// `wgpu` GPU canvas (`GpuCanvas`) and `photocraft-gpu` layer compositor when hardware graphics
    /// are available.
    pub fn create(
        cc: &eframe::CreationContext<'_>,
        plan: &Plan,
        pref: GpuBackend,
        mode: RenderingMode,
        present_desc: String,
        uncapped: bool,
        sentinel_ms: f64,
        started_sentinel: SharedSentinel,
        gpu_note: Arc<Mutex<Option<String>>>,
    ) -> Self {
        PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::Pro);

        let mut svcs = services::android_services();
        svcs.preset_store = Some(photocraft_engine::preset_store::open_dir_async(services::presets_dir()));

        let session = Session::new();
        let mut app = PhotocraftApp::new(session, svcs);
        app.set_theme(&cc.egui_ctx, ThemeKind::Pro);
        app.min_frame_ms = uncapped.then_some(1000.0 / 240.0);
        app.integrated_titlebar = false;
        app.custom_titlebar = false;
        app.background_jobs = std::env::var_os("PHOTOCRAFT_INLINE_JOBS").is_none();

        let info = &mut app.perf.gpu_info;
        info.preference = pref.name().to_string();
        info.present = present_desc;
        info.selected = if plan.env.is_some() { "env".into() } else { plan.backend.name().to_string() };
        info.fallback = match (
            plan.reason.clone(),
            gpu_note.lock().unwrap_or_else(PoisonError::into_inner).clone(),
        ) {
            (Some(reason), Some(note)) => Some(format!("{reason}. {note}")),
            (reason, note) => reason.or(note),
        };
        info.canvas = "cpu".into();

        if let Some(rs) = cc.wgpu_render_state.clone() {
            let adapter_info = rs.adapter.get_info();
            log::info!(
                "photocraft-android: wgpu adapter {:?} ({:?}, backend {:?})",
                adapter_info.name,
                adapter_info.device_type,
                adapter_info.backend
            );
            app.perf.gpu_info.set_adapter(&adapter_info);
            app.perf.gpu_info.set_available(&rs.available_adapters);
            let software_window = adapter_info.device_type == eframe::wgpu::DeviceType::Cpu;
            if software_window && mode != RenderingMode::Cpu {
                let software = "No compatible hardware graphics adapter; using software graphics.";
                let fallback = &mut app.perf.gpu_info.fallback;
                *fallback = Some(match fallback.take() {
                    Some(earlier) => format!("{earlier}. {software}"),
                    None => software.into(),
                });
            }
            if !software_window
                && plan.backend != GpuBackend::Cpu
                && std::env::var_os("PHOTOCRAFT_CPU_CANVAS").is_none()
                && app.session.prefs().performance.effective_rendering_mode() != RenderingMode::Cpu
            {
                app.set_wgpu(rs);
            } else {
                app.watch_window_device(&rs.device);
            }
        }

        if app.perf.gpu_info.canvas == "cpu"
            && mode != RenderingMode::Cpu
            && let Some(reason) = app.perf.gpu_info.fallback.clone()
        {
            photocraft_ui_egui::gpu_status::queue_fallback_notice(&mut app, &reason);
        }

        app.perf.span("gpuSentinel", sentinel_ms);
        let remember = gpu_startup::backend_to_remember(plan);
        app.on_started(move |app| {
            if let Some(s) = started_sentinel.lock().unwrap_or_else(PoisonError::into_inner).take() {
                s.finish();
            }
            if let Some(b) = remember
                && app.session.prefs().performance.gpu_backend != b
                && let Err(e) = app.run("prefs.set", serde_json::json!({"path": "performance.gpuBackend", "value": b.name()}))
            {
                log::warn!("couldn't remember Android GPU backend {}: {e}", b.name());
            }
        });

        Self { app }
    }
}

impl eframe::App for AndroidShell {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // Deduplicate simultaneous pointer + touch events on Android touchscreens so a single tap
        // on a menu item does not open and immediately close the menu (#2095).
        photocraft_ui_egui::touch_twins::drop_twins(ctx, raw_input);
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.app.logic(ctx, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
        file_dialog::render_modal(ui.ctx());
    }
}
