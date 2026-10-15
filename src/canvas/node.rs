//! Canvas node model: images and frames.

use bevy::prelude::*;
use bevy::tasks::Task;
use std::path::PathBuf;

/// Images larger than this are scaled down when first placed (world units).
pub const MAX_DISPLAY_DIM: f32 = 512.0;

/// Which open project (tab) a node belongs to.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub struct TabOwner(pub u64);

/// Excludes parked tabs from canvas work without losing their entity IDs.
#[derive(Component)]
pub struct InactiveTab;

/// A placement/transform snapshot (center, size, z-rotation in radians).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub center: Vec2,
    pub size: Vec2,
    pub rot: f32,
}

/// Everything about a node's state that undo/redo restores.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NodeSnapshot {
    pub pose: Pose,
    pub z: f32,
    pub flip_x: bool,
    pub flip_y: bool,
    /// Image crop, in natural texel coordinates (unused for frames).
    pub crop: Rect,
    /// World units per source texel (unused for frames).
    pub px_scale: f32,
}

#[derive(Component)]
pub struct CanvasNode {
    /// Row id in the project db; `None` until first persisted.
    pub db_id: Option<i64>,
    pub kind: NodeKind,
    /// Displayed size in world units.
    pub size: Vec2,
    /// Z rotation in radians.
    pub rotation: f32,
}

pub enum NodeKind {
    Image(ImageNode),
    Frame(FrameNode),
}

/// A labeled container. Dragging it moves the image nodes it contains.
pub struct FrameNode {
    pub label: String,
}

/// Marks the Text2d child that displays a frame's label.
#[derive(Component)]
pub struct FrameLabel(pub Entity);

/// Everything needed to draw / re-place an image node. Cheap to clone when it
/// holds no texture (import) or shares the texture handle (copy/paste).
#[derive(Clone)]
pub struct ImageData {
    /// Path relative to the project root, e.g. `images/1a2b3c4d.png`.
    pub rel_path: String,
    /// Resolved absolute path (used for loading; recomputed per open).
    pub abs_path: PathBuf,
    /// Original file name, for tooltips / relinking.
    pub orig_name: String,
    /// Short content hash (hex).
    pub hash: String,
    /// Natural pixel size.
    pub natural: UVec2,
    /// Preview texture handle.
    pub preview: Handle<Image>,
    /// Preview pixel size.
    pub preview_size: UVec2,
    /// True while only the transparent restore placeholder is bound: the real
    /// preview hasn't been read from cache / regenerated yet, and no full-res
    /// texture has arrived. The canvas draws a dim boundary around such nodes
    /// so they aren't invisible while waiting.
    pub preview_pending: bool,
    /// Visible portion of the image, in natural texel coordinates
    /// (texture space: origin top-left). Resizing handles keep the picture at
    /// a uniform scale ("fill/cover", like Figma): the container resizes and
    /// the crop follows, so pixels are never stretched.
    pub crop: Rect,
    /// Display scale: world units per source texel (uniform).
    pub px_scale: f32,
}

impl ImageData {
    /// Displayed size in world units (what the transform/handles use).
    pub fn display_size(&self) -> Vec2 {
        self.crop.size() * self.px_scale
    }

    pub fn full_crop(natural: UVec2) -> Rect {
        Rect::from_corners(Vec2::ZERO, natural.as_vec2().max(Vec2::ONE))
    }

    pub fn default_px_scale(natural: UVec2) -> f32 {
        let natural = natural.as_vec2().max(Vec2::ONE);
        (MAX_DISPLAY_DIM / natural.max_element()).min(1.0)
    }

    /// A crop (in natural texels) expressed in the pixel space of a texture
    /// tier. Full-res textures are natural-sized, so the crop is used as-is;
    /// preview textures are downscaled (see `preview_size`), so the crop must
    /// be scaled to match, otherwise the sprite samples outside the texture
    /// and shows a stretched sliver of the image.
    pub fn tier_rect_of(crop: Rect, natural: UVec2, preview_size: UVec2, full_active: bool) -> Rect {
        let natural = natural.as_vec2().max(Vec2::ONE);
        let preview = preview_size.as_vec2().max(Vec2::ONE);
        if full_active || preview == natural {
            return crop;
        }
        let s = preview / natural;
        Rect::from_corners(crop.min * s, crop.max * s)
    }

    /// See [`ImageData::tier_rect_of`], for this image's current crop.
    pub fn tier_rect(&self, full_active: bool) -> Rect {
        Self::tier_rect_of(self.crop, self.natural, self.preview_size, full_active)
    }

    /// The aspect-preserving preview size for a natural size (a guess used
    /// before the real preview arrives; see `pump_preview_loads`). Must match
    /// `import::PREVIEW_MAX`.
    pub fn preview_size_for(natural: UVec2) -> UVec2 {
        let natural_f = natural.as_vec2().max(Vec2::ONE);
        let fit = (MAX_DISPLAY_DIM / natural_f.max_element()).min(1.0);
        (natural_f * fit).max(Vec2::ONE).as_uvec2()
    }
}

pub struct ImageNode {
    pub data: ImageData,
    /// Full-resolution texture, once streamed in.
    pub full: Option<Handle<Image>>,
    /// A decoded full-res texture waiting for its `GpuImage` before binding.
    /// Binding an unprepared `Image` leaves the sprite with a placeholder
    /// material slot for a frame (see `lod::GpuTextureState`). Promoted to
    /// `full` by `lod::bind_pending_full_textures`.
    pub full_incoming: Option<Handle<Image>>,
    /// Whether the full-res texture currently uses nearest sampling
    /// (pixel-peeping at deep zoom).
    pub sampler_nearest: bool,
    /// CPU pixel data still attached to the full-res `Image` asset: freed once
    /// the render world has the texture on the GPU (see
    /// `lod::GpuTextureState`). Keeping the asset in the main world is what
    /// makes runtime sampler flips work.
    pub pending_cpu_free: Option<Handle<Image>>,
    /// Approximate VRAM bytes of the resident full-res texture.
    pub resident_bytes: u64,
    /// Wall-clock time (seconds since startup) at which this node last wanted
    /// its full-res texture, i.e. it was visible and magnified enough to
    /// exceed the preview. Drives proactive release in
    /// `lod::evict_full_textures`; a node that stops being stamped (panned
    /// away, zoomed out, or on an inactive tab) is freed after a grace window.
    pub last_needed: f64,
    /// The `SpriteMeshMaterial` this node currently owns. Bevy's sprite
    /// material cache bakes the image sampler into a material when it is
    /// prepared, so peeping swaps in a fresh material of our own (see
    /// `lod::update_texture_samplers`).
    pub sprite_material: Option<Handle<bevy::sprite_render::SpriteMeshMaterial>>,
}

/// In-flight full-resolution decode task. `Err` carries the read/decode
/// reason so `lod::collect_full_textures` can report failures.
#[derive(Component)]
pub struct FullLoadTask(pub Task<Result<Image, String>>);

impl ImageNode {
    /// The sprite rect for whichever texture tier is currently bound: the
    /// natural-space crop for full-res, the downscaled crop for previews.
    pub fn active_rect(&self) -> Rect {
        self.data.tier_rect(self.full.is_some())
    }
}

impl NodeKind {
    pub fn image(&self) -> Option<&ImageNode> {
        match self {
            NodeKind::Image(img) => Some(img),
            NodeKind::Frame(_) => None,
        }
    }

    pub fn image_mut(&mut self) -> Option<&mut ImageNode> {
        match self {
            NodeKind::Image(img) => Some(img),
            NodeKind::Frame(_) => None,
        }
    }

    pub fn is_frame(&self) -> bool {
        matches!(self, NodeKind::Frame(_))
    }

    #[allow(dead_code)]
    pub fn label(&self) -> Option<&str> {
        match self {
            NodeKind::Frame(frame) => Some(&frame.label),
            NodeKind::Image(_) => None,
        }
    }
}

impl CanvasNode {
    pub fn image(&self) -> Option<&ImageNode> {
        self.kind.image()
    }

    pub fn image_mut(&mut self) -> Option<&mut ImageNode> {
        self.kind.image_mut()
    }
}

/// Bevy's CPU mip upload path (`mip_level_count > 1` via
/// `create_texture_with_data`) renders incorrectly blurry on this setup, so
/// images are single-level; minification quality is handled by the tier system
/// (pre-antialiased previews + on-demand full-res) instead of mips.
pub fn to_bevy_image(dyn_img: image::DynamicImage) -> Image {
    use bevy::asset::RenderAssetUsages;
    to_bevy_image_with_usage(dyn_img, RenderAssetUsages::RENDER_WORLD)
}

/// Same as [`to_bevy_image`], with explicit asset usage.
pub fn to_bevy_image_with_usage(
    dyn_img: image::DynamicImage,
    usage: bevy::asset::RenderAssetUsages,
) -> Image {
    use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

    let rgba = dyn_img.into_rgba8();
    let (w, h) = rgba.dimensions();
    Image::new(
        Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba.into_raw(),
        TextureFormat::Rgba8UnormSrgb,
        usage,
    )
}
