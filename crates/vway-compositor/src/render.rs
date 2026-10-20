//! Rendering: on demand, a full render of the space into an offscreen buffer, then PNG.
//!
//! Two renderers. **GLES** on the first DRM render node (EGL device platform) when available: it
//! imports any dmabuf the GPU can sample, including tiled modifiers and VRAM that the CPU cannot
//! map (RADV places Vulkan swapchain images there). **Pixman** otherwise (`--gpu software`, no
//! render node, or EGL failing): CPU only, LINEAR dmabufs imported by mapping them.

use std::path::{Path, PathBuf};

use pixman::{FormatCode, Image};
use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::gles::{GlesRenderbuffer, GlesRenderer};
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::{
    Bind, ExportMem, ImportAll, ImportDma, Offscreen, Renderer as _, RendererSuper, Texture,
    TextureMapping,
};
use smithay::desktop::{Space, Window};
use smithay::output::Output;
use smithay::utils::{Buffer, Rectangle, Size};
use tracing::{info, warn};
use vway_proto::{ErrorBody, ErrorCode, err};

use crate::config::GpuMode;
use crate::state::{State, physical_size};

const CLEAR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

pub enum Renderer {
    Gles(Box<GlesRenderer>),
    Pixman(PixmanRenderer),
}

impl Renderer {
    /// Pick a renderer for `gpu`; returns it with the render node it runs on (if any).
    pub fn new(gpu: GpuMode) -> Result<(Renderer, Option<PathBuf>), String> {
        let node = match gpu {
            GpuMode::Auto => first_render_node(),
            GpuMode::Software => None,
        };
        // Debugging/testing override: VWAY_RENDERER=pixman skips GLES.
        let force_pixman = std::env::var("VWAY_RENDERER").is_ok_and(|v| v == "pixman");
        if let Some(node) = node.as_ref().filter(|_| !force_pixman) {
            match gles_on(node) {
                Ok(r) => {
                    info!(node = %node.display(), "renderer: gles");
                    return Ok((Renderer::Gles(Box::new(r)), Some(node.clone())));
                }
                Err(e) => warn!(
                    node = %node.display(),
                    "GLES renderer unavailable ({e}); using pixman. GPU apps whose buffers the CPU \
                     cannot map (e.g. Vulkan on RADV) will fail to present"
                ),
            }
        }
        let r = PixmanRenderer::new().map_err(|e| format!("pixman renderer: {e:?}"))?;
        info!("renderer: pixman");
        Ok((Renderer::Pixman(r), node))
    }

    pub fn name(&self) -> &'static str {
        match self {
            Renderer::Gles(_) => "gles",
            Renderer::Pixman(_) => "pixman",
        }
    }

    pub fn dmabuf_formats(&self) -> FormatSet {
        match self {
            Renderer::Gles(r) => r.dmabuf_formats(),
            Renderer::Pixman(r) => r.dmabuf_formats(),
        }
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> Result<(), String> {
        match self {
            Renderer::Gles(r) => r
                .import_dmabuf(dmabuf, None)
                .map(drop)
                .map_err(|e| format!("{e:?}")),
            Renderer::Pixman(r) => r
                .import_dmabuf(dmabuf, None)
                .map(drop)
                .map_err(|e| format!("{e:?}")),
        }
    }

    /// Free textures of destroyed buffers.
    pub fn cleanup(&mut self) {
        let _ = match self {
            Renderer::Gles(r) => r.cleanup_texture_cache().map_err(drop),
            Renderer::Pixman(r) => r.cleanup_texture_cache().map_err(drop),
        };
    }
}

/// The first DRM render node, for the GLES renderer and dmabuf feedback.
pub fn first_render_node() -> Option<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().next()
}

fn gles_on(node: &Path) -> Result<GlesRenderer, String> {
    let device = EGLDevice::enumerate()
        .map_err(|e| format!("EGL device enumeration: {e}"))?
        .find(|d| d.render_device_path().ok().as_deref() == Some(node))
        .ok_or_else(|| format!("no EGL device for {}", node.display()))?;
    // SAFETY: the device outlives the display (the display keeps what it needs alive).
    let display = unsafe { EGLDisplay::new(device) }.map_err(|e| format!("EGL display: {e}"))?;
    let context = EGLContext::new(&display).map_err(|e| format!("EGL context: {e}"))?;
    // SAFETY: the context is fresh and used only by this renderer on this thread.
    unsafe { GlesRenderer::new(context) }.map_err(|e| format!("GLES renderer: {e}"))
}

pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

/// A full-output frame as tightly packed opaque RGBA rows.
struct Frame {
    width: usize,
    rgba: Vec<u8>,
}

fn draw<R>(
    renderer: &mut R,
    target: &mut <R as RendererSuper>::Framebuffer<'_>,
    output: &Output,
    space: &Space<Window>,
) -> Result<(), String>
where
    R: smithay::backend::renderer::Renderer + ImportAll,
    R::TextureId: Clone + Texture + 'static,
{
    let mut tracker = OutputDamageTracker::from_output(output);
    smithay::desktop::space::render_output::<_, WaylandSurfaceRenderElement<R>, _, _>(
        output,
        renderer,
        target,
        1.0,
        0,
        [space],
        &[],
        &mut tracker,
        CLEAR,
    )
    .map(drop)
    .map_err(|e| format!("render: {e:?}"))
}

fn frame_gles(
    r: &mut GlesRenderer,
    output: &Output,
    space: &Space<Window>,
    (pw, ph): (u32, u32),
) -> Result<Frame, String> {
    let size: Size<i32, Buffer> = (pw as i32, ph as i32).into();
    let mut rb: GlesRenderbuffer = r
        .create_buffer(Fourcc::Abgr8888, size)
        .map_err(|e| format!("offscreen buffer: {e:?}"))?;
    let mut target = r.bind(&mut rb).map_err(|e| format!("bind: {e:?}"))?;
    draw(r, &mut target, output, space)?;
    let mapping = r
        .copy_framebuffer(&target, Rectangle::from_size(size), Fourcc::Abgr8888)
        .map_err(|e| format!("read back: {e:?}"))?;
    drop(target);
    let flipped = mapping.flipped();
    let bytes = r.map_texture(&mapping).map_err(|e| format!("map: {e:?}"))?;
    let row = pw as usize * 4;
    if bytes.len() < row * ph as usize {
        return Err(format!("short read back: {} bytes", bytes.len()));
    }
    let mut rgba = Vec::with_capacity(row * ph as usize);
    for y in 0..ph as usize {
        // GL reads back bottom-up unless the mapping says it is already flipped.
        let src = if flipped { y } else { ph as usize - 1 - y };
        // Abgr8888 is R, G, B, A in memory.
        for px in bytes[src * row..(src + 1) * row].as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[0], px[1], px[2], 0xff]);
        }
    }
    Ok(Frame {
        width: pw as usize,
        rgba,
    })
}

fn frame_pixman(
    r: &mut PixmanRenderer,
    output: &Output,
    space: &Space<Window>,
    (pw, ph): (u32, u32),
) -> Result<Frame, String> {
    let mut image: Image<'static, 'static> =
        Image::new(FormatCode::A8R8G8B8, pw as usize, ph as usize, true)
            .map_err(|e| format!("allocate image: {e:?}"))?;
    {
        let mut target = r.bind(&mut image).map_err(|e| format!("bind: {e:?}"))?;
        draw(r, &mut target, output, space)?;
    }
    let stride = image.stride();
    // SAFETY: the image owns its buffer and outlives this function body.
    let base = unsafe { image.data() } as *const u8;
    if base.is_null() {
        return Err("pixman image has no data".into());
    }
    let mut rgba = Vec::with_capacity(pw as usize * ph as usize * 4);
    for row in 0..ph as usize {
        // SAFETY: row < ph and the image is ph rows of `stride` bytes.
        let row_ptr = unsafe { base.add(row * stride) } as *const u32;
        for col in 0..pw as usize {
            // A8R8G8B8 = 0xAARRGGBB, premultiplied; the clear color makes every pixel opaque.
            let px = unsafe { row_ptr.add(col).read_unaligned() };
            rgba.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, px as u8, 0xff]);
        }
    }
    Ok(Frame {
        width: pw as usize,
        rgba,
    })
}

impl State {
    /// Render the output (optionally cropped to a logical `[x, y, w, h]`) as PNG, resampled to
    /// `size` pixels if given.
    pub fn capture(
        &mut self,
        region: Option<[f64; 4]>,
        size: Option<[u32; 2]>,
    ) -> Result<Capture, ErrorBody> {
        if let Some([w, h]) = size
            && (!(1..=16384).contains(&w) || !(1..=16384).contains(&h))
        {
            return Err(err(
                ErrorCode::OutOfRange,
                format!("screenshot size {w}x{h} out of range (1..=16384)"),
            ));
        }
        let scale = self.cfg.scale;
        let (pw, ph) = physical_size(self.cfg.width, self.cfg.height, scale);
        let (pw, ph) = (pw as u32, ph as u32);
        let (cx, cy, cw, ch) = match region {
            None => (0, 0, pw as usize, ph as usize),
            Some([x, y, w, h]) => {
                if ![x, y, w, h].iter().all(|v| v.is_finite()) || w <= 0.0 || h <= 0.0 {
                    return Err(err(
                        ErrorCode::BadRequest,
                        "region needs positive width and height",
                    ));
                }
                let x0 = (x * scale).round().max(0.0) as i64;
                let y0 = (y * scale).round().max(0.0) as i64;
                let x1 = ((x + w) * scale).round().min(pw as f64) as i64;
                let y1 = ((y + h) * scale).round().min(ph as f64) as i64;
                if x1 <= x0 || y1 <= y0 {
                    return Err(err(
                        ErrorCode::OutOfRange,
                        format!(
                            "region {x},{y} {w}x{h} lies outside the {}x{} output",
                            self.cfg.width, self.cfg.height
                        ),
                    ));
                }
                (
                    x0 as usize,
                    y0 as usize,
                    (x1 - x0) as usize,
                    (y1 - y0) as usize,
                )
            }
        };

        let State {
            renderer,
            space,
            output,
            ..
        } = self;
        let frame = match renderer {
            Renderer::Gles(r) => frame_gles(r, output, space, (pw, ph)),
            Renderer::Pixman(r) => frame_pixman(r, output, space, (pw, ph)),
        }
        .map_err(|e| err(ErrorCode::RenderFailed, e))?;

        let rgba = if (cx, cy, cw, ch) == (0, 0, frame.width, ph as usize) {
            frame.rgba
        } else {
            let mut out = Vec::with_capacity(cw * ch * 4);
            for row in cy..cy + ch {
                let start = (row * frame.width + cx) * 4;
                out.extend_from_slice(&frame.rgba[start..start + cw * 4]);
            }
            out
        };
        let (w, h, rgba) = match size {
            Some([w, h]) if (w as usize, h as usize) != (cw, ch) => {
                (w, h, resample(&rgba, cw, ch, w as usize, h as usize))
            }
            _ => (cw as u32, ch as u32, rgba),
        };
        let png = encode_png(w, h, &rgba)
            .map_err(|e| err(ErrorCode::RenderFailed, format!("png: {e}")))?;
        Ok(Capture {
            width: w,
            height: h,
            png,
        })
    }
}

/// For each destination index, the source indices it covers and their weights (area coverage).
/// Downscaling averages boxes; upscaling repeats source pixels and blends at their edges.
fn coverage(src: usize, dst: usize) -> Vec<Vec<(usize, f32)>> {
    let step = src as f64 / dst as f64;
    (0..dst)
        .map(|i| {
            let (a, b) = (i as f64 * step, (i + 1) as f64 * step);
            let mut taps = Vec::new();
            let mut j = a.floor() as usize;
            while (j as f64) < b && j < src {
                let w = b.min(j as f64 + 1.0) - a.max(j as f64);
                if w > 0.0 {
                    taps.push((j, (w / (b - a)) as f32));
                }
                j += 1;
            }
            taps
        })
        .collect()
}

/// Resample tightly packed RGBA from `sw`×`sh` to `dw`×`dh` (separable area filter).
pub fn resample(rgba: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let (cols, rows) = (coverage(sw, dw), coverage(sh, dh));
    let mut tmp = vec![0f32; sh * dw * 4];
    for y in 0..sh {
        let src = &rgba[y * sw * 4..(y + 1) * sw * 4];
        for (x, taps) in cols.iter().enumerate() {
            let out = &mut tmp[(y * dw + x) * 4..(y * dw + x + 1) * 4];
            for &(j, w) in taps {
                for c in 0..4 {
                    out[c] += src[j * 4 + c] as f32 * w;
                }
            }
        }
    }
    let mut out = vec![0u8; dw * dh * 4];
    for (y, taps) in rows.iter().enumerate() {
        for x in 0..dw {
            for c in 0..4 {
                let v: f32 = taps
                    .iter()
                    .map(|&(j, w)| tmp[(j * dw + x) * 4 + c] * w)
                    .sum();
                out[(y * dw + x) * 4 + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, png::EncodingError> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut w = enc.write_header()?;
        w.write_image_data(rgba)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_averages_and_repeats() {
        // 4x2: left half black, right half white → 2x1 keeps the halves.
        let px = |v: u8| [v, v, v, 255];
        let src: Vec<u8> = [0, 0, 255, 255, 0, 0, 255, 255]
            .iter()
            .flat_map(|&v| px(v))
            .collect();
        assert_eq!(resample(&src, 4, 2, 2, 1), [px(0), px(255)].concat());
        // 3 → 2 columns: the middle pixel is split between both outputs.
        let src: Vec<u8> = [0, 90, 180].iter().flat_map(|&v| px(v)).collect();
        assert_eq!(resample(&src, 3, 1, 2, 1), [px(30), px(150)].concat());
        // 1x1 → 2x2 repeats.
        assert_eq!(resample(&px(7), 1, 1, 2, 2), [px(7); 4].concat());
    }
}
