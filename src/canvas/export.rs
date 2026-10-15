//! Flattening canvas nodes into a single image, "copy as image".
//!
//! Ctrl+C writes the result of [`flatten`] to the system clipboard as a PNG:
//! what you see is what you copy (crop, flips and rotation are baked into
//! the pixels), so pasting the same picture into GIMP, a browser, or another
//! refcanvas window reproduces the on-canvas appearance.

use std::path::PathBuf;

use bevy::prelude::*;

use super::hit::aabb_of;
use super::manipulate;
use super::node::Pose;
use super::selection::{NodeQ, Selection};

/// One image node, ready for the (worker-thread) export.
pub struct ExportNode {
    /// Original file to decode (full resolution).
    pub path: PathBuf,
    /// Visible region, natural texels.
    pub crop: Rect,
    pub flip_x: bool,
    pub flip_y: bool,
    /// Displayed placement, world units.
    pub pose: Pose,
    /// World units per source texel.
    pub px_scale: f32,
    /// Stacking order.
    pub z: f32,
}

/// Largest exported edge, pixels.
const MAX_DIM: f32 = 8192.0;
/// Largest exported area, pixels (memory safety valve).
const MAX_PIXELS: f32 = 32_000_000.0;

/// Collect the selected image nodes (frames are skipped), bottom of the
/// stack first so the flattening paints them in the visible order.
pub fn gather(selection: &Selection, q: &NodeQ) -> Vec<ExportNode> {
    let mut out = Vec::new();
    for e in &selection.nodes {
        let Ok((_, tf, sprite, node)) = q.get(*e) else {
            continue;
        };
        let Some(img) = node.image() else {
            continue;
        };
        out.push(ExportNode {
            path: img.data.abs_path.clone(),
            crop: img.data.crop,
            flip_x: sprite.flip_x,
            flip_y: sprite.flip_y,
            pose: manipulate::current_pose(tf, node),
            px_scale: img.data.px_scale,
            z: tf.translation.z,
        });
    }
    out.sort_by(|a, b| a.z.total_cmp(&b.z));
    out
}

/// Flatten `nodes` into one RGBA image (transparent background).
///
/// All nodes share one output scale: the finest node (`px_scale` minimum)
/// keeps its source pixels 1:1; the result is capped to [`MAX_DIM`] and
/// [`MAX_PIXELS`]. Missing/unreadable sources fail the whole export.
pub fn flatten(nodes: &[ExportNode]) -> anyhow::Result<image::RgbaImage> {
    anyhow::ensure!(!nodes.is_empty(), "nothing to export");

    let mut bounds = aabb_of(&nodes[0].pose);
    for n in &nodes[1..] {
        bounds = bounds.union(aabb_of(&n.pose));
    }
    let side = bounds.size().max(Vec2::splat(1e-3));
    let finest = nodes
        .iter()
        .map(|n| n.px_scale)
        .fold(f32::INFINITY, f32::min);
    // World → output pixels, 1:1 for the finest node.
    let mut scale = 1.0 / finest.max(1e-6);
    let max_side = side.x.max(side.y) * scale;
    if max_side > MAX_DIM {
        scale *= MAX_DIM / max_side;
    }
    let area = side.x * side.y * scale * scale;
    if area > MAX_PIXELS {
        scale *= (MAX_PIXELS / area).sqrt();
    }
    // The small epsilon keeps float noise from adding a transparent fringe
    // when the true size is an exact pixel count.
    let w = ((side.x * scale) - 1e-3).max(1.0).ceil() as u32;
    let h = ((side.y * scale) - 1e-3).max(1.0).ceil() as u32;

    let mut out = image::RgbaImage::new(w, h);
    for n in nodes {
        paint(&mut out, n, &bounds, scale)?;
    }
    Ok(out)
}

/// Draw one node into `out`, baking crop, flips and rotation.
fn paint(
    out: &mut image::RgbaImage,
    n: &ExportNode,
    bounds: &Rect,
    scale: f32,
) -> anyhow::Result<()> {
    let src = image::open(&n.path)?.into_rgba8();
    let crop = crop_flip(&src, n.crop, n.flip_x, n.flip_y);
    let (cw, ch) = (crop.width() as f32, crop.height() as f32);

    // Output rows of the (rotated) node's world AABB.
    let quad = aabb_of(&n.pose);
    let x0 = ((quad.min.x - bounds.min.x) * scale).floor().max(0.0) as u32;
    let x1 = (((quad.max.x - bounds.min.x) * scale).ceil() as i64)
        .clamp(0, out.width() as i64) as u32;
    let y0 = ((bounds.max.y - quad.max.y) * scale).floor().max(0.0) as u32;
    let y1 = (((bounds.max.y - quad.min.y) * scale).ceil() as i64)
        .clamp(0, out.height() as i64) as u32;

    let (sin, cos) = n.pose.rot.sin_cos();
    let half = n.pose.size * 0.5;
    let inv_w = 1.0 / n.pose.size.x.max(1e-6);
    let inv_h = 1.0 / n.pose.size.y.max(1e-6);

    for py in y0..y1 {
        // World y of this scanline (row 0 is the top edge, +y is up).
        let wy = bounds.max.y - (py as f32 + 0.5) / scale;
        for px in x0..x1 {
            let wx = bounds.min.x + (px as f32 + 0.5) / scale;
            // World → node-local: undo the rotation, +y up.
            let dx = wx - n.pose.center.x;
            let dy = wy - n.pose.center.y;
            let lx = dx * cos + dy * sin;
            let ly = dy * cos - dx * sin;
            if lx < -half.x || lx > half.x || ly < -half.y || ly > half.y {
                continue;
            }
            // Local → cropped source texels (texture v grows downwards).
            let u = (lx + half.x) * inv_w;
            let v = (half.y - ly) * inv_h;
            let sample = sample_premul(&crop, u * cw - 0.5, v * ch - 0.5);
            blend(out.get_pixel_mut(px, py), sample);
        }
    }
    Ok(())
}

/// The node's visible pixels: crop (natural texels), then flips.
fn crop_flip(src: &image::RgbaImage, crop: Rect, flip_x: bool, flip_y: bool) -> image::RgbaImage {
    let (w, h) = (src.width().max(1), src.height().max(1));
    let x = crop.min.x.round().clamp(0.0, (w - 1) as f32) as u32;
    let y = crop.min.y.round().clamp(0.0, (h - 1) as f32) as u32;
    let cw = crop.width().round().clamp(1.0, (w - x) as f32) as u32;
    let ch = crop.height().round().clamp(1.0, (h - y) as f32) as u32;
    let mut out = image::imageops::crop_imm(src, x, y, cw, ch).to_image();
    if flip_x {
        image::imageops::flip_horizontal_in_place(&mut out);
    }
    if flip_y {
        image::imageops::flip_vertical_in_place(&mut out);
    }
    out
}

/// Source-over into `dst` from a premultiplied RGBA sample (`a` in 0..=1).
fn blend(dst: &mut image::Rgba<u8>, src: [f32; 4]) {
    let sa = src[3];
    if sa <= 0.0 {
        return;
    }
    let d = dst.0;
    let da = d[3] as f32 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        return;
    }
    let inv = 1.0 / out_a;
    let ch = |s: f32, d: u8| ((s + d as f32 / 255.0 * da * (1.0 - sa)) * inv).clamp(0.0, 1.0);
    dst.0 = [
        (ch(src[0], d[0]) * 255.0).round() as u8,
        (ch(src[1], d[1]) * 255.0).round() as u8,
        (ch(src[2], d[2]) * 255.0).round() as u8,
        (out_a * 255.0).round() as u8,
    ];
}

/// Bilinear sample in premultiplied alpha (avoids dark fringes along
/// transparent/rotated edges). Coordinates are in texel space with pixel
/// centers at `i + 0.5`; samples clamp to the image edge.
fn sample_premul(img: &image::RgbaImage, x: f32, y: f32) -> [f32; 4] {
    let (w, h) = (img.width() as i64, img.height() as i64);
    let fx = x.floor();
    let fy = y.floor();
    let (x0, y0) = (fx as i64, fy as i64);
    let (tx, ty) = (x - fx, y - fy);
    let mut acc = [0.0f32; 4];
    for (dx, dy, wgt) in [
        (0, 0, (1.0 - tx) * (1.0 - ty)),
        (1, 0, tx * (1.0 - ty)),
        (0, 1, (1.0 - tx) * ty),
        (1, 1, tx * ty),
    ] {
        if wgt <= 0.0 {
            continue;
        }
        let sx = (x0 + dx).clamp(0, w - 1) as u32;
        let sy = (y0 + dy).clamp(0, h - 1) as u32;
        let p = img.get_pixel(sx, sy).0;
        let a = p[3] as f32 / 255.0;
        acc[0] += p[0] as f32 / 255.0 * a * wgt;
        acc[1] += p[1] as f32 / 255.0 * a * wgt;
        acc[2] += p[2] as f32 / 255.0 * a * wgt;
        acc[3] += a * wgt;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a tiny PNG to the temp dir and build an `ExportNode` for it.
    fn node(img: &image::RgbaImage, pose: Pose, crop: Rect, px_scale: f32) -> (PathBuf, ExportNode) {
        let path = std::env::temp_dir().join(format!(
            "refcanvas-export-{}-{:p}.png",
            std::process::id(),
            img.as_raw()
        ));
        img.save(&path).unwrap();
        let node = ExportNode {
            path: path.clone(),
            crop,
            flip_x: false,
            flip_y: false,
            pose,
            px_scale,
            z: 0.0,
        };
        (path, node)
    }

    fn rgba(r: u8, g: u8, b: u8, a: u8) -> image::Rgba<u8> {
        image::Rgba([r, g, b, a])
    }

    #[test]
    fn uncropped_copy_is_pixel_exact() {
        let mut img = image::RgbaImage::new(3, 2);
        img.put_pixel(0, 0, rgba(255, 0, 0, 255));
        img.put_pixel(1, 0, rgba(0, 255, 0, 255));
        img.put_pixel(2, 0, rgba(0, 0, 255, 255));
        img.put_pixel(0, 1, rgba(10, 20, 30, 128));
        img.put_pixel(1, 1, rgba(40, 50, 60, 255));
        img.put_pixel(2, 1, rgba(70, 80, 90, 0));

        let pose = Pose {
            center: Vec2::new(5.0, -2.0),
            size: Vec2::new(3.0, 2.0),
            rot: 0.0,
        };
        let (path, n) = node(&img, pose, Rect::from_corners(Vec2::ZERO, Vec2::new(3.0, 2.0)), 1.0);
        let out = flatten(&[n]).unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(out.dimensions(), (3, 2));
        for y in 0..2 {
            for x in 0..3 {
                let src = img.get_pixel(x, y);
                let got = out.get_pixel(x, y);
                if src[3] == 0 {
                    // Fully transparent stays transparent (its RGB is
                    // meaningless and not preserved).
                    assert_eq!(got[3], 0, "({x},{y})");
                } else {
                    assert_eq!(got, src, "({x},{y})");
                }
            }
        }
    }

    #[test]
    fn crop_and_flip_are_applied() {
        // A B C D
        // E F G H
        let mut img = image::RgbaImage::new(4, 2);
        for (i, p) in [(255u8, 0u8), (0, 255), (0, 0), (255, 255)].into_iter().enumerate() {
            img.put_pixel(i as u32, 0, rgba(p.0, 0, 0, 255));
            img.put_pixel(i as u32, 1, rgba(0, p.1, 0, 255));
        }
        let crop = Rect::from_corners(Vec2::new(1.0, 0.0), Vec2::new(3.0, 2.0));
        let pose = Pose {
            center: Vec2::ZERO,
            size: Vec2::new(2.0, 2.0),
            rot: 0.0,
        };
        let (path, mut n) = node(&img, pose, crop, 1.0);
        n.flip_x = true;
        let out = flatten(&[n]).unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(out.dimensions(), (2, 2));
        // Cropped columns 1,2 then mirrored: C B / G F.
        assert_eq!(out.get_pixel(0, 0), img.get_pixel(2, 0));
        assert_eq!(out.get_pixel(1, 0), img.get_pixel(1, 0));
        assert_eq!(out.get_pixel(0, 1), img.get_pixel(2, 1));
        assert_eq!(out.get_pixel(1, 1), img.get_pixel(1, 1));
    }

    #[test]
    fn rotation_is_baked() {
        // Two pixels side by side: red left, blue right.
        let mut img = image::RgbaImage::new(2, 1);
        img.put_pixel(0, 0, rgba(255, 0, 0, 255));
        img.put_pixel(1, 0, rgba(0, 0, 255, 255));
        let pose = Pose {
            center: Vec2::ZERO,
            size: Vec2::new(2.0, 1.0),
            rot: std::f32::consts::FRAC_PI_2, // 90° CCW
        };
        let (path, n) = node(&img, pose, Rect::from_corners(Vec2::ZERO, Vec2::new(2.0, 1.0)), 1.0);
        let out = flatten(&[n]).unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(out.dimensions(), (1, 2));
        // Red (left) rotates to the bottom, blue to the top.
        assert_eq!(*out.get_pixel(0, 0), rgba(0, 0, 255, 255));
        assert_eq!(*out.get_pixel(0, 1), rgba(255, 0, 0, 255));
    }
}
