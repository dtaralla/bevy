pub mod camera;
pub mod color;
pub mod mesh;
pub mod render_asset;
pub mod render_component;
pub mod render_graph;
pub mod render_phase;
pub mod render_resource;
pub mod renderer;
pub mod shader;
pub mod texture;
pub mod view;

pub use once_cell;

use crate::{
    camera::CameraPlugin,
    mesh::MeshPlugin,
    render_graph::RenderGraph,
    renderer::render_system,
    texture::ImagePlugin,
    view::{ViewPlugin, WindowRenderPlugin},
};
use bevy_app::{App, AppLabel, Plugin};
use bevy_asset::AssetServer;
use bevy_ecs::prelude::*;
use std::ops::{Deref, DerefMut};
use wgpu::BackendBit;

#[derive(Default)]
pub struct RenderPlugin;

/// The names of the default App stages
#[derive(Debug, Hash, PartialEq, Eq, Clone, StageLabel)]
pub enum RenderStage {
    /// Extract data from "app world" and insert it into "render world". This step should be kept
    /// as short as possible to increase the "pipelining potential" for running the next frame
    /// while rendering the current frame.
    Extract,

    /// Prepare render resources from extracted data.
    Prepare,

    /// Create Bind Groups that depend on Prepare data and queue up draw calls to run during the Render stage.
    Queue,

    // TODO: This could probably be moved in favor of a system ordering abstraction in Render or Queue
    /// Sort RenderPhases here
    PhaseSort,

    /// Actual rendering happens here. In most cases, only the render backend should insert resources here
    Render,

    /// Cleanup render resources here.
    Cleanup,
}

/// The Render App World. This is only available as a resource during the Extract step.
#[derive(Default)]
pub struct RenderWorld(World);

impl Deref for RenderWorld {
    type Target = World;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for RenderWorld {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Label for the rendering sub-app
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, AppLabel)]
pub struct RenderApp;

/// A "scratch" world used to avoid allocating new worlds every frame when
// swapping out the Render World.
#[derive(Default)]
struct ScratchRenderWorld(World);

impl Plugin for RenderPlugin {
    fn build(&self, app: &mut App) {
        let (instance, device, queue) =
            futures_lite::future::block_on(renderer::initialize_renderer(
                BackendBit::PRIMARY,
                &wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    ..Default::default()
                },
                &wgpu::DeviceDescriptor::default(),
            ));
        app.insert_resource(device.clone())
            .insert_resource(queue.clone())
            .init_resource::<ScratchRenderWorld>();
        let asset_server = app.world.get_resource::<AssetServer>().unwrap().clone();

        let mut render_app = App::empty();
        let mut extract_stage = SystemStage::parallel();
        // don't apply buffers when the stage finishes running
        // extract stage runs on the app world, but the buffers are applied to the render world
        extract_stage.set_apply_buffers(false);
        render_app
            .add_stage(RenderStage::Extract, extract_stage)
            .add_stage(RenderStage::Prepare, SystemStage::parallel())
            .add_stage(RenderStage::Queue, SystemStage::parallel())
            .add_stage(RenderStage::PhaseSort, SystemStage::parallel())
            .add_stage(
                RenderStage::Render,
                SystemStage::parallel().with_system(render_system.exclusive_system()),
            )
            .add_stage(RenderStage::Cleanup, SystemStage::parallel())
            .insert_resource(instance)
            .insert_resource(device)
            .insert_resource(queue)
            .insert_resource(asset_server)
            .init_resource::<RenderGraph>();

        app.add_sub_app(RenderApp, render_app, move |app_world, render_app| {
            #[cfg(feature = "trace")]
            let render_span = bevy_utils::tracing::info_span!("renderer subapp");
            #[cfg(feature = "trace")]
            let _render_guard = render_span.enter();
            {
                #[cfg(feature = "trace")]
                let stage_span =
                    bevy_utils::tracing::info_span!("stage", name = "reserve_and_flush");
                #[cfg(feature = "trace")]
                let _stage_guard = stage_span.enter();

                // reserve all existing app entities for use in render_app
                // they can only be spawned using `get_or_spawn()`
                let meta_len = app_world.entities().meta.len();
                render_app
                    .world
                    .entities()
                    .reserve_entities(meta_len as u32);

                // flushing as "invalid" ensures that app world entities aren't added as "empty archetype" entities by default
                // these entities cannot be accessed without spawning directly onto them
                // this _only_ works as expected because clear_entities() is called at the end of every frame.
                render_app.world.entities_mut().flush_as_invalid();
            }

            {
                #[cfg(feature = "trace")]
                let stage_span = bevy_utils::tracing::info_span!("stage", name = "extract");
                #[cfg(feature = "trace")]
                let _stage_guard = stage_span.enter();

                // extract
                extract(app_world, render_app);
            }

            {
                #[cfg(feature = "trace")]
                let stage_span = bevy_utils::tracing::info_span!("stage", name = "prepare");
                #[cfg(feature = "trace")]
                let _stage_guard = stage_span.enter();

                // prepare
                let prepare = render_app
                    .schedule
                    .get_stage_mut::<SystemStage>(&RenderStage::Prepare)
                    .unwrap();
                prepare.run(&mut render_app.world);
            }

            {
                #[cfg(feature = "trace")]
                let stage_span = bevy_utils::tracing::info_span!("stage", name = "queue");
                #[cfg(feature = "trace")]
                let _stage_guard = stage_span.enter();

                // queue
                let queue = render_app
                    .schedule
                    .get_stage_mut::<SystemStage>(&RenderStage::Queue)
                    .unwrap();
                queue.run(&mut render_app.world);
            }

            {
                #[cfg(feature = "trace")]
                let stage_span = bevy_utils::tracing::info_span!("stage", name = "sort");
                #[cfg(feature = "trace")]
                let _stage_guard = stage_span.enter();

                // phase sort
                let phase_sort = render_app
                    .schedule
                    .get_stage_mut::<SystemStage>(&RenderStage::PhaseSort)
                    .unwrap();
                phase_sort.run(&mut render_app.world);
            }

            {
                #[cfg(feature = "trace")]
                let stage_span = bevy_utils::tracing::info_span!("stage", name = "render");
                #[cfg(feature = "trace")]
                let _stage_guard = stage_span.enter();

                // render
                let render = render_app
                    .schedule
                    .get_stage_mut::<SystemStage>(&RenderStage::Render)
                    .unwrap();
                render.run(&mut render_app.world);
            }

            {
                #[cfg(feature = "trace")]
                let stage_span = bevy_utils::tracing::info_span!("stage", name = "cleanup");
                #[cfg(feature = "trace")]
                let _stage_guard = stage_span.enter();

                // cleanup
                let cleanup = render_app
                    .schedule
                    .get_stage_mut::<SystemStage>(&RenderStage::Cleanup)
                    .unwrap();
                cleanup.run(&mut render_app.world);

                render_app.world.clear_entities();
            }
        });

        app.add_plugin(WindowRenderPlugin)
            .add_plugin(CameraPlugin)
            .add_plugin(ViewPlugin)
            .add_plugin(MeshPlugin)
            .add_plugin(ImagePlugin);
    }
}

fn extract(app_world: &mut World, render_app: &mut App) {
    let extract = render_app
        .schedule
        .get_stage_mut::<SystemStage>(&RenderStage::Extract)
        .unwrap();

    // temporarily add the render world to the app world as a resource
    let scratch_world = app_world.remove_resource::<ScratchRenderWorld>().unwrap();
    let render_world = std::mem::replace(&mut render_app.world, scratch_world.0);
    app_world.insert_resource(RenderWorld(render_world));

    extract.run(app_world);

    // add the render world back to the render app
    let render_world = app_world.remove_resource::<RenderWorld>().unwrap();
    let scratch_world = std::mem::replace(&mut render_app.world, render_world.0);
    app_world.insert_resource(ScratchRenderWorld(scratch_world));

    extract.apply_buffers(&mut render_app.world);
}