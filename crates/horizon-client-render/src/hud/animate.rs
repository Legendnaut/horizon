//! The GUI atlas's animated sprites, frame by frame (vanilla's or a Polytone `mode`'s).
//!
//! Where compute shaders can write the atlas, every frame of every animation is uploaded
//! once at its size in the atlas, and a tick that changes sprites writes one small job per
//! changed sprite and runs one dispatch (`hud_animate.wgsl`). Copying each changed sprite on
//! its own (a copy command each, a blit job each in Mesa's V3DV) cost the Raspberry Pi 4
//! about 15 ms a frame for some 55 sprites, the frame's commands taking longer to record than
//! the GPU took to draw it. Without compute (OpenGL before 4.3) each changed sprite is still
//! mixed on the CPU and copied.

use wgpu::util::DeviceExt;

/// Bytes of `hud_animate.wgsl`'s `Job`.
const JOB_BYTES: usize = 32;
/// `hud_animate.wgsl`'s workgroup edge.
const WORKGROUP: u32 = 8;

/// The animations' frames on the GPU and what writes them into the atlas.
pub(crate) struct Gpu {
    pipeline: wgpu::ComputePipeline,
    bind: wgpu::BindGroup,
    jobs: wgpu::Buffer,
    /// Each sprite's frames: the word offset of each in the frames buffer.
    frames: Vec<Vec<u32>>,
    /// The largest sprite's texels on a side, for the dispatch.
    widest: u32,
    tallest: u32,
    /// This tick's jobs.
    pending: Vec<u8>,
}

impl Gpu {
    /// The frames of `animations` at their size in `texture`, or `None` (the CPU copies them)
    /// when the device makes no pipeline or they don't fit in one storage buffer.
    fn new(
        device: &wgpu::Device,
        texture: &wgpu::Texture,
        animations: &[(
            super::Rect,
            crate::sprites::SpriteAnimation,
            (usize, usize, u32),
        )],
    ) -> Option<Self> {
        if animations.is_empty()
            || !texture
                .usage()
                .contains(wgpu::TextureUsages::STORAGE_BINDING)
        {
            return None;
        }
        let size = (texture.width(), texture.height());
        let mut words: Vec<u8> = Vec::new();
        let mut frames = Vec::with_capacity(animations.len());
        let (mut widest, mut tallest) = (1, 1);
        for (rect, animation, _) in animations {
            let (w, h) = rect.texels(size);
            widest = widest.max(w);
            tallest = tallest.max(h);
            let offsets = (0..animation.frame_count())
                .map(|frame| {
                    let offset = (words.len() / 4) as u32;
                    words.extend_from_slice(&animation.resized_pixels(frame, w, h));
                    offset
                })
                .collect();
            frames.push(offsets);
        }
        let limits = device.limits();
        let limit = limits
            .max_storage_buffer_binding_size
            .min(limits.max_buffer_size);
        if words.len() as u64 > limit {
            log::warn!(
                "Horizon: the GUI atlas's animations ({} bytes) do not fit in a storage buffer; \
                 copied from the CPU",
                words.len()
            );
            return None;
        }
        let pipeline = pipeline(device)?;
        let frames_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("GUI atlas animation frames"),
            contents: &words,
            usage: wgpu::BufferUsages::STORAGE,
        });
        let jobs = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("GUI atlas animation jobs"),
            size: (animations.len() * JOB_BYTES) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let view = texture.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("GUI atlas animation"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: frames_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: jobs.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
            ],
        });
        Some(Self {
            pipeline,
            bind,
            jobs,
            frames,
            widest,
            tallest,
            pending: Vec::with_capacity(animations.len() * JOB_BYTES),
        })
    }
}

/// `hud_animate.wgsl`'s pipeline, unless the device fails to make it.
fn pipeline(device: &wgpu::Device) -> Option<wgpu::ComputePipeline> {
    let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("GUI atlas animation"),
        source: wgpu::ShaderSource::Wgsl(include_str!("hud_animate.wgsl").into()),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("GUI atlas animation"),
        layout: None,
        module: &module,
        entry_point: Some("animate"),
        compilation_options: Default::default(),
        cache: None,
    });
    // Asked inside the scopes: an invalid pipeline reports itself here.
    let _ = pipeline.get_bind_group_layout(0);
    let internal = pollster::block_on(internal.pop());
    match internal.or(pollster::block_on(validation.pop())) {
        None => Some(pipeline),
        Some(e) => {
            log::warn!("Horizon renderer: GUI atlas animations copied from the CPU ({e})");
            None
        }
    }
}

impl super::HudRenderer {
    /// The atlas's animated sprites (`HudAtlas::sprite_animations`), shown from their next
    /// [`animate`](Self::animate).
    pub fn set_sprite_animations(
        &mut self,
        animations: &[(super::Rect, crate::sprites::SpriteAnimation)],
    ) {
        self.sprite_animations = animations
            .iter()
            .map(|(rect, animation)| (*rect, animation.clone(), (usize::MAX, 0, 0)))
            .collect();
        self.animation_gpu = None;
        self.animation_gpu_tried = false;
    }

    /// Shows each animated sprite's frame: its mcmeta's, or what its Polytone `mode` picks.
    pub(crate) fn animate(
        &mut self,
        device: &wgpu::Device,
        uploads: &mut crate::gfx::upload::Uploads,
        encoder: &mut wgpu::CommandEncoder,
        tick: u64,
        animator: &crate::gfx::animate::SpriteAnimator,
    ) {
        let inputs = animator.mode_inputs();
        let inputs = Some(&inputs);
        let Some(texture) = &self.atlas_texture else {
            return;
        };
        if !self.animation_gpu_tried {
            self.animation_gpu_tried = true;
            self.animation_gpu = Gpu::new(device, texture, &self.sprite_animations);
        }
        for (index, (rect, animation, previous)) in self.sprite_animations.iter_mut().enumerate() {
            // `SpriteContents.AnimationState`: the frame from the mcmeta's
            // list, blended into the next one by the sub-frame's thousandths
            // when the sprite interpolates; or what a Polytone `mode` picks.
            let blend = animation.blend_with(tick, inputs);
            let state = (blend.frame, blend.next, blend.millis);
            if state == *previous {
                continue;
            }
            *previous = state;
            let (w, h) = rect.texels((texture.width(), texture.height()));
            let x = (rect.u0 * texture.width() as f32).round() as u32;
            let y = (rect.v0 * texture.height() as f32).round() as u32;
            if let Some(gpu) = &mut self.animation_gpu {
                let frames = &gpu.frames[index];
                let job = [
                    x,
                    y,
                    w,
                    h,
                    frames[blend.frame],
                    frames[blend.next],
                    blend.millis,
                    0,
                ];
                for word in job {
                    gpu.pending.extend_from_slice(&word.to_le_bytes());
                }
                continue;
            }
            let pixels = if blend.millis == 0 {
                animation.resized_pixels(blend.frame, w, h)
            } else {
                let current = animation.resized_pixels(blend.frame, w, h);
                let next = animation.resized_pixels(blend.next, w, h);
                let millis = blend.millis;
                current
                    .iter()
                    .zip(next)
                    .map(|(&a, b)| {
                        ((u32::from(a) * (1000 - millis) + u32::from(b) * millis + 500) / 1000)
                            as u8
                    })
                    .collect()
            };
            uploads.texture(
                encoder,
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                w,
                h,
                &pixels,
            );
        }
        let Some(gpu) = &mut self.animation_gpu else {
            return;
        };
        let count = (gpu.pending.len() / JOB_BYTES) as u32;
        if count == 0 {
            return;
        }
        uploads.buffer(encoder, &gpu.jobs, 0, &gpu.pending);
        gpu.pending.clear();
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("GUI atlas animation"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&gpu.pipeline);
        pass.set_bind_group(0, &gpu.bind, &[]);
        pass.dispatch_workgroups(
            gpu.widest.div_ceil(WORKGROUP),
            gpu.tallest.div_ceil(WORKGROUP),
            count,
        );
    }
}
