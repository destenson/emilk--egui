//! This crates provides bindings between [`egui`](https://github.com/emilk/egui) and [wgpu](https://crates.io/crates/wgpu).
//!
//! If you're targeting WebGL you also need to turn on the
//! `webgl` feature of the `wgpu` crate:
//!
//! ```toml
//! # Enable both WebGL and WebGPU backends on web.
//! wgpu = { version = "*", features = ["webgpu", "webgl"] }
//! ```
//!
//! You can control whether WebGL or WebGPU will be picked at runtime by configuring
//! [`WgpuConfiguration::wgpu_setup`].
//! The default is to prefer WebGPU and fall back on WebGL.
//!
//! ## Feature flags
#![doc = document_features::document_features!()]
//!

#![allow(unsafe_code)]

pub use wgpu;

/// Low-level painting of [`egui`](https://github.com/emilk/egui) on [`wgpu`].
mod renderer;

pub use renderer::*;
use wgpu::{Adapter, Device, Instance, Queue};

/// Helpers for capturing screenshots of the UI.
pub mod capture;

/// Module for painting [`egui`](https://github.com/emilk/egui) with [`wgpu`] on [`winit`].
#[cfg(feature = "winit")]
pub mod winit;

use std::sync::Arc;

use epaint::mutex::RwLock;

/// An error produced by egui-wgpu.
#[derive(thiserror::Error, Debug)]
pub enum WgpuError {
    #[error("Failed to create wgpu adapter, no suitable adapter found: {0}")]
    NoSuitableAdapterFound(String),

    #[error("There was no valid format for the surface at all.")]
    NoSurfaceFormatsAvailable,

    #[error(transparent)]
    RequestDeviceError(#[from] wgpu::RequestDeviceError),

    #[error(transparent)]
    CreateSurfaceError(#[from] wgpu::CreateSurfaceError),

    #[cfg(feature = "winit")]
    #[error(transparent)]
    HandleError(#[from] ::winit::raw_window_handle::HandleError),
}

/// Access to the render state for egui.
#[derive(Clone)]
pub struct RenderState {
    /// Wgpu adapter used for rendering.
    pub adapter: Arc<wgpu::Adapter>,

    /// All the available adapters.
    ///
    /// This is not available on web.
    /// On web, we always select WebGPU is available, then fall back to WebGL if not.
    // TODO(wgpu#6665): Remove layer of `Arc` here once we update to wgpu 24
    #[cfg(not(target_arch = "wasm32"))]
    pub available_adapters: Arc<[Arc<wgpu::Adapter>]>,

    /// Wgpu device used for rendering, created from the adapter.
    pub device: Arc<wgpu::Device>,

    /// Wgpu queue used for rendering, created from the adapter.
    pub queue: Arc<wgpu::Queue>,

    /// The target texture format used for presenting to the window.
    pub target_format: wgpu::TextureFormat,

    /// Egui renderer responsible for drawing the UI.
    pub renderer: Arc<RwLock<Renderer>>,
}

async fn request_adapter(
    instance: &Instance,
    power_preference: wgpu::PowerPreference,
    surface: &wgpu::Surface<'_>,
    available_adapters: &[Arc<wgpu::Adapter>],
) -> Result<Arc<wgpu::Adapter>, WgpuError> {
    profiling::function_scope!();

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference,
            compatible_surface: Some(surface),
            // We don't expose this as an option right now since it's fairly rarely useful:
            // * only has an effect on native
            // * fails if there's no software rasterizer available
            // * can achieve the same with `native_adapter_selector`
            force_fallback_adapter: false,
        })
        .await
        .ok_or_else(|| {
            #[cfg(not(target_arch = "wasm32"))]
            if available_adapters.is_empty() {
                log::info!("No wgpu adapters found");
            } else if available_adapters.len() == 1 {
                log::info!(
                    "The only available wgpu adapter was not suitable: {}",
                    adapter_info_summary(&available_adapters[0].get_info())
                );
            } else {
                log::info!(
                    "No suitable wgpu adapter found out of the {} available ones: {}",
                    available_adapters.len(),
                    describe_adapters(available_adapters)
                );
            }

            WgpuError::NoSuitableAdapterFound("`request_adapters` returned `None`".to_owned())
        })?;

    #[cfg(target_arch = "wasm32")]
    log::debug!(
        "Picked wgpu adapter: {}",
        adapter_info_summary(&adapter.get_info())
    );

    #[cfg(not(target_arch = "wasm32"))]
    if available_adapters.len() == 1 {
        log::debug!(
            "Picked the only available wgpu adapter: {}",
            adapter_info_summary(&adapter.get_info())
        );
    } else {
        log::info!(
            "There were {} available wgpu adapters: {}",
            available_adapters.len(),
            describe_adapters(available_adapters)
        );
        log::debug!(
            "Picked wgpu adapter: {}",
            adapter_info_summary(&adapter.get_info())
        );
    }

    Ok(Arc::new(adapter))
}

impl RenderState {
    /// Creates a new [`RenderState`], containing everything needed for drawing egui with wgpu.
    ///
    /// # Errors
    /// Wgpu initialization may fail due to incompatible hardware or driver for a given config.
    pub async fn create(
        config: &WgpuConfiguration,
        instance: &wgpu::Instance,
        surface: &wgpu::Surface<'static>,
        depth_format: Option<wgpu::TextureFormat>,
        msaa_samples: u32,
        dithering: bool,
    ) -> Result<Self, WgpuError> {
        profiling::scope!("RenderState::create"); // async yield give bad names using `profile_function`

        // This is always an empty list on web.
        #[cfg(not(target_arch = "wasm32"))]
        let available_adapters = {
            let backends = if let WgpuSetup::CreateNew(WgpuSetupCreateNew {
                supported_backends,
                ..
            }) = &config.wgpu_setup
            {
                *supported_backends
            } else {
                wgpu::Backends::all()
            };

            instance
                .enumerate_adapters(backends)
                // TODO(wgpu#6665): Remove layer of `Arc` here once we update to wgpu 24.
                .into_iter()
                .map(Arc::new)
                .collect::<Vec<_>>()
        };

        let (adapter, device, queue) = match config.wgpu_setup.clone() {
            WgpuSetup::CreateNew(WgpuSetupCreateNew {
                supported_backends: _,
                power_preference,
                native_adapter_selector,
                device_descriptor,
                trace_path,
            }) => {
                let adapter = {
                    #[cfg(target_arch = "wasm32")]
                    {
                        request_adapter(instance, power_preference, surface, &available_adapters)
                            .await
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    if let Some(native_adapter_selector) = native_adapter_selector {
                        native_adapter_selector(&available_adapters, Some(surface))
                            .map_err(WgpuError::NoSuitableAdapterFound)
                    } else {
                        request_adapter(instance, power_preference, surface, &available_adapters)
                            .await
                    }
                }?;

                let (device, queue) = {
                    profiling::scope!("request_device");
                    adapter
                        .request_device(&(*device_descriptor)(&adapter), trace_path.as_deref())
                        .await?
                };

                // On wasm, depending on feature flags, wgpu objects may or may not implement sync.
                // It doesn't make sense to switch to Rc for that special usecase, so simply disable the lint.
                #[allow(clippy::arc_with_non_send_sync)]
                (adapter, Arc::new(device), Arc::new(queue))
            }
            WgpuSetup::Existing {
                instance: _,
                adapter,
                device,
                queue,
            } => (adapter, device, queue),
        };

        let capabilities = {
            profiling::scope!("get_capabilities");
            surface.get_capabilities(&adapter).formats
        };
        let target_format = crate::preferred_framebuffer_format(&capabilities)?;

        let renderer = Renderer::new(
            &device,
            target_format,
            depth_format,
            msaa_samples,
            dithering,
        );

        // On wasm, depending on feature flags, wgpu objects may or may not implement sync.
        // It doesn't make sense to switch to Rc for that special usecase, so simply disable the lint.
        #[allow(clippy::arc_with_non_send_sync)]
        Ok(Self {
            adapter,
            #[cfg(not(target_arch = "wasm32"))]
            available_adapters: available_adapters.into(),
            device,
            queue,
            target_format,
            renderer: Arc::new(RwLock::new(renderer)),
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn describe_adapters(adapters: &[Arc<wgpu::Adapter>]) -> String {
    if adapters.is_empty() {
        "(none)".to_owned()
    } else if adapters.len() == 1 {
        adapter_info_summary(&adapters[0].get_info())
    } else {
        let mut list_string = String::new();
        for adapter in adapters {
            if !list_string.is_empty() {
                list_string += ", ";
            }
            list_string += &format!("{{{}}}", adapter_info_summary(&adapter.get_info()));
        }
        list_string
    }
}

/// Specifies which action should be taken as consequence of a [`wgpu::SurfaceError`]
pub enum SurfaceErrorAction {
    /// Do nothing and skip the current frame.
    SkipFrame,

    /// Instructs egui to recreate the surface, then skip the current frame.
    RecreateSurface,
}

#[derive(Clone)]
pub enum WgpuSetup {
    /// Construct a wgpu setup using some predefined settings & heuristics.
    /// This is the default option. You can customize most behaviours overriding the
    /// supported backends, power preferences, and device description.
    ///
    /// By default can also be configured with the environment variables:
    /// * `WGPU_BACKEND`: `vulkan`, `dx11`, `dx12`, `metal`, `opengl`, `webgpu`
    /// * `WGPU_POWER_PREF`: `low`, `high` or `none`
    /// * `WGPU_TRACE`: Path to a file to output a wgpu trace file.
    CreateNew(WgpuSetupCreateNew),

    /// Run on an existing wgpu setup.
    Existing {
        // TODO(wgpu#6665): Remove layer of `Arc` here once we update to wgpu 24
        instance: Arc<Instance>,
        adapter: Arc<Adapter>,
        device: Arc<Device>,
        queue: Arc<Queue>,
    },
}

impl Default for WgpuSetup {
    fn default() -> Self {
        Self::CreateNew(WgpuSetupCreateNew::default())
    }
}

impl std::fmt::Debug for WgpuSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateNew(create_new) => f
                .debug_tuple("WgpuSetup::CreateNew")
                .field(create_new)
                .finish(),
            Self::Existing { .. } => f
                .debug_struct("WgpuSetup::Existing")
                .finish_non_exhaustive(),
        }
    }
}

/// Method for selecting an adapter on native.
///
/// This can be used for fully custom adapter selection.
/// If available, `wgpu::Surface` is passed to allow checking for surface compatibility.
// TODO(wgpu#6665): Remove layer of `Arc` here.
pub type NativeAdapterSelectorMethod = Arc<
    dyn Fn(&[Arc<wgpu::Adapter>], Option<&wgpu::Surface<'_>>) -> Result<Arc<wgpu::Adapter>, String>
        + Send
        + Sync,
>;
/// Configuration for creating a new wgpu setup.
///
/// Used for [`WgpuSetup::CreateNew`].
#[derive(Clone)]
pub struct WgpuSetupCreateNew {
    /// Backends that should be supported (wgpu will pick one of these).
    ///
    /// For instance, if you only want to support WebGL (and not WebGPU),
    /// you can set this to [`wgpu::Backends::GL`].
    ///
    /// By default on web, WebGPU will be used if available.
    /// WebGL will only be used as a fallback,
    /// and only if you have enabled the `webgl` feature of crate `wgpu`.
    pub supported_backends: wgpu::Backends,

    /// Power preference for the adapter.
    pub power_preference: wgpu::PowerPreference,

    /// Optional selector for native adapters.
    ///
    /// This field has no effect when targeting web!
    /// Otherwise, if set [`WgpuSetup::CreateNew::power_preference`] is ignored and the
    /// adapter is instead selected by this method.
    /// Note that [`WgpuSetup::CreateNew::supported_backends`] is still used to
    /// filter the adapter enumeration in the first place.
    ///
    /// Defaults to `None`.
    pub native_adapter_selector: Option<NativeAdapterSelectorMethod>,

    /// Configuration passed on device request, given an adapter
    pub device_descriptor:
        Arc<dyn Fn(&wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> + Send + Sync>,

    /// Option path to output a wgpu trace file.
    ///
    /// This only works if this feature is enabled in `wgpu-core`.
    /// Does not work when running with WebGPU.
    /// Defaults to the path set in the `WGPU_TRACE` environment variable.
    pub trace_path: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for WgpuSetupCreateNew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgpuSetupCreateNew")
            .field("supported_backends", &self.supported_backends)
            .field("power_preference", &self.power_preference)
            .field(
                "native_adapter_selector",
                &self.native_adapter_selector.is_some(),
            )
            .field("trace_path", &self.trace_path)
            .finish()
    }
}

impl Default for WgpuSetupCreateNew {
    fn default() -> Self {
        Self {
            // Add GL backend, primarily because WebGPU is not stable enough yet.
            // (note however, that the GL backend needs to be opted-in via the wgpu feature flag "webgl")
            supported_backends: wgpu::util::backend_bits_from_env()
                .unwrap_or(wgpu::Backends::PRIMARY | wgpu::Backends::GL),

            power_preference: wgpu::util::power_preference_from_env()
                .unwrap_or(wgpu::PowerPreference::HighPerformance),

            native_adapter_selector: None,

            device_descriptor: Arc::new(|adapter| {
                let base_limits = if adapter.get_info().backend == wgpu::Backend::Gl {
                    wgpu::Limits::downlevel_webgl2_defaults()
                } else {
                    wgpu::Limits::default()
                };

                wgpu::DeviceDescriptor {
                    label: Some("egui wgpu device"),
                    required_features: wgpu::Features::default(),
                    required_limits: wgpu::Limits {
                        // When using a depth buffer, we have to be able to create a texture
                        // large enough for the entire surface, and we want to support 4k+ displays.
                        max_texture_dimension_2d: 8192,
                        ..base_limits
                    },
                    memory_hints: wgpu::MemoryHints::default(),
                }
            }),

            trace_path: std::env::var("WGPU_TRACE")
                .ok()
                .map(std::path::PathBuf::from),
        }
    }
}

/// Configuration for using wgpu with eframe or the egui-wgpu winit feature.
#[derive(Clone)]
pub struct WgpuConfiguration {
    /// Present mode used for the primary surface.
    pub present_mode: wgpu::PresentMode,

    /// Desired maximum number of frames that the presentation engine should queue in advance.
    ///
    /// Use `1` for low-latency, and `2` for high-throughput.
    ///
    /// See [`wgpu::SurfaceConfiguration::desired_maximum_frame_latency`] for details.
    ///
    /// `None` = `wgpu` default.
    pub desired_maximum_frame_latency: Option<u32>,

    /// How to create the wgpu adapter & device
    pub wgpu_setup: WgpuSetup,

    /// Callback for surface errors.
    pub on_surface_error: Arc<dyn Fn(wgpu::SurfaceError) -> SurfaceErrorAction + Send + Sync>,
}

#[test]
fn wgpu_config_impl_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<WgpuConfiguration>();
}

impl std::fmt::Debug for WgpuConfiguration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            present_mode,
            desired_maximum_frame_latency,
            wgpu_setup,
            on_surface_error: _,
        } = self;
        f.debug_struct("WgpuConfiguration")
            .field("present_mode", &present_mode)
            .field(
                "desired_maximum_frame_latency",
                &desired_maximum_frame_latency,
            )
            .field("wgpu_setup", &wgpu_setup)
            .finish_non_exhaustive()
    }
}

impl Default for WgpuConfiguration {
    fn default() -> Self {
        Self {
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: None,
            wgpu_setup: Default::default(),
            on_surface_error: Arc::new(|err| {
                if err == wgpu::SurfaceError::Outdated {
                    // This error occurs when the app is minimized on Windows.
                    // Silently return here to prevent spamming the console with:
                    // "The underlying surface has changed, and therefore the swap chain must be updated"
                } else {
                    log::warn!("Dropped frame with error: {err}");
                }
                SurfaceErrorAction::SkipFrame
            }),
        }
    }
}

/// Find the framebuffer format that egui prefers
///
/// # Errors
/// Returns [`WgpuError::NoSurfaceFormatsAvailable`] if the given list of formats is empty.
pub fn preferred_framebuffer_format(
    formats: &[wgpu::TextureFormat],
) -> Result<wgpu::TextureFormat, WgpuError> {
    for &format in formats {
        if matches!(
            format,
            wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm
        ) {
            return Ok(format);
        }
    }

    formats
        .first()
        .copied()
        .ok_or(WgpuError::NoSurfaceFormatsAvailable)
}

/// Take's epi's depth/stencil bits and returns the corresponding wgpu format.
pub fn depth_format_from_bits(depth_buffer: u8, stencil_buffer: u8) -> Option<wgpu::TextureFormat> {
    match (depth_buffer, stencil_buffer) {
        (0, 8) => Some(wgpu::TextureFormat::Stencil8),
        (16, 0) => Some(wgpu::TextureFormat::Depth16Unorm),
        (24, 0) => Some(wgpu::TextureFormat::Depth24Plus),
        (24, 8) => Some(wgpu::TextureFormat::Depth24PlusStencil8),
        (32, 0) => Some(wgpu::TextureFormat::Depth32Float),
        (32, 8) => Some(wgpu::TextureFormat::Depth32FloatStencil8),
        _ => None,
    }
}

// ---------------------------------------------------------------------------

/// A human-readable summary about an adapter
pub fn adapter_info_summary(info: &wgpu::AdapterInfo) -> String {
    let wgpu::AdapterInfo {
        name,
        vendor,
        device,
        device_type,
        driver,
        driver_info,
        backend,
    } = &info;

    // Example values:
    // > name: "llvmpipe (LLVM 16.0.6, 256 bits)", device_type: Cpu, backend: Vulkan, driver: "llvmpipe", driver_info: "Mesa 23.1.6-arch1.4 (LLVM 16.0.6)"
    // > name: "Apple M1 Pro", device_type: IntegratedGpu, backend: Metal, driver: "", driver_info: ""
    // > name: "ANGLE (Apple, Apple M1 Pro, OpenGL 4.1)", device_type: IntegratedGpu, backend: Gl, driver: "", driver_info: ""

    let mut summary = format!("backend: {backend:?}, device_type: {device_type:?}");

    if !name.is_empty() {
        summary += &format!(", name: {name:?}");
    }
    if !driver.is_empty() {
        summary += &format!(", driver: {driver:?}");
    }
    if !driver_info.is_empty() {
        summary += &format!(", driver_info: {driver_info:?}");
    }
    if *vendor != 0 {
        // TODO(emilk): decode using https://github.com/gfx-rs/wgpu/blob/767ac03245ee937d3dc552edc13fe7ab0a860eec/wgpu-hal/src/auxil/mod.rs#L7
        summary += &format!(", vendor: 0x{vendor:04X}");
    }
    if *device != 0 {
        summary += &format!(", device: 0x{device:02X}");
    }

    summary
}
