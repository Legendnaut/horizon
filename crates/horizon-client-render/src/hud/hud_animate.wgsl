// The GUI atlas's animated sprites (hud/animate.rs): each job writes its sprite's
// frame, or the frame mixed with the next one, into its rect of the atlas, as
// `HudRenderer::animate` does on the CPU. One invocation per texel; the job is
// the z coordinate.
struct Job {
    // The rect's corner and size in atlas texels.
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    // Word offsets of the two frames (at the rect's size) in `frames`.
    current: u32,
    next: u32,
    // Thousandths of the way from `current` to `next` (0: `current` alone).
    millis: u32,
    pad: u32,
}
@group(0) @binding(0) var<storage, read> frames: array<u32>;
@group(0) @binding(1) var<storage, read> jobs: array<Job>;
@group(0) @binding(2) var atlas: texture_storage_2d<rgba8unorm, write>;

fn channels(rgba: u32) -> vec4<u32> {
    return vec4(rgba & 0xffu, (rgba >> 8u) & 0xffu, (rgba >> 16u) & 0xffu, rgba >> 24u);
}

@compute @workgroup_size(8, 8)
fn animate(@builtin(global_invocation_id) id: vec3<u32>) {
    let job = jobs[id.z];
    if id.x >= job.w || id.y >= job.h {
        return;
    }
    let texel = id.y * job.w + id.x;
    var color = channels(frames[job.current + texel]);
    if job.millis > 0u {
        // mix(current, next, millis / 1000), rounded to the nearest 8-bit
        // value (halves up) in integers, as the CPU mixes it.
        let next = channels(frames[job.next + texel]);
        color = (color * (1000u - job.millis) + next * job.millis + 500u) / 1000u;
    }
    textureStore(atlas, vec2(job.x + id.x, job.y + id.y), vec4<f32>(color) / 255.0);
}
