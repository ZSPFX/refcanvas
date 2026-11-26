//! dot grid: one quad, shader rebuilds world offset from screen and f64 uniforms.
//! two lattices a power of two apart cross-fade while zooming.

use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, ShaderType};
use bevy::shader::ShaderRef;
use bevy::sprite_render::{Material2d, Material2dPlugin};
use bevy::window::PrimaryWindow;

use super::camera::{View, world_per_logical_px};
use crate::CANVAS_BG;

pub struct GridPlugin;

/// grid shader path: release builds embed it (registered by
/// [`GridPlugin::build`]) so the exe ships standalone; dev reads `assets/` for hot reload.
const GRID_SHADER: &str = if cfg!(debug_assertions) {
    "shaders/dot_grid.wesl"
} else {
    "embedded://refcanvas/shaders/dot_grid.wesl"
};

/// registry key: [`GRID_SHADER`] minus the `embedded://` source prefix.
#[cfg(not(debug_assertions))]
const GRID_SHADER_EMBEDDED: &str = "refcanvas/shaders/dot_grid.wesl";

impl Plugin for GridPlugin {
    fn build(&self, app: &mut App) {
        // release ships as one file: embed the shader. the registry shares its
        // in-memory dir with the `embedded` asset source, so the path resolves.
        #[cfg(not(debug_assertions))]
        app.world_mut()
            .resource_mut::<bevy::asset::io::embedded::EmbeddedAssetRegistry>()
            .insert_asset(
                // only used by the `embedded_watcher` feature (not enabled).
                std::path::PathBuf::new(),
                std::path::Path::new(GRID_SHADER_EMBEDDED),
                include_bytes!("../../assets/shaders/dot_grid.wesl"),
            );

        app.add_plugins(Material2dPlugin::<GridMaterial>::default())
            .add_systems(Startup, setup_grid);
    }
}

/// grid dot visibility (0 = hidden), toggled with g.
#[derive(Resource)]
pub struct GridFade(pub f32);

impl Default for GridFade {
    fn default() -> Self {
        Self(1.0)
    }
}

#[derive(Component)]
pub struct GridQuad;

const GRID_Z: f32 = -1000.0;
/// world-space grid spacing at 100% zoom (1 world unit = 1 logical px there).
const BASE_SPACING: f64 = 10.0;
/// minimum on-screen dot spacing (logical px) before the grid subdivides.
const MIN_SCREEN_SPACING: f64 = 14.0;
const DOT_RADIUS_LOGICAL: f32 = 1.2;

#[derive(Asset, TypePath, AsBindGroup, Debug, Clone, Default)]
pub struct GridMaterial {
    #[uniform(0)]
    pub params: GridParams,
}

/// must match `GridParams` in `assets/shaders/dot_grid.wesl` exactly.
#[derive(Debug, Clone, ShaderType)]
pub struct GridParams {
    /// world-lattice phase for the fine / coarse levels, in [0,1).
    pub frac_a: Vec2,
    pub frac_b: Vec2,
    /// render target size, physical pixels.
    pub viewport_px: Vec2,
    pub spacing_a: f32,
    pub spacing_b: f32,
    /// world units per physical pixel.
    pub world_per_px: f32,
    pub dot_radius: f32,
    pub alpha_a: f32,
    pub alpha_b: f32,
    pub bg: Vec4,
    pub dot: Vec4,
    /// global fade (grid visibility toggle).
    pub fade: Vec4,
}

impl Default for GridParams {
    fn default() -> Self {
        Self {
            frac_a: Vec2::ZERO,
            frac_b: Vec2::ZERO,
            viewport_px: Vec2::ONE,
            spacing_a: BASE_SPACING as f32,
            spacing_b: (BASE_SPACING * 2.0) as f32,
            world_per_px: 1.0,
            dot_radius: DOT_RADIUS_LOGICAL,
            alpha_a: 1.0,
            alpha_b: 0.0,
            bg: linear(CANVAS_BG),
            dot: linear(Color::srgb(0.235, 0.235, 0.235)),
            fade: Vec4::ONE,
        }
    }
}

impl Material2d for GridMaterial {
    fn fragment_shader() -> ShaderRef {
        GRID_SHADER.into()
    }
}

fn setup_grid(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<GridMaterial>>,
) {
    commands.spawn((
        GridQuad,
        Mesh2d(meshes.add(Rectangle::new(1.0, 1.0))),
        MeshMaterial2d(materials.add(GridMaterial::default())),
        Transform::from_xyz(0.0, 0.0, GRID_Z),
    ));
}

pub fn update_grid(
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform)>,
    quad: Single<(&mut Transform, &MeshMaterial2d<GridMaterial>), With<GridQuad>>,
    mut materials: ResMut<Assets<GridMaterial>>,
    view: Res<View>,
    grid_fade: Res<GridFade>,
) {
    let (camera, cam_transform) = *camera;
    let Some(wpp_logical) = world_per_logical_px(camera, cam_transform) else {
        return;
    };
    let (mut quad_transform, material_handle) = quad.into_inner();

    let logical = window.size().as_dvec2();
    let dpi = window.scale_factor() as f64;
    let physical = logical * dpi;
    let wpp_physical = wpp_logical / dpi;

    // choose the cross-faded lattice pair. v = 0 at "fine spacing exactly
    // MIN_SCREEN_SPACING"; the fraction fades fine out while coarse fades in.
    let base_screen = BASE_SPACING / wpp_logical;
    let v = (MIN_SCREEN_SPACING / base_screen).log2();
    let frac = v - v.floor();
    let level_a = v.floor().clamp(-12.0, 20.0);
    let spacing_a = BASE_SPACING * level_a.exp2();
    let spacing_b = spacing_a * 2.0;
    let alpha_a = (1.0 - frac) as f32;
    let alpha_b = frac as f32;

    let frac_of = |spacing: f64| -> Vec2 {
        // phase of the world lattice relative to the camera center.
        Vec2::new(
            (view.center.x / spacing).rem_euclid(1.0) as f32,
            (view.center.y / spacing).rem_euclid(1.0) as f32,
        )
    };

    let params = GridParams {
        frac_a: frac_of(spacing_a),
        frac_b: frac_of(spacing_b),
        viewport_px: physical.as_vec2(),
        spacing_a: spacing_a as f32,
        spacing_b: spacing_b as f32,
        world_per_px: wpp_physical as f32,
        dot_radius: DOT_RADIUS_LOGICAL * dpi as f32,
        alpha_a,
        alpha_b,
        fade: Vec4::splat(grid_fade.0),
        ..default()
    };

    // size the quad to (slightly more than) the visible world area.
    let viewport_world = logical * wpp_logical * 1.02;
    quad_transform.translation.x = view.center.x as f32;
    quad_transform.translation.y = view.center.y as f32;
    quad_transform.scale.x = viewport_world.x as f32;
    quad_transform.scale.y = viewport_world.y as f32;

    if let Some(mut material) = materials.get_mut(&material_handle.0) {
        material.params = params;
    }
}

/// sRGB → linear rgba, matching what shaders expect.
pub fn linear(color: Color) -> Vec4 {
    let l = color.to_linear();
    Vec4::new(l.red, l.green, l.blue, l.alpha)
}
