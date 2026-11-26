//! canvas node model: images and frames.

use bevy::prelude::*;
use bevy::tasks::Task;
use std::path::PathBuf;

use crate::project::oplog::NodeId;

/// larger images are scaled down on first place, world units
pub const MAX_DISPLAY_DIM: f32 = 512.0;

#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub struct TabOwner(pub u64);

/// parked tabs skip canvas work without losing their entity ids
#[derive(Component)]
pub struct InactiveTab;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub center: Vec2,
    pub size: Vec2,
    pub rot: f32,
}

/// state undo/redo restores
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NodeSnapshot {
    pub pose: Pose,
    pub z: f32,
    pub flip_x: bool,
    pub flip_y: bool,
    /// image crop in natural texels (unused for frames)
    pub crop: Rect,
    /// world units per source texel (unused for frames)
    pub px_scale: f32,
}

#[derive(Component)]
pub struct CanvasNode {
    /// stable oplog id; `None` until first persisted
    pub node_id: Option<NodeId>,
    pub kind: NodeKind,
    /// displayed size, world units
    pub size: Vec2,
    /// z rotation, radians
    pub rotation: f32,
}

pub enum NodeKind {
    Image(ImageNode),
    Frame(FrameNode),
}

/// labeled container; dragging moves its images
pub struct FrameNode {
    pub label: String,
}

#[derive(Component)]
pub struct FrameLabel(pub Entity);

/// cheap to clone: the texture handle is shared
#[derive(Clone)]
pub struct ImageData {
    /// path relative to the project root, e.g. `images/1a2b3c4d.png`
    pub rel_path: String,
    /// resolved absolute path; used for loading, recomputed per open
    pub abs_path: PathBuf,
    /// original file name, for tooltips/relinking
    pub orig_name: String,
    pub hash: String,
    pub natural: UVec2,
    pub preview: Handle<Image>,
    pub preview_size: UVec2,
    /// only a transparent placeholder is bound: preview not loaded and no
    /// full-res yet; the canvas draws a dim boundary so it isn't invisible.
    pub preview_pending: bool,
    /// visible region, natural texels (origin top-left). resizing keeps a uniform
    /// scale ("cover"), so pixels never stretch and an explicit crop survives.
    pub crop: Rect,
    /// world units per source texel (uniform)
    pub px_scale: f32,
}

impl ImageData {
    /// displayed size, world units
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

    /// crop in natural texels → the bound tier's pixel space; previews are
    /// downscaled, and an unscaled crop would sample outside the texture.
    pub fn tier_rect_of(crop: Rect, natural: UVec2, preview_size: UVec2, full_active: bool) -> Rect {
        let natural = natural.as_vec2().max(Vec2::ONE);
        let preview = preview_size.as_vec2().max(Vec2::ONE);
        if full_active || preview == natural {
            return crop;
        }
        let s = preview / natural;
        Rect::from_corners(crop.min * s, crop.max * s)
    }

    /// [`ImageData::tier_rect_of`] for the current crop
    pub fn tier_rect(&self, full_active: bool) -> Rect {
        Self::tier_rect_of(self.crop, self.natural, self.preview_size, full_active)
    }

    /// guessed aspect-preserving preview size before the real one arrives; must
    /// match `import::PREVIEW_MAX`.
    pub fn preview_size_for(natural: UVec2) -> UVec2 {
        let natural_f = natural.as_vec2().max(Vec2::ONE);
        let fit = (MAX_DISPLAY_DIM / natural_f.max_element()).min(1.0);
        (natural_f * fit).max(Vec2::ONE).as_uvec2()
    }
}

pub struct ImageNode {
    pub data: ImageData,
    /// full-res texture, once streamed in
    pub full: Option<Handle<Image>>,
    /// decoded full-res texture waiting for its `GpuImage`; binding an unprepared
    /// image flashes a placeholder. promoted to `full` by lod binding.
    pub full_incoming: Option<Handle<Image>>,
    /// full-res uses nearest sampling (pixel-peeping at deep zoom)
    pub sampler_nearest: bool,
    /// cpu data still on the full-res asset; freed once the render world has the
    /// texture. keeping it in the main world is what allows runtime sampler flips.
    pub pending_cpu_free: Option<Handle<Image>>,
    /// approx vram bytes of the resident full-res texture
    pub resident_bytes: u64,
    /// seconds since startup when the node last wanted full-res (visible and
    /// magnified). drives proactive release; unstamped nodes free after a grace window.
    pub last_needed: f64,
    /// material this node owns; the sprite material cache bakes in the sampler,
    /// so peeping swaps in a fresh one (see `lod::update_texture_samplers`).
    pub sprite_material: Option<Handle<bevy::sprite_render::SpriteMeshMaterial>>,
}

/// in-flight decode; `Err` carries the read/decode reason for lod reporting
#[derive(Component)]
pub struct FullLoadTask(pub Task<Result<Image, String>>);

impl ImageNode {
    /// sprite rect for the bound tier: natural crop for full-res, downscaled
    /// for previews.
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

/// single-level only: the cpu mip upload path renders blurry on this setup, so
/// minification comes from the tier system instead.
pub fn to_bevy_image(dyn_img: image::DynamicImage) -> Image {
    use bevy::asset::RenderAssetUsages;
    to_bevy_image_with_usage(dyn_img, RenderAssetUsages::RENDER_WORLD)
}

/// [`to_bevy_image`] with explicit asset usage
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
