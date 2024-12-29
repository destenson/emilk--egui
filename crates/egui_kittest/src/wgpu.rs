use crate::texture_to_image::texture_to_image;
use egui::TexturesDelta;
use egui_wgpu::wgpu::{Backends, StoreOp, TextureFormat};
use egui_wgpu::{wgpu, RenderState, ScreenDescriptor, WgpuSetup};
use image::RgbaImage;
use std::iter::once;
use std::sync::Arc;
use wgpu::Maintain;

// TODO(#5506): Replace this with the setup from https://github.com/emilk/egui/pull/5506
pub fn default_wgpu_setup() -> egui_wgpu::WgpuSetup {
    egui_wgpu::WgpuSetup::CreateNew {
        supported_backends: Backends::all(),
        device_descriptor: Arc::new(|_| wgpu::DeviceDescriptor::default()),
        power_preference: wgpu::PowerPreference::default(),
    }
}

pub(crate) fn create_render_state(setup: WgpuSetup) -> egui_wgpu::RenderState {
    let instance = match &setup {
        WgpuSetup::Existing { instance, .. } => instance.clone(),
        WgpuSetup::CreateNew { .. } => Default::default(),
    };

    pollster::block_on(egui_wgpu::RenderState::create(
        &egui_wgpu::WgpuConfiguration {
            wgpu_setup: setup,
            ..Default::default()
        },
        &instance,
        None,
        None,
        1,
        false,
    ))
        .expect("Failed to create render state")
}

/// Utility to render snapshots from a [`crate::Harness`] using [`egui_wgpu`].
pub struct WgpuTestRenderer {
    render_state: RenderState,
}

impl Default for WgpuTestRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl WgpuTestRenderer {
    /// Create a new [`WgpuTestRenderer`] with the default setup.
    pub fn new() -> Self {
        Self {
            render_state: create_render_state(default_wgpu_setup()),
        }
    }

    /// Create a new [`WgpuTestRenderer`] with the given setup.
    pub fn from_setup(setup: WgpuSetup) -> Self {
        Self {
            render_state: create_render_state(setup),
        }
    }
}

impl crate::TestRenderer for WgpuTestRenderer {
    #[cfg(feature = "eframe")]
    fn setup_eframe(&self, cc: &mut eframe::CreationContext<'_>, frame: &mut eframe::Frame) {
        cc.wgpu_render_state = Some(self.render_state.clone());
        frame.wgpu_render_state = Some(self.render_state.clone());
    }

    fn handle_delta(&mut self, delta: &TexturesDelta) {
        let mut renderer = self.render_state.renderer.write();
        for (id, image) in &delta.set {
            renderer.update_texture(
                &self.render_state.device,
                &self.render_state.queue,
                *id,
                image,
            );
        }
    }

    /// Render the [`crate::Harness`] and return the resulting image.
    fn render(
        &mut self,
        ctx: &egui::Context,
        output: &egui::FullOutput,
    ) -> Result<RgbaImage, String> {
        let mut renderer = self.render_state.renderer.write();

        let mut encoder =
            self.render_state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("Egui Command Encoder"),
                });

        let size = ctx.screen_rect().size() * ctx.pixels_per_point();
        let screen = ScreenDescriptor {
            pixels_per_point: ctx.pixels_per_point(),
            size_in_pixels: [size.x.round() as u32, size.y.round() as u32],
        };

        let tessellated = ctx.tessellate(output.shapes.clone(), ctx.pixels_per_point());

        let user_buffers = renderer.update_buffers(
            &self.render_state.device,
            &self.render_state.queue,
            &mut encoder,
            &tessellated,
            &screen,
        );

        let texture = self
            .render_state
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("Egui Texture"),
                size: wgpu::Extent3d {
                    width: screen.size_in_pixels[0],
                    height: screen.size_in_pixels[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });

        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("Egui Render Pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &texture_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                })
                .forget_lifetime();

            renderer.render(&mut pass, &tessellated, &screen);
        }

        self.render_state
            .queue
            .submit(user_buffers.into_iter().chain(once(encoder.finish())));

        self.render_state.device.poll(Maintain::Wait);

        Ok(texture_to_image(
            &self.render_state.device,
            &self.render_state.queue,
            &texture,
        ))
    }
}
