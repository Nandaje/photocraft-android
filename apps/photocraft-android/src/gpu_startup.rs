//! GPU acceleration startup and crash-safe backend selection for Android.
//!
//! Android devices expose hardware acceleration through **Vulkan 1.1+** (Adreno, Mali,
//! Immortalis, Xclipse, PowerVR) and **OpenGL ES 3.0+** (`wgpu::Backends::GL`).
//!
//! By default (`GpuBackend::Auto`, `RenderingMode::Auto` or `RenderingMode::Gpu`), PhotoCraft on
//! Android enables both `wgpu::Backends::VULKAN` and `wgpu::Backends::GL`, preferring hardware
//! Vulkan first and falling back to OpenGL ES if Vulkan surface creation or driver initialization
//! fails. Full adapter texture limits (`max_texture_dimension_2d` up to 8192/16384 px) are
//! requested via [`photocraft_ui_egui::gpu_canvas::use_adapter_limits_with`] so both the tiled
//! GPU canvas (`GpuCanvas`) and the `photocraft-gpu` layer compositor run on the GPU.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use eframe::{egui_wgpu, wgpu};
use photocraft_engine::prefs::{GpuBackend, RenderingMode};
use serde_json::{Value, json};

/// Crash-recovery marker file name in the Android config directory.
pub const MARKER_FILE: &str = "gpu-starting.json";
const MARKER_MAX: u64 = 64 << 10;

/// What a start records before touching the graphics driver.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Marker {
    pub backend: String,
    pub adapter: String,
    pub adapter_backend: String,
    pub driver: String,
    pub version: String,
}

impl Marker {
    pub fn to_json(&self) -> String {
        json!({
            "backend": self.backend,
            "adapter": self.adapter,
            "adapterBackend": self.adapter_backend,
            "driver": self.driver,
            "version": self.version,
        })
        .to_string()
    }

    pub fn parse(text: &str) -> Self {
        let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").chars().take(256).collect::<String>();
        Marker {
            backend: s("backend"),
            adapter: s("adapter"),
            adapter_backend: s("adapterBackend"),
            driver: s("driver"),
            version: s("version"),
        }
    }

    pub fn tried(&self) -> Option<GpuBackend> {
        let planned = if self.backend.is_empty() { Some(GpuBackend::Auto) } else { GpuBackend::parse(&self.backend) }?;
        match (planned, GpuBackend::parse(&self.adapter_backend)) {
            (GpuBackend::Auto, Some(actual)) => Some(actual),
            _ => Some(planned),
        }
    }
}

/// The GPU backend plan for this launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub backend: GpuBackend,
    pub env: Option<String>,
    pub reason: Option<String>,
    pub remember: bool,
}

/// Sanitize a backend preference for Android (DirectX 12 and Metal do not exist on Android and map
/// to `Auto`, which enables Vulkan + OpenGL ES).
pub fn sanitize(pref: GpuBackend) -> GpuBackend {
    match pref {
        GpuBackend::Dx12 | GpuBackend::Metal => GpuBackend::Auto,
        other => other,
    }
}

/// Fallback chain on Android: `Auto`/`Vulkan` -> `Gl` (OpenGL ES) -> `Cpu`.
pub fn next_safer(tried: GpuBackend) -> GpuBackend {
    match tried {
        GpuBackend::Auto | GpuBackend::Vulkan | GpuBackend::Dx12 | GpuBackend::Metal => GpuBackend::Gl,
        GpuBackend::Gl | GpuBackend::Cpu => GpuBackend::Cpu,
    }
}

/// Decide this launch's GPU backend from saved preferences, any crash marker, `WGPU_BACKEND`, and
/// `safe_gpu`.
pub fn plan(pref: GpuBackend, crashed: Option<&Marker>, env: Option<&str>, safe_gpu: bool) -> Plan {
    if let Some(v) = env.map(str::trim).filter(|v| !v.is_empty()) {
        let backend = if safe_gpu { GpuBackend::Cpu } else { GpuBackend::Auto };
        return Plan {
            backend,
            env: Some(v.to_string()),
            reason: Some(format!("WGPU_BACKEND={v} overrides the backend")),
            remember: false,
        };
    }
    if safe_gpu {
        return Plan {
            backend: GpuBackend::Cpu,
            env: None,
            reason: Some("safe-gpu: CPU renderer for this launch".into()),
            remember: false,
        };
    }
    let pref = sanitize(pref);
    if let Some((m, tried)) = crashed.and_then(|m| Some((m, m.tried()?))) {
        let next = next_safer(tried);
        let on = if m.adapter.is_empty() { String::new() } else { format!(" ({})", m.adapter) };
        return Plan {
            backend: next,
            env: None,
            reason: Some(format!("the previous start didn't finish on {}{on}; using {}", tried.name(), next.name())),
            remember: next != pref,
        };
    }
    Plan { backend: pref, env: None, reason: None, remember: false }
}

/// Resolve the rendering policy with [`RenderingMode`]. On Android, GPU acceleration is enabled by
/// default (`RenderingMode::Auto` or `RenderingMode::Gpu`).
pub fn plan_with_mode(pref: GpuBackend, mode: RenderingMode, crashed: Option<&Marker>, env: Option<&str>, safe_gpu: bool) -> Plan {
    let backend = match mode {
        RenderingMode::Cpu => GpuBackend::Cpu,
        RenderingMode::Auto | RenderingMode::Gpu if pref == GpuBackend::Cpu => GpuBackend::Auto,
        _ => pref,
    };
    let env = if safe_gpu || mode == RenderingMode::Cpu { None } else { env };
    let crashed = if mode == RenderingMode::Cpu { None } else { crashed };
    plan(backend, crashed, env, safe_gpu)
}

/// Persist hardware fallbacks (e.g. Vulkan -> OpenGL ES), but never permanently pin CPU mode.
pub fn backend_to_remember(plan: &Plan) -> Option<GpuBackend> {
    (plan.remember && plan.backend != GpuBackend::Cpu).then_some(plan.backend)
}

/// `wgpu::Backends` for Android:
/// - `Auto`: `VULKAN | GL` (hardware Vulkan with OpenGL ES 3.0+ fallback)
/// - `Vulkan`: `VULKAN`
/// - `Gl`: `GL`
/// - `Cpu`: `GL | VULKAN` (window surface presentation while canvas composites on CPU)
pub fn backends(plan: &Plan) -> Option<wgpu::Backends> {
    if plan.env.is_some() {
        return None;
    }
    Some(match plan.backend {
        GpuBackend::Auto | GpuBackend::Dx12 | GpuBackend::Metal => wgpu::Backends::VULKAN | wgpu::Backends::GL,
        GpuBackend::Vulkan => wgpu::Backends::VULKAN,
        GpuBackend::Gl => wgpu::Backends::GL,
        GpuBackend::Cpu => wgpu::Backends::GL | wgpu::Backends::VULKAN,
    })
}

/// Candidate adapter evaluated during startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub vendor: u32,
    pub device_type: wgpu::DeviceType,
    pub backend: wgpu::Backend,
    pub surface_ok: bool,
}

fn rank(a: &Candidate, backend: GpuBackend) -> (u8, u8) {
    let ty = match a.device_type {
        wgpu::DeviceType::DiscreteGpu => 0,
        wgpu::DeviceType::IntegratedGpu => 1,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Other => 3,
        wgpu::DeviceType::Cpu => 4,
    };
    let ty = match (backend, a.device_type) {
        (GpuBackend::Cpu, wgpu::DeviceType::Cpu) => 0,
        (GpuBackend::Cpu, _) => ty + 1,
        _ => ty,
    };
    let be = match a.backend {
        wgpu::Backend::Vulkan => 0,
        wgpu::Backend::Gl => 1,
        _ => 2,
    };
    (ty, be)
}

/// Pick the best Android GPU adapter that can present to the surface.
pub fn pick(adapters: &[Candidate], backend: GpuBackend) -> Option<usize> {
    adapters
        .iter()
        .enumerate()
        .filter(|(_, a)| a.surface_ok)
        .min_by_key(|(i, a)| (rank(a, backend), *i))
        .map(|(i, _)| i)
}

/// Configure `eframe`'s `WgpuSetup` for Android GPU acceleration and unlock the hardware adapter's
/// full texture limits for `GpuCanvas` and `photocraft-gpu`.
pub fn configure(setup: &mut egui_wgpu::WgpuSetup, plan: &Plan, sentinel: SharedSentinel, note: Arc<Mutex<Option<String>>>) {
    if let egui_wgpu::WgpuSetup::CreateNew(create) = setup {
        if let Some(b) = backends(plan) {
            create.instance_descriptor.backends = b;
        }
        create.power_preference = wgpu::PowerPreference::HighPerformance;
        let backend = plan.backend;
        create.native_adapter_selector = Some(Arc::new(move |adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>| {
            let cands: Vec<Candidate> = adapters
                .iter()
                .map(|a| {
                    let i = a.get_info();
                    Candidate {
                        vendor: i.vendor,
                        device_type: i.device_type,
                        backend: i.backend,
                        surface_ok: surface.is_none_or(|s| a.is_surface_supported(s)),
                    }
                })
                .collect();
            let i = pick(&cands, backend).ok_or_else(|| "no compatible Android GPU adapter found".to_string())?;
            if backend == GpuBackend::Cpu && cands.get(i).is_some_and(|c| c.device_type != wgpu::DeviceType::Cpu) {
                *note.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some("CPU image rendering; window presentation uses Android hardware graphics".into());
            }
            adapters.get(i).cloned().ok_or_else(|| "no compatible Android GPU adapter found".to_string())
        }));
    }
    photocraft_ui_egui::gpu_canvas::use_adapter_limits_with(setup, move |adapter| {
        let info = adapter.get_info();
        let mut guard = sentinel.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(s) = guard.as_mut() {
            let mut m = s.marker.clone();
            m.adapter = info.name.clone();
            m.adapter_backend = photocraft_ui_egui::gpu_canvas::backend_name(info.backend).to_string();
            m.driver = [info.driver.as_str(), info.driver_info.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            if let Err(e) = s.write(m) {
                log::warn!("Android GPU startup marker: {e}");
            }
        }
    });
}

/// Previous launch state from the marker file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Previous {
    Clean,
    Crashed(Marker),
    Busy,
}

impl Previous {
    pub fn crashed(&self) -> Option<&Marker> {
        match self {
            Previous::Crashed(m) => Some(m),
            _ => None,
        }
    }
}

/// Locked marker file for the active startup sequence.
#[derive(Debug)]
pub struct Sentinel {
    path: PathBuf,
    file: Option<File>,
    marker: Marker,
}

pub type SharedSentinel = Arc<Mutex<Option<Sentinel>>>;

impl Sentinel {
    pub fn begin(dir: &Path) -> (Previous, Option<Sentinel>) {
        let path = dir.join(MARKER_FILE);
        if let Err(e) = std::fs::create_dir_all(dir) {
            log::warn!("Android GPU startup marker dir: {e}");
            return (Previous::Clean, None);
        }
        let mut file = match OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("Android GPU startup marker {}: {e}", path.display());
                return (Previous::Clean, None);
            }
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return (Previous::Busy, None),
            Err(std::fs::TryLockError::Error(e)) => {
                log::warn!("Android GPU startup marker lock: {e}");
                return (Previous::Clean, None);
            }
        }
        let mut text = String::new();
        let _ = (&mut file).take(MARKER_MAX).read_to_string(&mut text);
        let previous = if text.trim().is_empty() { Previous::Clean } else { Previous::Crashed(Marker::parse(&text)) };
        (previous, Some(Sentinel { path, file: Some(file), marker: Marker::default() }))
    }

    pub fn write(&mut self, m: Marker) -> std::io::Result<()> {
        let f = self.file.as_mut().ok_or_else(|| std::io::Error::other("marker closed"))?;
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(m.to_json().as_bytes())?;
        f.flush()?;
        self.marker = m;
        Ok(())
    }

    pub fn finish(mut self) {
        if let Some(f) = self.file.take() {
            let _ = f.set_len(0);
            drop(f);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Read low-latency drawing preference (`performance.lowLatencyDrawing`).
pub fn read_low_latency(path: Option<&Path>) -> bool {
    let v: Value = path.and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let default = photocraft_engine::prefs::Preferences::default().performance.low_latency_drawing;
    v.get("performance").and_then(|p| p.get("lowLatencyDrawing")).and_then(Value::as_bool).unwrap_or(default)
}

/// Read GPU rendering preferences (`gpuBackend`, `useGpu`, `renderingMode`).
/// Defaults to `(GpuBackend::Auto, RenderingMode::Auto)` (GPU acceleration enabled).
pub fn read_rendering_prefs(path: Option<&Path>) -> (GpuBackend, RenderingMode) {
    let v: Value = path.and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let perf = v.get("performance");
    let backend = perf.and_then(|p| p.get("gpuBackend")).and_then(Value::as_str).and_then(GpuBackend::parse).unwrap_or_default();
    let use_gpu = perf.and_then(|p| p.get("useGpu")).and_then(Value::as_bool).unwrap_or(true);
    let explicit = perf.and_then(|p| p.get("renderingMode")).and_then(Value::as_str).and_then(RenderingMode::parse);
    let mode = explicit.unwrap_or(if !use_gpu || backend == GpuBackend::Cpu { RenderingMode::Cpu } else { RenderingMode::Auto });
    (backend, mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_plan_enables_vulkan_and_gles_gpu_acceleration() {
        let (pref, mode) = read_rendering_prefs(None);
        assert_eq!(pref, GpuBackend::Auto);
        assert_eq!(mode, RenderingMode::Auto);
        let p = plan_with_mode(pref, mode, None, None, false);
        assert_eq!(p.backend, GpuBackend::Auto);
        assert_eq!(backends(&p), Some(wgpu::Backends::VULKAN | wgpu::Backends::GL));
    }

    #[test]
    fn vulkan_crash_falls_back_to_gles_then_cpu() {
        let m = Marker {
            backend: "auto".into(),
            adapter: "Adreno (TM) 740".into(),
            adapter_backend: "vulkan".into(),
            ..Default::default()
        };
        let p1 = plan_with_mode(GpuBackend::Auto, RenderingMode::Auto, Some(&m), None, false);
        assert_eq!(p1.backend, GpuBackend::Gl);
        assert!(p1.remember);
        assert_eq!(backend_to_remember(&p1), Some(GpuBackend::Gl));

        let m2 = Marker {
            backend: "gl".into(),
            adapter: "Adreno (TM) 740".into(),
            adapter_backend: "gl".into(),
            ..Default::default()
        };
        let p2 = plan_with_mode(GpuBackend::Gl, RenderingMode::Auto, Some(&m2), None, false);
        assert_eq!(p2.backend, GpuBackend::Cpu);
        assert_eq!(backend_to_remember(&p2), None, "CPU fallback is never permanently pinned");
    }

    #[test]
    fn adapter_picker_prefers_hardware_vulkan_over_gles() {
        let cands = [
            Candidate { vendor: 0x5143, device_type: wgpu::DeviceType::IntegratedGpu, backend: wgpu::Backend::Gl, surface_ok: true },
            Candidate { vendor: 0x5143, device_type: wgpu::DeviceType::IntegratedGpu, backend: wgpu::Backend::Vulkan, surface_ok: true },
        ];
        assert_eq!(pick(&cands, GpuBackend::Auto), Some(1));
    }
}
