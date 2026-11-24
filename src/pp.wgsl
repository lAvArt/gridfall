// Post-processing: bright-pass/downsample, separable blur, composite + tonemap.
struct P { v: vec4<f32> }; // xy: texel step (down: src texel, blur: direction*texel), z: threshold, w: bloom hold-back (composite)
@group(0) @binding(0) var t0: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<uniform> p: P;
@group(0) @binding(3) var t1: texture_2d<f32>;
@group(0) @binding(4) var t2: texture_2d<f32>;
@group(0) @binding(5) var hud: texture_2d<f32>; // full-resolution HUD layer (composite only)

struct FOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs_full(@builtin(vertex_index) vi: u32) -> FOut {
    var q = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var o: FOut;
    o.pos = vec4<f32>(q[vi], 0.0, 1.0);
    o.uv = vec2<f32>(q[vi].x * 0.5 + 0.5, 0.5 - q[vi].y * 0.5);
    return o;
}

fn bright(c: vec3<f32>, thr: f32) -> vec3<f32> {
    let m = max(max(c.r, c.g), c.b);
    return c * (max(m - thr, 0.0) / max(m, 1e-4));
}

@fragment
fn fs_down(i: FOut) -> @location(0) vec4<f32> {
    let s = p.v.xy;
    var c = textureSampleLevel(t0, samp, i.uv + s * vec2<f32>(-1.0, -1.0), 0.0).rgb;
    c += textureSampleLevel(t0, samp, i.uv + s * vec2<f32>(1.0, -1.0), 0.0).rgb;
    c += textureSampleLevel(t0, samp, i.uv + s * vec2<f32>(-1.0, 1.0), 0.0).rgb;
    c += textureSampleLevel(t0, samp, i.uv + s * vec2<f32>(1.0, 1.0), 0.0).rgb;
    c *= 0.25;
    return vec4<f32>(select(c, bright(c, p.v.z), p.v.z > 0.0), 1.0);
}

@fragment
fn fs_blur(i: FOut) -> @location(0) vec4<f32> {
    // 9-tap gaussian via 5 bilinear taps
    let d = p.v.xy;
    var c = textureSampleLevel(t0, samp, i.uv, 0.0).rgb * 0.2270270270;
    c += textureSampleLevel(t0, samp, i.uv + d * 1.3846153846, 0.0).rgb * 0.3162162162;
    c += textureSampleLevel(t0, samp, i.uv - d * 1.3846153846, 0.0).rgb * 0.3162162162;
    c += textureSampleLevel(t0, samp, i.uv + d * 3.2307692308, 0.0).rgb * 0.0702702703;
    c += textureSampleLevel(t0, samp, i.uv - d * 3.2307692308, 0.0).rgb * 0.0702702703;
    return vec4<f32>(c, 1.0);
}

fn hash12(q: vec2<f32>) -> f32 {
    var x = fract(q * vec2<f32>(123.34, 456.21));
    x += dot(x, x + 45.32);
    return fract(x.x * x.y);
}

// everything that reaches the screen at uv: scene, bloom and the full-resolution HUD layer
fn grab(uv: vec2<f32>) -> vec3<f32> {
    let scene = textureSampleLevel(t0, samp, uv, 0.0).rgb;
    let b1 = textureSampleLevel(t1, samp, uv, 0.0).rgb;
    let b2 = textureSampleLevel(t2, samp, uv, 0.0).rgb;
    // p.v.w: bloom held back while the desktop wallpaper is on screen, so it reads exactly as on the desktop
    return scene + (b1 * 0.26 + b2 * 0.14) * (1.0 - p.v.w) + textureSampleLevel(hud, samp, uv, 0.0).rgb;
}

@fragment
fn fs_comp(i: FOut) -> @location(0) vec4<f32> {
    var hdr: vec3<f32>;
    if (p.v.x > 0.5) {
        // CRT (the ASCII style): a bulging glass, scanlines, a little colour fringing, flicker and a slow roll bar
        let c = i.uv * 2.0 - vec2<f32>(1.0);
        let r2 = dot(c, c);
        let q = c * (1.0 + 0.040 * r2);
        let uv = q * 0.5 + vec2<f32>(0.5);
        if (abs(q.x) > 1.0 || abs(q.y) > 1.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 1.0);
        }
        let fringe = vec2<f32>(0.0007, 0.0) * (0.4 + r2);
        var col = vec3<f32>(grab(uv + fringe).r, grab(uv).g, grab(uv - fringe).b);
        let rows = f32(textureDimensions(hud).y);
        let scan = 0.80 + 0.20 * cos(uv.y * rows * 2.0943951); // one dark line in every three
        let roll = 1.0 + 0.07 * smoothstep(0.0, 0.5, 1.0 - abs(fract(uv.y * 0.9 - p.v.y * 0.07) - 0.5) * 2.0);
        let flick = 1.0 + 0.012 * sin(p.v.y * 97.0) + 0.006 * sin(p.v.y * 13.0);
        let vig = 1.0 - 0.38 * smoothstep(0.35, 1.5, r2);
        // the glass edge falls off before the bezel
        let edge = smoothstep(1.0, 0.985, max(abs(q.x), abs(q.y)));
        hdr = col * scan * roll * flick * vig * edge;
    } else {
        hdr = grab(i.uv);
    }
    // soft shoulder: highlights roll toward white-hot, shadows stay deep
    var c = vec3<f32>(1.0) - exp(-hdr * 1.35);
    c += (hash12(i.pos.xy) - 0.5) * 0.0010;
    return vec4<f32>(max(c, vec3<f32>(0.0)), 1.0);
}
