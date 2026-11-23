//! Offscreen GPU checks for scene compositing and depth occlusion.
use super::*;
use crate::scene3d::skin::{
    BodyType, Skin,
    tests::{png, sample_image},
};
use crate::scene3d::{
    camera::Camera,
    scene::{self, SceneInput, SceneObject},
};
use eframe::egui::{PaintCallbackInfo, Rect, pos2};

const WIDTH: u32 = 256;
const HEIGHT: u32 = 192;

fn texture(device: &wgpu::Device, format: wgpu::TextureFormat, samples: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("scene_regression_target"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: samples,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | if samples == 1 {
                wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::empty()
            },
        view_formats: &[],
    })
}

fn callback(rect: Rect, skin: Option<&crate::scene3d::skin::Skin>) -> SceneCallback {
    let camera = Camera::default();
    let mut mesh = MeshBuilder::default();
    let objects = [
        SceneObject {
            display_number: 1,
            position: [-0.4, 0.2, -0.3],
            active: true,
            gain: 1.0,
            ..Default::default()
        },
        SceneObject {
            display_number: 2,
            head_locked: true,
            position: [0.3, 0.6, 0.2],
            active: true,
            gain: 0.1,
            ..Default::default()
        },
    ];
    scene::build(
        &mut mesh,
        &camera,
        rect.height(),
        SceneInput {
            objects: &objects,
            skin,
            show_element_numbers: true,
            has_lfe: true,
            ..Default::default()
        },
    );
    scene_callback(rect, &camera, &mesh)
}

fn scene_callback(rect: Rect, camera: &Camera, mesh: &MeshBuilder) -> SceneCallback {
    let mut callback =
        SceneCallback::new(mesh, camera.view_projection(rect.width() / rect.height()));
    callback.rect = rect;
    callback
}

fn viewport(pass: &mut wgpu::RenderPass<'_>, info: &PaintCallbackInfo) {
    let view = info.viewport_in_pixels();
    #[allow(
        clippy::cast_precision_loss,
        reason = "the test viewport fits exactly in f32"
    )]
    pass.set_viewport(
        view.left_px as f32,
        view.top_px as f32,
        view.width_px as f32,
        view.height_px as f32,
        0.0,
        1.0,
    );
    let clip = info.clip_rect_in_pixels();
    pass.set_scissor_rect(
        u32::try_from(clip.left_px).unwrap(),
        u32::try_from(clip.top_px).unwrap(),
        u32::try_from(clip.width_px).unwrap(),
        u32::try_from(clip.height_px).unwrap(),
    );
}

#[allow(
    clippy::too_many_lines,
    reason = "one regression renders both the former pass and the new composite to readback textures"
)]
fn render(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    format: wgpu::TextureFormat,
    callback: &SceneCallback,
    reference: bool,
) -> Vec<u8> {
    let rect = callback.rect;
    let screen = ScreenDescriptor {
        size_in_pixels: [WIDTH, HEIGHT],
        pixels_per_point: 1.5,
    };
    let info = PaintCallbackInfo {
        viewport: rect,
        clip_rect: rect.shrink(2.0),
        pixels_per_point: screen.pixels_per_point,
        screen_size_px: screen.size_in_pixels,
    };
    let mut resources = CallbackResources::default();
    resources.insert(SceneRenderer::new(device, format));
    let output = texture(device, format, 1);
    let output_view = output.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    let background = wgpu::Color {
        r: 0.5,
        g: 0.3,
        b: 0.2,
        a: 1.0,
    };
    if reference {
        let renderer = resources.get_mut::<SceneRenderer>().unwrap();
        renderer.reserve(device, callback.vertices.len() as u64);
        queue.write_buffer(
            &renderer.uniforms,
            0,
            bytemuck::cast_slice(&callback.view_projection),
        );
        queue.write_buffer(
            &renderer.vertices,
            0,
            bytemuck::cast_slice(&callback.vertices),
        );
        let colour = texture(device, format, MSAA_SAMPLES);
        let depth = texture(device, DEPTH_FORMAT, MSAA_SAMPLES);
        let colour_view = colour.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("legacy_full_window_msaa"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &colour_view,
                depth_slice: None,
                resolve_target: Some(&output_view),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(background),
                    store: wgpu::StoreOp::Discard,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        viewport(&mut pass, &info);
        pass.set_bind_group(0, &renderer.bind_group, &[]);
        pass.set_vertex_buffer(0, renderer.vertices.slice(..));
        for (pipeline, range) in [
            (&renderer.solid, &callback.solid),
            (&renderer.line, &callback.line),
            (&renderer.decal, &callback.decal),
            (&renderer.transparent, &callback.transparent),
        ] {
            pass.set_pipeline(pipeline);
            pass.draw(range.clone(), 0..1);
        }
    } else {
        let commands = callback.prepare(device, queue, &screen, &mut encoder, &mut resources);
        assert!(commands.is_empty());
        let target = resources
            .get::<SceneRenderer>()
            .unwrap()
            .target
            .as_ref()
            .unwrap();
        assert!(
            u64::from(target.size[0]) * u64::from(target.size[1])
                < u64::from(WIDTH) * u64::from(HEIGHT)
        );
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("single_sample_composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &output_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(background),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            })
            .forget_lifetime();
        viewport(&mut pass, &info);
        callback.paint(info, &mut pass, &resources);
    }
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("scene_regression_readback"),
        size: u64::from(WIDTH) * u64::from(HEIGHT) * 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        output.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(WIDTH * 4),
                rows_per_image: Some(HEIGHT),
            },
        },
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    let (send, receive) = std::sync::mpsc::channel();
    readback
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            send.send(result).unwrap();
        });
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    receive.recv().unwrap().unwrap();
    let bytes = readback.slice(..).get_mapped_range().unwrap().to_vec();
    readback.unmap();
    bytes
}

fn device_and_queue() -> (wgpu::Device, wgpu::Queue) {
    let instance = wgpu::Instance::default();
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .unwrap();
    eprintln!("GPU scene regression: {:?}", adapter.get_info());
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap()
}

#[test]
#[ignore = "requires a GPU adapter; compares real offscreen MSAA and compositing"]
fn composite_preserves_scene_colours_occlusion_and_antialiasing() {
    let (device, queue) = device_and_queue();
    let skins = [BodyType::Steve, BodyType::Alex].map(|model| {
        let mut image = sample_image(model, false);
        for y in 8..16 {
            for x in 40..48 {
                image.put_pixel(x, y, image::Rgba([240, 30, 70, 128]));
            }
        }
        Skin::decode(&png(&image)).unwrap()
    });
    for skin in [None, Some(&skins[0]), Some(&skins[1])] {
        for format in [
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ] {
            for rect in [
                Rect::from_min_max(pos2(12.25, 8.4), pos2(148.8, 121.9)),
                Rect::from_min_max(pos2(-12.0, -5.0), pos2(125.0, 115.0)),
                Rect::from_min_max(pos2(60.0, 40.0), pos2(185.0, 145.0)),
            ] {
                let callback = callback(rect, skin);
                let before = render(&device, &queue, format, &callback, true);
                let after = render(&device, &queue, format, &callback, false);
                let maximum = before
                    .iter()
                    .zip(&after)
                    .map(|(&a, &b)| a.abs_diff(b))
                    .max()
                    .unwrap();
                // An extra 8-bit resolve can round a channel by one level. Larger
                // changes indicate a colour-space, alpha, viewport, or depth error.
                assert!(
                    maximum <= 2,
                    "{format:?}, {rect:?}: maximum channel error {maximum}"
                );
                assert!(
                    after
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|pixel| pixel.as_slice() != &after[..4]),
                    "scene must contain visible geometry"
                );
            }
        }
    }
}

#[test]
#[ignore = "requires a GPU adapter to check the room switch against the depth buffer"]
fn room_switch_keeps_footprints_visible_and_opens_the_dashed_edges_smoothly() {
    use crate::scene3d::{camera::Preset, room::RoomView, switch::InRoom};

    let (device, queue) = device_and_queue();
    let geometry = crate::room_package::RoomGeometry::parse(include_str!(
        "../../../tests/fixtures/room-reference-7x6x3.2-corrected-v1.json"
    ))
    .unwrap();
    let room = RoomView::new(&geometry, Some(1.0));
    let rect = Rect::from_min_max(pos2(12.0, 10.0), pos2(150.0, 120.0));
    let build = |camera: &Camera, progress, objects: &[SceneObject<'_>]| {
        let mut mesh = MeshBuilder::default();
        scene::build(
            &mut mesh,
            camera,
            rect.height(),
            SceneInput {
                objects,
                room: Some(InRoom {
                    view: &room,
                    progress,
                }),
                ..Default::default()
            },
        );
        mesh
    };
    let capture = |camera: &Camera, mesh: &MeshBuilder| {
        let callback = scene_callback(rect, camera, mesh);
        render(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8Unorm,
            &callback,
            false,
        )
    };
    let changed_pixels = |a: &[u8], b: &[u8]| {
        a.as_chunks::<4>()
            .0
            .iter()
            .zip(b.as_chunks::<4>().0)
            .filter(|(a, b)| (0..3).any(|channel| a[channel].abs_diff(b[channel]) > 10))
            .count()
    };

    let camera = Camera::default();
    let object = SceneObject {
        position: [0.45, 0.32, 0.38],
        active: true,
        gain: 1.0,
        presence: 1.0,
        ..Default::default()
    };
    for progress in [0.31, 0.32] {
        let room_decals = build(&camera, progress, &[]).decal.len();
        let mut mesh = build(&camera, progress, &[object]);
        let with_mark = capture(&camera, &mesh);
        mesh.decal.truncate(room_decals);
        let visible = changed_pixels(&with_mark, &capture(&camera, &mesh));
        assert!(
            visible >= 4,
            "only {visible} footprint pixels visible at {progress}"
        );
    }

    // From directly behind, the old solid wire and the finished dashes
    // would have exactly equal depth. Adjacent frames must agree across
    // the handover, allowing a few MSAA samples to cross a pixel boundary.
    let mut back = Camera::default();
    back.apply_preset(Preset::Back, rect.width() / rect.height());
    back.advance(2.0);
    let before = capture(&back, &build(&back, 0.6999, &[]));
    let after = capture(&back, &build(&back, 0.7, &[]));
    let changed = changed_pixels(&before, &after);
    assert!(
        changed <= 4,
        "{changed} pixels jumped as the box finished opening its dashes"
    );
}
