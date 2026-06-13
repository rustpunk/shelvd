//! A tiny instanced-free solid-rectangle layer: cell backgrounds, the cursor,
//! and (later) selections. Vertices are generated on the CPU each frame in
//! physical pixels; a uniform carries the surface resolution so the vertex
//! shader can map pixels → clip space.

use bytemuck::{Pod, Zeroable};

/// A filled rectangle in physical pixels with a linear-light RGBA color.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RectVertex {
    pos: [f32; 2],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    _pad: [f32; 2],
}

const SHADER: &str = r#"
struct Globals { resolution: vec2<f32>, _pad: vec2<f32> };
@group(0) @binding(0) var<uniform> globals: Globals;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>,
};
struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    let ndc = vec2<f32>(
        in.pos.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - in.pos.y / globals.resolution.y * 2.0,
    );
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

pub struct RectRenderer {
    pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    vbuf_cap_bytes: u64,
    vertex_count: u32,
}

impl RectRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shelvd rect shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shelvd rect globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("shelvd rect bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("shelvd rect bg"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shelvd rect pl"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<RectVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("shelvd rect pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[vertex_layout],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        let vbuf_cap_bytes = (std::mem::size_of::<RectVertex>() * 6 * 256) as u64;
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shelvd rect vertices"),
            size: vbuf_cap_bytes,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            globals_buf,
            bind_group,
            vbuf,
            vbuf_cap_bytes,
            vertex_count: 0,
        }
    }

    /// Update the resolution uniform (call on resize).
    pub fn set_resolution(&self, queue: &wgpu::Queue, width: f32, height: f32) {
        let globals = Globals { resolution: [width, height], _pad: [0.0, 0.0] };
        queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));
    }

    /// Upload this frame's rectangles, growing the vertex buffer if needed.
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, rects: &[Rect]) {
        let mut verts: Vec<RectVertex> = Vec::with_capacity(rects.len() * 6);
        for r in rects {
            let (l, t, rt, b) = (r.x, r.y, r.x + r.w, r.y + r.h);
            let tl = RectVertex { pos: [l, t], color: r.color };
            let tr = RectVertex { pos: [rt, t], color: r.color };
            let bl = RectVertex { pos: [l, b], color: r.color };
            let br = RectVertex { pos: [rt, b], color: r.color };
            verts.extend_from_slice(&[tl, bl, br, tl, br, tr]);
        }
        self.vertex_count = verts.len() as u32;
        if verts.is_empty() {
            return;
        }

        let needed = (verts.len() * std::mem::size_of::<RectVertex>()) as u64;
        if needed > self.vbuf_cap_bytes {
            let new_cap = needed.next_power_of_two();
            self.vbuf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("shelvd rect vertices"),
                size: new_cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.vbuf_cap_bytes = new_cap;
        }
        queue.write_buffer(&self.vbuf, 0, bytemuck::cast_slice(&verts));
    }

    /// Draw the uploaded rectangles into an in-progress render pass.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.vertex_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.vbuf.slice(..));
        pass.draw(0..self.vertex_count, 0..1);
    }
}
