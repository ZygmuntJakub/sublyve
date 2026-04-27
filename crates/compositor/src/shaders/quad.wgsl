struct Uniforms {
    // NDC scale for letterbox/pillarbox.
    scale: vec2<f32>,
    // Per-layer opacity, applied to the source alpha so every blend mode
    // honours the layer's opacity slider uniformly.
    opacity: f32,
    _pad: f32,
};

@group(0) @binding(0) var t_diffuse: texture_2d<f32>;
@group(0) @binding(1) var s_diffuse: sampler;
@group(0) @binding(2) var<uniform> u: Uniforms;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.position = vec4<f32>(in.position * u.scale, 0.0, 1.0);
    out.uv = in.uv;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Output premultiplied alpha so the fixed-function blend states in
    // VideoPipelines compute the correct formula for every BlendMode.
    let s = textureSample(t_diffuse, s_diffuse, in.uv);
    let a = s.a * u.opacity;
    return vec4<f32>(s.rgb * a, a);
}
