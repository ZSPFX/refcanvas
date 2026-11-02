use super::*;
use crate::canvas::{
    lod,
    node::{CanvasNode, FrameLabel},
    undo::UndoEntry,
};
use crate::project::oplog::{Clock, Journal, Op, SavedNode};
use bevy::ecs::system::RunSystemOnce;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    app: App,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "refcanvas-tabs-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        let mut tabs = Tabs::default();
        let mut clock = Clock::new("test-device".to_string(), 0, 0);
        for (id, count) in [(0, 3), (1, 5)] {
            let path = root.join(id.to_string());
            let store = ProjectStore { root: path.clone() };
            store.ensure_dirs().unwrap();
            image::RgbaImage::new(16, 16)
                .save(store.images_dir().join("test.png"))
                .unwrap();
            let mut journal = Journal::create(&store.journal_dir(), "seed").unwrap();
            let mut ops = Vec::new();
            for index in 0..count {
                ops.push(Op {
                    id: Uuid::now_v7(),
                    rev: clock.tick(),
                    deleted: false,
                    node: Some(SavedNode {
                        kind: "image".to_string(),
                        x: index as f32,
                        y: 0.0,
                        w: 16.0,
                        h: 16.0,
                        z: index as f32,
                        src: Some("images/test.png".to_string()),
                        hash: Some("test".to_string()),
                        img_w: Some(16),
                        img_h: Some(16),
                        ..Default::default()
                    }),
                });
            }
            ops.push(Op {
                id: Uuid::now_v7(),
                rev: clock.tick(),
                deleted: false,
                node: Some(SavedNode {
                    kind: "frame".to_string(),
                    x: 0.0,
                    y: 0.0,
                    w: 100.0,
                    h: 100.0,
                    label: Some("Frame".to_string()),
                    ..Default::default()
                }),
            });
            journal.append(ops).unwrap();
            let tab = open_tab_at(&path, id, &mut clock).unwrap();
            tabs.stored.push(tab);
        }
        tabs.clock = clock;
        tabs.next_id = 2;
        let mut app = App::new();
        super::super::configure_tab_switching(&mut app);
        app.insert_resource(tabs)
            .init_resource::<View>()
            .init_resource::<ProjectStore>()
            .init_resource::<Assets<Image>>()
            .init_resource::<ZCounter>()
            .init_resource::<PreviewLoader>()
            .init_resource::<TabRequests>()
            .init_resource::<Selection>()
            .init_resource::<Interaction>()
            .init_resource::<UndoStack>()
            .init_resource::<ImportQueue>()
            .init_resource::<Time>()
            .init_resource::<super::super::sync::SyncTimer>()
            .init_resource::<super::super::sync::PathProbe>()
            .add_systems(
                PostUpdate,
                |mut commands: Commands, sprites: Query<Entity, Added<Sprite>>| {
                    for entity in &sprites {
                        commands.entity(entity).insert(Mesh2d::default());
                    }
                },
            )
            .add_systems(Last, lod::park_inactive_meshes);
        Self { app, root }
    }

    fn request(&mut self, request: TabRequest) {
        self.app
            .world_mut()
            .resource_mut::<TabRequests>()
            .queue
            .push(request);
        self.app.update();
    }

    fn nodes(&mut self, id: u64) -> Vec<Entity> {
        let world = self.app.world_mut();
        world
            .query::<(Entity, &CanvasNode, &TabOwner)>()
            .iter(world)
            .filter(|(_, _, owner)| owner.0 == id)
            .map(|(entity, ..)| entity)
            .collect()
    }

    fn active_count(&mut self) -> usize {
        let world = self.app.world_mut();
        world
            .query_filtered::<&CanvasNode, Without<InactiveTab>>()
            .iter(world)
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.app = App::new();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn tabs_load_once_on_first_activation_and_park_their_meshes() {
    let mut f = Fixture::new();
    assert!(f.nodes(0).is_empty());
    assert!(f.nodes(1).is_empty());
    assert!(f.app.world().resource::<Assets<Image>>().is_empty());
    assert!(!f.app.world().resource::<PreviewLoader>().is_active());

    f.request(TabRequest::Switch(0));
    let first = f.nodes(0);
    assert_eq!(first.len(), 4);
    assert!(f.nodes(1).is_empty());
    assert!(f.app.world().resource::<Tabs>().stored[0].loaded);
    assert!(!f.app.world().resource::<Tabs>().stored[1].loaded);
    assert_eq!(f.active_count(), 4);

    f.request(TabRequest::Switch(1));
    let second = f.nodes(1);
    assert_eq!(second.len(), 6);
    assert_eq!(f.active_count(), 6);
    for &entity in &first {
        let node = f.app.world().entity(entity);
        assert!(node.contains::<InactiveTab>());
        assert!(node.contains::<lod::ParkedMesh>());
        assert!(!node.contains::<Mesh2d>());
        assert_eq!(node.get::<Visibility>(), Some(&Visibility::Hidden));
    }
    let world = f.app.world_mut();
    for (label, owner, inactive) in world
        .query::<(&FrameLabel, &TabOwner, Has<InactiveTab>)>()
        .iter(world)
    {
        assert_eq!(inactive, owner.0 == 0);
        assert!(world.get::<CanvasNode>(label.0).is_some());
    }

    f.request(TabRequest::Switch(0));
    assert_eq!(f.nodes(0), first);
    assert_eq!(f.nodes(1), second);
    assert_eq!(f.active_count(), 4);
    for entity in first {
        assert!(f.app.world().entity(entity).contains::<Mesh2d>());
        assert!(!f.app.world().entity(entity).contains::<lod::ParkedMesh>());
    }
}

#[test]
fn switching_saves_edits_and_restores_camera_undo_and_imports() {
    let mut f = Fixture::new();
    f.request(TabRequest::Switch(0));
    let entity = f.nodes(0)[0];
    let row_id = f
        .app
        .world()
        .get::<CanvasNode>(entity)
        .unwrap()
        .node_id
        .unwrap();
    f.app
        .world_mut()
        .get_mut::<Transform>(entity)
        .unwrap()
        .translation
        .x = 1234.0;
    f.app.world_mut().resource_mut::<View>().center = DVec2::new(12.0, 34.0);
    f.app
        .world_mut()
        .resource_mut::<UndoStack>()
        .push(UndoEntry::Added { items: vec![] });
    let path = f.root.join("0/images/test.png");
    super::super::import::enqueue_paths(
        &mut f.app.world_mut().resource_mut::<ImportQueue>(),
        Vec2::ZERO,
        vec![path],
    );

    f.request(TabRequest::Switch(1));
    assert!(f.app.world().resource::<UndoStack>().past.is_empty());
    assert!(!f.app.world().resource::<ImportQueue>().is_active());
    let first = &f.app.world().resource::<Tabs>().stored[0];
    assert!(first.imports.is_active());
    assert_eq!(
        first
            .merge
            .nodes
            .get(&row_id)
            .unwrap()
            .node
            .as_ref()
            .unwrap()
            .x,
        1234.0
    );
    assert_eq!(first.view.0, DVec2::new(12.0, 34.0));

    f.request(TabRequest::Switch(0));
    assert_eq!(f.app.world().resource::<UndoStack>().past.len(), 1);
    assert!(f.app.world().resource::<ImportQueue>().is_active());
    assert_eq!(
        f.app.world().resource::<View>().center,
        DVec2::new(12.0, 34.0)
    );
}

#[test]
fn closing_an_unopened_tab_does_not_delete_its_rows() {
    let mut f = Fixture::new();
    f.request(TabRequest::Switch(0));
    f.request(TabRequest::Close(1));
    assert_eq!(f.app.world().resource::<Tabs>().active_id(), Some(0));
    let mut clock = Clock::new("reopen".to_string(), 0, 0);
    let reopened = open_tab_at(&f.root.join("1"), 99, &mut clock).unwrap();
    assert_eq!(reopened.merge.nodes.len(), 6);
    assert!(f.nodes(1).is_empty());
}

#[test]
fn closing_active_tab_saves_latest_edits_before_loading_neighbor() {
    let mut f = Fixture::new();
    f.request(TabRequest::Switch(0));
    let entity = f.nodes(0)[0];
    let row_id = f
        .app
        .world()
        .get::<CanvasNode>(entity)
        .unwrap()
        .node_id
        .unwrap();
    f.app
        .world_mut()
        .get_mut::<Transform>(entity)
        .unwrap()
        .translation
        .x = 987.0;
    f.request(TabRequest::Close(0));
    assert_eq!(f.app.world().resource::<Tabs>().active_id(), Some(1));
    assert_eq!(f.active_count(), 6);
    assert!(f.nodes(0).is_empty());
    let mut clock = Clock::new("reopen".to_string(), 0, 0);
    let reopened = open_tab_at(&f.root.join("0"), 99, &mut clock).unwrap();
    assert_eq!(
        reopened
            .merge
            .nodes
            .get(&row_id)
            .unwrap()
            .node
            .as_ref()
            .unwrap()
            .x,
        987.0
    );
}

#[test]
fn queued_switches_wait_for_spawns_before_saving() {
    let mut f = Fixture::new();
    f.request(TabRequest::Switch(0));
    f.app
        .world_mut()
        .resource_mut::<TabRequests>()
        .queue
        .extend([TabRequest::Switch(1), TabRequest::Switch(0)]);
    f.app.update();
    assert_eq!(f.app.world().resource::<Tabs>().active_id(), Some(1));
    f.app.update();
    assert_eq!(f.app.world().resource::<Tabs>().active_id(), Some(0));
    assert_eq!(
        f.app.world().resource::<Tabs>().stored[1].merge.nodes.len(),
        6
    );
}

#[test]
fn full_resolution_handles_are_released_when_tab_is_parked() {
    let mut f = Fixture::new();
    f.request(TabRequest::Switch(0));
    let entity = f
        .nodes(0)
        .into_iter()
        .find(|e| {
            f.app
                .world()
                .get::<CanvasNode>(*e)
                .unwrap()
                .image()
                .is_some()
        })
        .unwrap();
    let full = f
        .app
        .world_mut()
        .resource_mut::<Assets<Image>>()
        .add(Image::default());
    {
        let world = f.app.world_mut();
        world.get_mut::<Sprite>(entity).unwrap().image = full.clone();
        let mut node = world.get_mut::<CanvasNode>(entity).unwrap();
        let image = node.image_mut().unwrap();
        image.full = Some(full.clone());
        image.full_incoming = Some(full.clone());
        image.pending_cpu_free = Some(full);
        image.resident_bytes = 1024;
    }
    f.request(TabRequest::Switch(1));
    let image = f
        .app
        .world()
        .get::<CanvasNode>(entity)
        .unwrap()
        .image()
        .unwrap();
    assert!(image.full.is_none());
    assert!(image.full_incoming.is_none());
    assert!(image.pending_cpu_free.is_none());
    assert_eq!(image.resident_bytes, 0);
    assert_eq!(
        f.app.world().get::<Sprite>(entity).unwrap().image,
        image.data.preview
    );
}

#[test]
fn unloaded_tab_is_not_saved_as_an_empty_project() {
    let mut f = Fixture::new();
    f.app.world_mut().run_system_once(|mut tabs: ResMut<Tabs>, view: Res<View>, mut nodes: Query<(Entity, &Transform, &mut CanvasNode, &Sprite, &TabOwner)>| {
        save_active(&mut tabs, &view, &mut nodes);
    }).unwrap();
    assert_eq!(
        f.app.world().resource::<Tabs>().stored[0].merge.nodes.len(),
        4
    );
}

#[test]
fn startup_only_loads_remembered_active_tab_even_after_a_missing_path() {
    let mut f = Fixture::new();
    let roots = vec![f.root.join("missing"), f.root.join("0"), f.root.join("1")];
    f.app.world_mut().insert_resource(Tabs::default());
    f.app
        .world_mut()
        .run_system_once(
            move |mut commands: Commands,
                  mut tabs: ResMut<Tabs>,
                  mut view: ResMut<View>,
                  mut store: ResMut<ProjectStore>,
                  mut zc: ResMut<ZCounter>,
                  mut images: ResMut<Assets<Image>>,
                  mut loader: ResMut<PreviewLoader>,
                  mut nodes: Query<(Entity, &TabOwner, &mut Visibility)>| {
                restore_session(
                    Session {
                        tabs: roots
                            .iter()
                            .map(|root| root.to_string_lossy().into_owned())
                            .collect(),
                        active: 2,
                    },
                    &mut commands,
                    &mut tabs,
                    &mut view,
                    &mut store,
                    &mut zc,
                    &mut images,
                    &mut loader,
                    &mut nodes,
                );
            },
        )
        .unwrap();
    let (first_id, second_id, len, unavailable, first_loaded, second_loaded) = {
        let tabs = f.app.world().resource::<Tabs>();
        (
            tabs.stored[0].id,
            tabs.stored[1].id,
            tabs.len(),
            tabs.unavailable.len(),
            tabs.stored[0].loaded,
            tabs.stored[1].loaded,
        )
    };
    assert_eq!(len, 2);
    assert_eq!(unavailable, 1);
    assert!(!first_loaded);
    assert!(second_loaded);
    assert!(f.nodes(first_id).is_empty());
    assert_eq!(f.nodes(second_id).len(), 6);
    assert_eq!(f.active_count(), 6);
}

#[test]
fn a_project_that_cannot_be_opened_is_not_added() {
    let mut f = Fixture::new();
    f.request(TabRequest::Switch(0));
    let bad = f.root.join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    // A regular file where a directory must be makes opening fail.
    std::fs::write(bad.join("journal"), b"not a directory").unwrap();
    f.request(TabRequest::Open(bad));
    assert_eq!(f.app.world().resource::<Tabs>().active_id(), Some(0));
    assert_eq!(f.app.world().resource::<Tabs>().len(), 2);
    assert_eq!(f.active_count(), 4);
}

#[test]
fn tab_camera_matches_new_images_on_the_first_frame() {
    use crate::canvas::{CanvasSet, camera};
    use bevy::window::RequestRedraw;

    let mut f = Fixture::new();
    f.app
        .init_resource::<Time>()
        .add_message::<RequestRedraw>()
        .add_systems(Update, camera::view_smooth.in_set(CanvasSet));
    let camera_entity = f
        .app
        .world_mut()
        .spawn((
            Camera2d,
            Transform::from_xyz(0.0, 0.0, 1000.0),
            Projection::Orthographic(OrthographicProjection::default_2d()),
        ))
        .id();
    // Give the second tab a camera position in local state before switching.
    f.app.world_mut().resource_mut::<Tabs>().stored[1].view = (DVec2::new(12000.0, -8000.0), 5.0);

    f.request(TabRequest::Switch(0));
    {
        let mut view = f.app.world_mut().resource_mut::<View>();
        view.center = DVec2::new(25.0, -40.0);
        view.log_scale = -2.0;
    }
    for (tab, center, scale) in [
        (1, Vec2::new(12000.0, -8000.0), 5.0_f64.exp() as f32),
        (0, Vec2::new(25.0, -40.0), (-2.0_f64).exp() as f32),
        (1, Vec2::new(12000.0, -8000.0), 5.0_f64.exp() as f32),
    ] {
        f.request(TabRequest::Switch(tab));
        let camera = f.app.world().entity(camera_entity);
        assert_eq!(
            camera.get::<Transform>().unwrap().translation.truncate(),
            center
        );
        assert_eq!(
            camera
                .get::<GlobalTransform>()
                .unwrap()
                .translation()
                .truncate(),
            center
        );
        let Projection::Orthographic(projection) = camera.get::<Projection>().unwrap() else {
            panic!("expected an orthographic camera");
        };
        assert_eq!(projection.scale, scale);
        assert!(
            f.nodes(tab)
                .iter()
                .all(|entity| f.app.world().get::<Visibility>(*entity)
                    == Some(&Visibility::Inherited))
        );
    }
}

#[test]
fn remote_edits_apply_without_echoing_a_local_op() {
    let mut f = Fixture::new();
    f.request(TabRequest::Switch(0));
    let entity = f
        .nodes(0)
        .into_iter()
        .find(|entity| {
            f.app
                .world()
                .get::<CanvasNode>(*entity)
                .unwrap()
                .image()
                .is_some()
        })
        .unwrap();
    let node_id = f
        .app
        .world()
        .get::<CanvasNode>(entity)
        .unwrap()
        .node_id
        .unwrap();

    let store = ProjectStore {
        root: f.root.join("0"),
    };
    let mut clock = Clock::new("remote-device".to_string(), 0, 0);
    let mut journal = Journal::create(&store.journal_dir(), clock.device_id()).unwrap();
    journal
        .append(vec![Op {
            id: node_id,
            rev: clock.tick(),
            deleted: false,
            node: Some(SavedNode {
                kind: "image".to_string(),
                x: 500.0,
                y: 0.0,
                w: 16.0,
                h: 16.0,
                z: 0.0,
                src: Some("images/test.png".to_string()),
                hash: Some("test".to_string()),
                img_w: Some(16),
                img_h: Some(16),
                ..Default::default()
            }),
        }])
        .unwrap();

    let tab = &mut f.app.world_mut().resource_mut::<Tabs>().stored[0];
    let changes =
        crate::project::oplog::read_project(&store.root, &mut tab.merge, &mut tab.files).unwrap();
    tab.pending.extend(changes);

    f.app.update();
    assert_eq!(
        f.app
            .world()
            .get::<Transform>(entity)
            .unwrap()
            .translation
            .x,
        500.0
    );

    // Applying remote state must not make the local diff emit it back.
    let journal_path = f.app.world().resource::<Tabs>().stored[0]
        .journal
        .path()
        .to_path_buf();
    let before = std::fs::metadata(&journal_path).unwrap().len();
    f.app
        .world_mut()
        .run_system_once(
            |mut tabs: ResMut<Tabs>,
             view: Res<View>,
             mut nodes: Query<(Entity, &Transform, &mut CanvasNode, &Sprite, &TabOwner)>| {
                save_active(&mut tabs, &view, &mut nodes);
            },
        )
        .unwrap();
    let after = std::fs::metadata(&journal_path).unwrap().len();
    assert_eq!(before, after);
}

#[test]
fn remote_tombstone_removes_a_live_node() {
    let mut f = Fixture::new();
    f.request(TabRequest::Switch(0));
    let entity = f.nodes(0)[0];
    let node_id = f
        .app
        .world()
        .get::<CanvasNode>(entity)
        .unwrap()
        .node_id
        .unwrap();
    let store = ProjectStore {
        root: f.root.join("0"),
    };
    let mut clock = Clock::new("remote-device".to_string(), 100, 0);
    let mut journal = Journal::create(&store.journal_dir(), clock.device_id()).unwrap();
    journal
        .append(vec![Op {
            id: node_id,
            rev: clock.tick(),
            deleted: true,
            node: None,
        }])
        .unwrap();

    let tab = &mut f.app.world_mut().resource_mut::<Tabs>().stored[0];
    let changes =
        crate::project::oplog::read_project(&store.root, &mut tab.merge, &mut tab.files).unwrap();
    tab.pending.extend(changes);
    f.app.update();
    assert!(f.app.world().get_entity(entity).is_err());
}

#[test]
fn legacy_database_migrates_once_and_ignores_the_old_camera() {
    let root = std::env::temp_dir().join(format!(
        "refcanvas-legacy-{}-{}",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    let path = root.join("legacy");
    let store = ProjectStore { root: path.clone() };
    store.ensure_dirs().unwrap();
    image::RgbaImage::new(16, 16)
        .save(store.images_dir().join("test.png"))
        .unwrap();
    {
        let db = rusqlite::Connection::open(store.db_path()).unwrap();
        db.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE nodes (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 kind TEXT NOT NULL DEFAULT 'image',
                 x REAL NOT NULL, y REAL NOT NULL, w REAL NOT NULL, h REAL NOT NULL,
                 rot REAL NOT NULL DEFAULT 0, z REAL NOT NULL DEFAULT 0,
                 src TEXT, orig_name TEXT, hash TEXT, img_w INTEGER, img_h INTEGER,
                 flip_x INTEGER NOT NULL DEFAULT 0, flip_y INTEGER NOT NULL DEFAULT 0);
             INSERT INTO meta VALUES ('view_x','12000'),('view_y','-8000'),('view_log_scale','5');
             INSERT INTO nodes (x,y,w,h,z,src,hash,img_w,img_h)
                 VALUES (42,0,16,16,0,'images/test.png','test',16,16);",
        )
        .unwrap();
    }

    let mut clock = Clock::new("migrator".to_string(), 0, 0);
    let first = open_tab_at(&path, 1, &mut clock).unwrap();
    assert_eq!(first.merge.nodes.len(), 1);
    assert_eq!(first.view, (DVec2::ZERO, 0.0));

    // A second open sees the journal and must not import the same row again.
    let second = open_tab_at(&path, 2, &mut clock).unwrap();
    assert_eq!(second.merge.nodes.len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn large_tab_mesh_transitions_are_bounded_and_survive_a_quick_return() {
    let mut app = App::new();
    app.add_systems(Update, lod::resume_active_meshes)
        .add_systems(Last, lod::park_inactive_meshes);
    let entities: Vec<_> = (0..5000)
        .map(|_| app.world_mut().spawn((Mesh2d::default(), InactiveTab)).id())
        .collect();
    let count = |world: &mut World| world.query::<&lod::ParkedMesh>().iter(world).count();

    app.update();
    assert_eq!(count(app.world_mut()), lod::TAB_MESHES_PER_FRAME);
    for entity in &entities {
        app.world_mut().entity_mut(*entity).remove::<InactiveTab>();
    }
    app.update();
    assert_eq!(count(app.world_mut()), 0);
    assert!(
        entities
            .iter()
            .all(|entity| app.world().get::<Mesh2d>(*entity).is_some())
    );

    for entity in &entities {
        app.world_mut().entity_mut(*entity).insert(InactiveTab);
    }
    for _ in 0..5000_usize.div_ceil(lod::TAB_MESHES_PER_FRAME) {
        app.update();
    }
    assert_eq!(count(app.world_mut()), 5000);
    for entity in &entities {
        app.world_mut().entity_mut(*entity).remove::<InactiveTab>();
    }
    app.update();
    assert_eq!(count(app.world_mut()), 5000 - lod::TAB_MESHES_PER_FRAME);
    for _ in 1..5000_usize.div_ceil(lod::TAB_MESHES_PER_FRAME) {
        app.update();
    }
    assert_eq!(count(app.world_mut()), 0);
    assert!(
        entities
            .iter()
            .all(|entity| app.world().get::<Mesh2d>(*entity).is_some())
    );
}
