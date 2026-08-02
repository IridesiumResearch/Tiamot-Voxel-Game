// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Rendering to a texture and reading it back.
//!
//! # This is what makes the renderer testable at all
//!
//! Everything visual in Task 08 is otherwise a human gate. An offscreen target
//! turns "did rendering silently break" into an assertion CI can make on a
//! machine with no display, no window manager, and a software Vulkan driver.
//!
//! It draws through the same [`Renderer`](super::Renderer) a window does — the
//! renderer takes a [`wgpu::TextureView`] and does not know where it came from.
//! A screenshot test against a separate "headless renderer" would be a test of
//! the headless renderer.
//!
//! # Perceptual hashing, not pixel equality
//!
//! Two drivers will not produce identical pixels: rasterisation rules,
//! filtering, and floating-point differences all move colours by a little. So
//! the frame is reduced to a coarse grid of averaged cells before it is hashed
//! — a gate that catches "the world stopped drawing" or "everything turned
//! magenta" and deliberately does not catch a one-bit difference in a texel.

use crate::camera::Camera;

use super::{COLOUR_FORMAT, Gpu, RenderError, Renderer};

/// Bytes per row must be a multiple of this for a texture-to-buffer copy.
const COPY_ALIGNMENT: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

/// Cells per axis in the coarse hash. See the module docs.
///
/// Sixteen is enough to notice a missing chunk or an inverted horizon and
/// coarse enough that no amount of filtering difference moves a cell.
pub const HASH_GRID: u32 = 16;

/// A colour target with a readback path.
pub struct Offscreen {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl Offscreen {
    /// Creates a target of the given size.
    #[must_use]
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: COLOUR_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            width,
            height,
        }
    }

    /// The view to render into.
    #[must_use]
    pub const fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Its dimensions.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Renders one frame and reads it back.
    ///
    /// # Errors
    ///
    /// [`RenderError::Readback`] if the copy buffer cannot be mapped.
    pub fn capture(
        &self,
        renderer: &mut Renderer,
        camera: &Camera,
    ) -> Result<crate::texture::Image, RenderError> {
        renderer.render(&self.view, camera, (self.width, self.height));
        self.read_back(renderer.gpu())
    }

    /// Copies the texture into host memory as RGBA8.
    ///
    /// # Errors
    ///
    /// [`RenderError::Readback`] if the buffer cannot be mapped.
    pub fn read_back(&self, gpu: &Gpu) -> Result<crate::texture::Image, RenderError> {
        // A texture-to-buffer copy needs each row padded to the alignment. The
        // padding is real bytes in the buffer and has to be skipped when the
        // rows are reassembled — forgetting to is a readback that looks correct
        // at some widths and sheared at others.
        let unpadded = self.width * 4;
        let padded = unpadded.div_ceil(COPY_ALIGNMENT) * COPY_ALIGNMENT;

        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(padded) * u64::from(self.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit(Some(encoder.finish()));

        let (sender, receiver) = std::sync::mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| RenderError::Readback(err.to_string()))?;
        receiver
            .recv()
            .map_err(|err| RenderError::Readback(err.to_string()))?
            .map_err(|err| RenderError::Readback(err.to_string()))?;

        let mapped = buffer.slice(..).get_mapped_range();
        let mut rgba = Vec::with_capacity((self.width * self.height * 4) as usize);
        for row in 0..self.height {
            let start = (row * padded) as usize;
            rgba.extend_from_slice(&mapped[start..start + unpadded as usize]);
        }
        drop(mapped);
        buffer.unmap();

        Ok(crate::texture::Image {
            width: self.width,
            height: self.height,
            rgba,
        })
    }
}

/// A coarse, driver-tolerant hash of a frame.
///
/// The image is reduced to [`HASH_GRID`] squared averaged cells, each channel
/// quantised to 4 bits, and hashed. Two drivers rendering the same scene agree;
/// a scene that changed does not.
///
/// Quantising after averaging rather than before is deliberate: averaging first
/// makes a single stray pixel irrelevant, and quantising second means a cell
/// sitting exactly on a boundary is the only place a driver difference can
/// still show — one cell out of 256, and it takes a real visual change to move
/// more than that.
#[must_use]
pub fn perceptual_hash(image: &crate::texture::Image) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tiamot:frame:v1");

    for cell_y in 0..HASH_GRID {
        for cell_x in 0..HASH_GRID {
            let x0 = cell_x * image.width / HASH_GRID;
            let x1 = ((cell_x + 1) * image.width / HASH_GRID).max(x0 + 1);
            let y0 = cell_y * image.height / HASH_GRID;
            let y1 = ((cell_y + 1) * image.height / HASH_GRID).max(y0 + 1);

            let mut total = [0u64; 3];
            let mut samples = 0u64;
            for y in y0..y1.min(image.height) {
                for x in x0..x1.min(image.width) {
                    if let Some(pixel) = image.pixel(x, y) {
                        for channel in 0..3 {
                            total[channel] += u64::from(pixel[channel]);
                        }
                        samples += 1;
                    }
                }
            }
            let samples = samples.max(1);
            for channel in &total {
                // Four bits per channel. Sixteen buckets is far coarser than
                // any driver difference and far finer than "the world stopped
                // drawing".
                hasher.update(&[((channel / samples) as u8) >> 4]);
            }
        }
    }

    *hasher.finalize().as_bytes()
}

/// A perceptual hash as lowercase hex, for goldens and log lines.
#[must_use]
pub fn hash_hex(image: &crate::texture::Image) -> String {
    crate::trust::to_hex(&perceptual_hash(image))
}
