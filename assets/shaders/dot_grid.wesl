// The dot grid, drawn on a quad that is kept centered on the camera and sized
// to the viewport. Dots are computed entirely from uniforms so they stay
// pixel-stable at any zoom and any world position.
//
// `frac_*` is the world-lattice phase relative to the camera center, computed
// in f64 on the CPU and passed as [0,1) pairs.
//
// Two lattices (fine + coarse, 2x apart) are cross-faded by `alpha_*`.

import bevy_sprite_render::mesh2d::vertex_output::VertexOutput;

struct GridParams {
    frac_a: vec2<f32>,
    frac_b: vec2<f32>,
    viewport_px: vec2<f32>,
    spacing_a: f32,
    spacing_b: f32,
    world_per_px: f32,
    dot_radius: f32,
    alpha_a: f32,
    alpha_b: f32,
    bg: vec4<f32>,
    dot_color: vec4<f32>,
    fade: vec4<f32>,
}

@group(constants::MATERIAL_BIND_GROUP) @binding(0) var<uniform> grid: GridParams;

fn dot_alpha(px: vec2<f32>, phase: vec2<f32>, spacing: f32) -> f32 {
    // Fragment position relative to the camera center, in world units.
    // (+y up in world space, so the screen y axis flips.)
    let local = (px - grid.viewport_px * 0.5) * vec2<f32>(grid.world_per_px, -grid.world_per_px);
    // Distance to the nearest world-lattice point.
    let g = fract(phase + (local + vec2<f32>(spacing * 0.5)) / spacing);
    let d = (g - vec2<f32>(0.5)) * spacing;
    let d_px = length(d) / grid.world_per_px;
    // Anti-aliased disc.
    return 1.0 - smoothstep(grid.dot_radius - 0.75, grid.dot_radius + 0.75, d_px);
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let a_fine = dot_alpha(in.position.xy, grid.frac_a, grid.spacing_a) * grid.alpha_a;
    let a_coarse = dot_alpha(in.position.xy, grid.frac_b, grid.spacing_b) * grid.alpha_b;
    let alpha = clamp(a_fine + a_coarse, 0.0, 1.0) * grid.fade.x;
    return vec4<f32>(mix(grid.bg.rgb, grid.dot_color.rgb, alpha * grid.dot_color.a), 1.0);
}
