//! frames: labeled containers that drag their images along.

use std::collections::HashMap;

use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::text::TextLayoutInfo;

use super::camera::View;
use super::hit::{self, point_inside};
use super::node::{InactiveTab, CanvasNode, FrameLabel, FrameNode, NodeKind, Pose, TabOwner};
use super::selection::NodeQ;
use crate::project::oplog::NodeId;
use crate::project::tabs::Tabs;
use crate::theme::Accent;

/// frame fill/border/label alphas and label font size; hue follows the desktop
/// accent, which can change at runtime, so fill, border and label stay in sync.
const FRAME_FILL_ALPHA: f32 = 0.025;
const FRAME_BORDER_ALPHA: f32 = 0.28;
const LABEL_ALPHA: f32 = 0.8;
const LABEL_FONT_SIZE: f32 = 16.0;

/// world-space label hit boxes, refreshed by [`update_frame_labels`]; picking
/// treats the text as chrome, so a click selects or moves the frame.
#[derive(Resource, Default)]
pub struct FrameLabelHits(HashMap<Entity, Pose>);

impl FrameLabelHits {
    pub fn get(&self, frame: Entity) -> Option<&Pose> {
        self.0.get(&frame)
    }
}

/// double-click window for the label rename editor
pub const LABEL_DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(350);

/// double-click a label to edit; enter/click-away commits, escape cancels
#[derive(Resource, Default)]
pub struct FrameRename {
    pub frame: Option<Entity>,
    pub text: String,
    /// grab focus on the next ui frame
    pub focus_me: bool,
    /// had focus at least once, so a failed focus request can't commit instantly
    pub had_focus: bool,
    /// (time, frame) of the last label click, for double-click detection
    pub last_click: Option<(std::time::Instant, Entity)>,
}

impl FrameRename {
    /// seed the editor with the current label
    pub fn start(&mut self, frame: Entity, label: String) {
        self.frame = Some(frame);
        self.text = label;
        self.focus_me = true;
        self.had_focus = false;
        self.last_click = None;
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_frame(
    commands: &mut Commands,
    tab_id: u64,
    visible: bool,
    node_id: Option<NodeId>,
    label: String,
    pose: Pose,
    z: f32,
) -> Entity {
    let frame = commands
        .spawn((
            CanvasNode {
                node_id,
                kind: NodeKind::Frame(FrameNode {
                    label: label.clone(),
                }),
                size: pose.size,
                rotation: pose.rot,
            },
            // transparent here; `update_frame_colors` tints it every frame
            Sprite::from_color(Color::NONE, pose.size),
            Transform {
                translation: Vec3::new(pose.center.x, pose.center.y, z),
                rotation: Quat::from_rotation_z(pose.rot),
                ..default()
            },
            TabOwner(tab_id),
            if visible {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            },
        ))
        .id();

    let label_entity = commands
        .spawn((
            Text2d::new(label),
            TextFont {
                font_size: bevy::text::FontSize::Px(LABEL_FONT_SIZE),
                ..default()
            },
            TextColor(Color::NONE),
            Anchor::BOTTOM_LEFT,
            Transform::default(),
            FrameLabel(frame),
            TabOwner(tab_id),
            ChildOf(frame),
        ))
        .id();
    if !visible {
        commands.entity(frame).insert(InactiveTab);
        commands.entity(label_entity).insert(InactiveTab);
    }
    frame
}

/// tint fills and labels with the desktop accent; it can change at runtime, so
/// this runs every frame but only writes on change.
pub fn update_frame_colors(
    accent: Res<Accent>,
    mut fills: Query<(&mut Sprite, &CanvasNode), Without<InactiveTab>>,
    mut labels: Query<(&FrameLabel, &mut TextColor), Without<InactiveTab>>,
) {
    let fill = accent.alpha(FRAME_FILL_ALPHA);
    for (mut sprite, node) in &mut fills {
        if node.kind.is_frame() && sprite.color != fill {
            sprite.color = fill;
        }
    }
    let label = accent.alpha(LABEL_ALPHA);
    for (_, mut color) in &mut labels {
        if color.0 != label {
            color.0 = label;
        }
    }
}

/// keep labels at constant screen size above the frame, and record their
/// world-space boxes for picking.
pub fn update_frame_labels(
    view: Res<View>,
    tabs: Res<Tabs>,
    // `Without<FrameLabel>` makes this disjoint from the label query: both
    // touch `Transform`.
    frames: Query<(&CanvasNode, &TabOwner, &Transform), (Without<FrameLabel>, Without<InactiveTab>)>,
    mut labels: Query<(
        &FrameLabel,
        &mut Transform,
        &mut Visibility,
        &TextLayoutInfo,
    ), Without<InactiveTab>>,
    mut hits: ResMut<FrameLabelHits>,
) {
    let wpp = (view.scale() as f32).max(1e-4);
    let active = tabs.active_id();
    hits.0.clear();
    for (label, mut tf, mut vis, layout) in &mut labels {
        let Ok((node, owner, frame_tf)) = frames.get(label.0) else {
            continue;
        };
        let is_active = active == Some(owner.0);
        let screen_w = node.size.x / wpp;
        if !is_active || screen_w < 28.0 {
            *vis = Visibility::Hidden;
            continue;
        }
        *vis = Visibility::Inherited;
        tf.scale = Vec3::splat(wpp);
        tf.translation = Vec3::new(
            -node.size.x * 0.5 + 2.0 * wpp,
            node.size.y * 0.5 + 4.0 * wpp,
            0.0,
        );

        // bottom-left anchored; layout is physical px, convert to logical first
        let logical = layout.size / layout.scale_factor.max(1e-4);
        // 2 logical px of grab padding all around.
        let size = (logical * wpp).max(Vec2::ZERO) + Vec2::splat(2.0 * wpp);
        let rot = hit::rot2(node.rotation);
        let origin = frame_tf.translation.truncate() + rot * tf.translation.truncate();
        hits.0.insert(
            label.0,
            Pose {
                center: origin + rot * (size * 0.5),
                size,
                rot: node.rotation,
            },
        );
    }
}

pub fn draw_frame_borders(
    mut gizmos: Gizmos,
    accent: Res<Accent>,
    tabs: Res<Tabs>,
    q: Query<(&Transform, &CanvasNode, &TabOwner), Without<InactiveTab>>,
) {
    let Some(active) = tabs.active_id() else {
        return;
    };
    let border = accent.alpha(FRAME_BORDER_ALPHA);
    for (tf, node, owner) in &q {
        if !node.kind.is_frame() || owner.0 != active {
            continue;
        }
        let iso = Isometry2d::new(tf.translation.truncate(), Rot2::radians(node.rotation));
        gizmos.rect_2d(iso, node.size, border);
    }
}

/// images whose centers fall inside any frame pose
pub fn contained_images(frames: &[Pose], q: &NodeQ) -> Vec<Entity> {
    let mut out = Vec::new();
    if frames.is_empty() {
        return out;
    }
    for (e, tf, _, node) in q.iter() {
        if node.kind.is_frame() {
            continue;
        }
        let center = tf.translation.truncate();
        if frames.iter().any(|f| point_inside(center, f)) {
            out.push(e);
        }
    }
    out
}
