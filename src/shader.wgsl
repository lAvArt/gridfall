struct U {
    inv_vp: mat4x4<f32>,
    eye: vec4<f32>,
    params: vec4<f32>,          // time, aspect, 0, 0
    pulse: array<vec4<f32>, 4>, // xy origin (world xz), z radius, w strength
    pcol: array<vec4<f32>, 4>,
    extra: vec4<f32>,           // x: flat-background ghost (1→0), y: tron colour level, z: grid drift, w: 1 = no floor seams (transit)
    wall_vp: mat4x4<f32>,       // the opening camera: where the wallpaper filled the first frame
    wall: vec4<f32>,            // x: wallpaper opacity, y: its aspect, z: lAvArt hairline grid on
};
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var wallpaper: texture_2d<f32>;

// ───────────────────────── geometry batches ─────────────────────────
struct VIn {
    @location(0) pos: vec4<f32>,
    @location(1) col: vec4<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) kind: f32,
};
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) col: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) @interpolate(flat) kind: f32,
};

@vertex
fn vs_main(v: VIn) -> VOut {
    var o: VOut;
    o.pos = v.pos;
    o.col = v.col;
    o.uv = v.uv;
    o.kind = v.kind;
    return o;
}

@fragment
fn fs_main(i: VOut) -> @location(0) vec4<f32> {
    let k = i.kind;
    let c = i.col;
    let base = c.rgb * c.a;
    if (k < 0.5) {
        // soft light beam: uv.y across [-1,1]
        let v = i.uv.y;
        let v2 = v * v;
        let glow = 0.10 * exp(-v2 * 4.0) + 1.0 * exp(-v2 * 28.0);
        let core = exp(-v2 * 70.0);
        return vec4<f32>(c.rgb * glow * c.a + vec3<f32>(core) * c.a * 0.55 * (0.4 + 0.6 * c.rgb), 1.0);
    } else if (k < 1.5) {
        return vec4<f32>(base, 1.0);
    } else if (k < 2.5) {
        let cov = textureSampleLevel(atlas, samp, i.uv, 0.0).r;
        return vec4<f32>(c.rgb * cov * c.a, 1.0);
    } else if (k < 3.5) {
        // light-wall ribbon: uv.y 0 (floor) → 1 (top)
        let h = i.uv.y;
        let top = exp(-(1.0 - h) * 70.0);
        let foot = exp(-h * 50.0);
        let body = 0.06 * (1.0 - h) + 0.02;
        let I = 0.95 * top + 0.45 * foot + body;
        return vec4<f32>(c.rgb * I * c.a + vec3<f32>(top * 0.35) * c.a, 1.0);
    } else if (k < 4.5) {
        // tumbling voxel face
        let e = max(abs(i.uv.x), abs(i.uv.y));
        let ring = smoothstep(0.55, 0.82, e) * (1.0 - smoothstep(0.92, 1.0, e));
        let I = ring * 1.3 + 0.20;
        return vec4<f32>(c.rgb * I * c.a + vec3<f32>(ring * 0.25) * c.a, 1.0);
    } else if (k < 5.5) {
        // radial glow
        let r = length(i.uv);
        let I = exp(-r * r * 10.0) + 1.4 * exp(-r * r * 50.0);
        return vec4<f32>(c.rgb * I * c.a, 1.0);
    } else if (k < 6.5) {
        // anamorphic streak
        let I = exp(-i.uv.y * i.uv.y * 90.0) * exp(-abs(i.uv.x) * 2.2);
        return vec4<f32>(c.rgb * I * c.a, 1.0);
    } else if (k < 7.5) {
        // vignette + fade (alpha pipeline)
        let r = length(i.uv);
        let vig = smoothstep(0.62, 1.45, r) * 0.78;
        return vec4<f32>(0.0, 0.0, 0.0, clamp(vig + c.a, 0.0, 1.0));
    }
    // 8: solid alpha-blended (monolith armour)
    return c;
}

// ───────────────────────── floor + sky ─────────────────────────
struct FOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};

@vertex
fn vs_full(@builtin(vertex_index) vi: u32) -> FOut {
    var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var o: FOut;
    o.pos = vec4<f32>(p[vi], 0.0, 1.0);
    o.ndc = p[vi];
    return o;
}

fn hash12(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(123.34, 456.21));
    q += dot(q, q + 45.32);
    return fract(q.x * q.y);
}

fn haze(dir: vec3<f32>) -> vec3<f32> {
    // Ahead of the scan front: neutral grey physical mist. Behind it: teal haze of the digital layer.
    let zf = max(dir.z, 0.12);
    let xf = u.eye.x + dir.x / zf * 200.0;
    let s = clamp((u.params.z + 40.0 - xf) / 80.0, 0.0, 1.0);
    let sm = s * s * (3.0 - 2.0 * s);
    let phys = vec3<f32>(0.0105, 0.0108, 0.0112);     // physical: graphite mist
    let teal = mix(vec3<f32>(0.0070, 0.0072, 0.0078), vec3<f32>(0.0095, 0.0125, 0.0230), u.extra.y);   // graphite → #0B2E33 haze
    return mix(phys, teal, sm);
}

// seam lines: `wpx` is the line thickness in pixels
fn gridline(q: vec2<f32>, cell: f32, wpx: f32) -> f32 {
    let c = q / cell;
    let w = fwidth(c);
    let g = abs(fract(c - 0.5) - 0.5) / max(w, vec2<f32>(1e-5));
    let l = 1.0 - min(min(g.x, g.y) / wpx, 1.0);
    return l * (1.0 - smoothstep(0.05, 0.12, max(w.x, w.y)));
}

fn vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u2 = f * f * (3.0 - 2.0 * f);
    return mix(mix(hash12(i), hash12(i + vec2<f32>(1.0, 0.0)), u2.x),
               mix(hash12(i + vec2<f32>(0.0, 1.0)), hash12(i + vec2<f32>(1.0, 1.0)), u2.x), u2.y);
}

@fragment
fn fs_floor(i: FOut) -> @location(0) vec4<f32> {
    let n0 = u.inv_vp * vec4<f32>(i.ndc, 0.0, 1.0);
    let n1 = u.inv_vp * vec4<f32>(i.ndc, 1.0, 1.0);
    let dir = normalize(n1.xyz / n1.w - n0.xyz / n0.w);
    let eye = u.eye.xyz;

    let dy = min(dir.y, -1e-3);
    let t = -eye.y / dy;
    let q = eye.xz + dir.xz * t;

    let scan = u.params.z;
    let dg = clamp((scan + 6.0 - q.x) / 12.0, 0.0, 1.0);
    let dig = dg * dg * (3.0 - 2.0 * dg);       // 1 = digitised (behind the scan front)
    // a few wide seams instead of a dense grid (cf. the Ares hall floor)
    let seam = gridline(q, 30.0, 2.6) * dig * (1.0 - u.extra.w);
    // physical world ahead of the scan: organic terrain contours
    let hgt = vnoise(q * 0.045) * 0.65 + vnoise(q * 0.12) * 0.35;
    let hb = hgt * 16.0;
    let band = abs(fract(hb) - 0.5) / max(fwidth(hb), 1e-4);
    let contour = (1.0 - min(band, 1.0)) * (1.0 - smoothstep(0.2, 0.5, fwidth(hb)));
    let hz = haze(dir);

    // floor
    let fog = exp(-t * 0.0085);
    let tron = u.extra.y;
    let ghost = u.extra.x;
    // #121212 once tone-mapped; becomes teal-black gloss as the Tron palette comes in
    var col = mix(vec3<f32>(0.006, 0.007, 0.010), mix(vec3<f32>(0.0064), vec3<f32>(0.0085, 0.0105, 0.0185), tron), dig);
    col += mix(vec3<f32>(0.10), vec3<f32>(0.30, 0.50, 0.85), tron) * seam * 0.10 * smoothstep(0.0, 0.5, tron) * fog;
    // the desktop background's drifting hairline grid (48px cells, every 4th emphasised)
    let gq = vec2<f32>(q.x + u.extra.z, q.y - u.extra.z);
    let hair = gridline(gq, 2.65, 1.0) * 0.0027 + gridline(gq, 10.6, 1.0) * 0.0058;
    col += vec3<f32>(1.0) * hair * ghost * dig * u.wall.z;
    for (var k = 0; k < 4; k = k + 1) {
        let p = u.pulse[k];
        if (p.w > 0.0) {
            let d = length(q - p.xy);
            // laser-thin ring: hard edge (one pixel of AA), no soft halo, kept faint
            let aa = max(fwidth(d), 1e-3);
            let ring = (1.0 - smoothstep(0.07, 0.07 + aa, abs(d - p.z))) * p.w;
            col += u.pcol[k].rgb * ring * (0.07 + 0.6 * seam) * exp(-p.z * 0.014);
        }
    }
    col += vec3<f32>(0.40, 0.38, 0.34) * contour * (1.0 - dig) * 0.10 * fog;
    // the scan front itself
    let sd = (q.x - scan);
    col += vec3<f32>(0.30, 0.78, 1.0) * (1.0 * exp(-sd * sd / 0.06) + 0.012 * exp(-sd * sd / 6.0)) * smoothstep(10.0, 45.0, t) * exp(-t * 0.004);
    // wet-glass sheen toward the horizon
    col += hz * pow(1.0 - clamp(-dir.y, 0.0, 1.0), 6.0) * 0.30;
    col = mix(hz, col, mix(fog, 1.0, ghost));

    // sky
    let sky = hz * (0.75 + 0.25 * exp(-max(dir.y, 0.0) * 7.0)) + vec3<f32>(0.0002, 0.0003, 0.0006);

    // the user's wallpaper, pinned to the floor where it filled the opening frame (cover-fit, like the desktop),
    // written through the inverse tone curve so it reads exactly as on the desktop
    if (u.wall.x > 0.0 && dir.y < 0.0) {
        let c = u.wall_vp * vec4<f32>(q.x, 0.0, q.y, 1.0);
        if (c.w > 0.0) {
            let ndc = c.xy / c.w;
            var uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
            let sa = u.params.y;
            let ia = u.wall.y;
            if (ia > sa) {
                uv.x = (uv.x - 0.5) * sa / ia + 0.5;
            } else {
                uv.y = (uv.y - 0.5) * ia / sa + 0.5;
            }
            let edge = min(min(uv.x, 1.0 - uv.x), min(uv.y, 1.0 - uv.y));
            let m = smoothstep(0.0, 0.003, edge);
            let lin = textureSampleLevel(wallpaper, samp, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0).rgb;
            let hdr = -log(vec3<f32>(1.0) - min(lin, vec3<f32>(0.995))) / 1.35;
            col = mix(col, hdr, u.wall.x * m);
        }
    }
    var outc = select(sky, col, dir.y < 0.0);
    outc += (hash12(i.pos.xy + u.params.x) - 0.5) * 0.0007;
    return vec4<f32>(max(outc, vec3<f32>(0.0)), 1.0);
}
