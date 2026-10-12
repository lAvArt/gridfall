//! Turns simulation state into GPU quads. Every primitive is a quad; the wet
//! mirror floor is the same geometry re-projected with y flipped (no extra pass).

use crate::brand::{self, Role};
use crate::sim::{trace, Packet, Scenario, Shape, Sim, Team, ARENA_X, ARENA_Z0, ARENA_Z1, EMERGE_CELL, EMERGE_MARK_W, EMERGE_ZB, FLAT_ZB, MARK_Y, MARK_Z};
use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Quat, Vec2, Vec3, Vec4};
use std::collections::HashMap;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 4],
    pub col: [f32; 4],
    pub uv: [f32; 2],
    pub kind: f32,
    pub _pad: f32,
}

// Shader `kind` ids
const K_BEAM: f32 = 0.0;
const K_SOLID: f32 = 1.0;
const K_TEXT: f32 = 2.0;
const K_WALL: f32 = 3.0;
const K_SHARD: f32 = 4.0;
const K_GLOW: f32 = 5.0;
const K_STREAK: f32 = 6.0;
const K_ARMOUR: f32 = 8.0;

#[derive(Clone, Copy, PartialEq)]
enum Layer {
    Pre,  // additive, behind the monolith armour
    Fill, // alpha-blended
    Add,  // additive, in front
}

#[derive(Default)]
pub struct Frame {
    pub m: Vec<Vertex>, // mirrored reflections
    pub p: Vec<Vertex>,
    pub b: Vec<Vertex>,
    pub a: Vec<Vertex>,
    pub c: Vec<Vertex>, // vignette / fade
    pub h: Vec<Vertex>, // HUD and labels: drawn at full resolution on their own layer
}

impl Frame {
    pub fn clear(&mut self) {
        self.m.clear();
        self.p.clear();
        self.b.clear();
        self.a.clear();
        self.c.clear();
        self.h.clear();
    }
}

// ───────────────────────── glyph atlas ─────────────────────────
#[derive(Clone, Copy)]
pub struct Glyph {
    u0: f32,
    v0: f32,
    du: f32,
    dv: f32,
    /// advance as a fraction of the glyph cell's edge
    adv: f32,
}

pub struct Atlas {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
    map: HashMap<(u8, char), Glyph>,
}

const F_MONO: u8 = 0;
const F_DISPLAY: u8 = 1;

fn blit(data: &mut [u8], aw: u32, cx: u32, cy: u32, cell: u32, base: i32, m: &fontdue::Metrics, bmp: &[u8]) {
    let top = base - m.ymin - m.height as i32;
    for y in 0..m.height as i32 {
        for x in 0..m.width as i32 {
            let (px, py) = (m.xmin.max(0) + x, top + y);
            if (0..cell as i32).contains(&px) && (0..cell as i32).contains(&py) {
                data[((cy as i32 + py) as u32 * aw + cx + px as u32) as usize] = bmp[(y * m.width as i32 + x) as usize];
            }
        }
    }
}

impl Atlas {
    pub fn build() -> Self {
        // region A: HUD monospace, 48px cells · region B: display face for the lockup, 128px cells
        const CA: u32 = 48;
        const COLS_A: u32 = 16;
        const CB: u32 = 128;
        const COLS_B: u32 = 8;
        let mono: Vec<char> = (32u8..127).map(|b| b as char).chain("●·©—§×".chars()).collect();
        let disp: Vec<char> = std::iter::once(' ').chain(brand::get().wordmark.iter().flat_map(|l| l.chars())).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        let rows_a = (mono.len() as u32).div_ceil(COLS_A);
        let rows_b = (disp.len() as u32).div_ceil(COLS_B);
        let w = (COLS_A * CA).max(COLS_B * CB);
        let h = rows_a * CA + rows_b * CB;
        let mut data = vec![0u8; (w * h) as usize];
        let mut map = HashMap::new();
        let load = |paths: &[&str]| {
            paths
                .iter()
                .find_map(|p| std::fs::read(p).ok())
                .and_then(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok())
        };
        let mono_font = load(&[
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Bold.ttf",
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        ]);
        let home = std::env::var("HOME").unwrap_or_default();
        let sg = format!("{home}/.local/share/fonts/SpaceGrotesk-Variable.ttf");
        let disp_font = load(&[sg.as_str()]).or_else(|| load(&["/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf"]));

        if let Some(font) = &mono_font {
            for (i, &ch) in mono.iter().enumerate() {
                let (m, bmp) = font.rasterize(ch, 40.0);
                let (cx, cy) = ((i as u32 % COLS_A) * CA, (i as u32 / COLS_A) * CA);
                blit(&mut data, w, cx, cy, CA, 38, &m, &bmp);
                map.insert(
                    (F_MONO, ch),
                    Glyph { u0: cx as f32 / w as f32, v0: cy as f32 / h as f32, du: CA as f32 / w as f32, dv: CA as f32 / h as f32, adv: font.metrics('M', 40.0).advance_width / CA as f32 },
                );
            }
        }
        if let Some(font) = &disp_font {
            for (i, &ch) in disp.iter().enumerate() {
                let (m, bmp) = font.rasterize(ch, 100.0);
                let (cx, cy) = ((i as u32 % COLS_B) * CB, rows_a * CA + (i as u32 / COLS_B) * CB);
                blit(&mut data, w, cx, cy, CB, 100, &m, &bmp);
                map.insert(
                    (F_DISPLAY, ch),
                    Glyph { u0: cx as f32 / w as f32, v0: cy as f32 / h as f32, du: CB as f32 / w as f32, dv: CB as f32 / h as f32, adv: m.advance_width / CB as f32 },
                );
            }
        }
        Atlas { w, h, data, map }
    }
}

// ───────────────────────── painter ─────────────────────────
struct Painter<'a> {
    vp: Mat4,
    eye: Vec3,
    right: Vec3,
    up: Vec3,
    w: f32,
    h: f32,
    f: &'a mut Frame,
    atlas: &'a Atlas,
}

impl Painter<'_> {
    /// Screen position (pixels, origin top-left) of a world point, if it is in front of the camera.
    fn to_screen(&self, p: Vec3) -> Option<(f32, f32)> {
        let c = self.vp * p.extend(1.0);
        (c.w > 0.3).then(|| ((c.x / c.w * 0.5 + 0.5) * self.w, (0.5 - c.y / c.w * 0.5) * self.h))
    }

    fn clip(&self, p: Vec3, mirror: bool) -> Vec4 {
        let p = if mirror { Vec3::new(p.x, -p.y, p.z) } else { p };
        self.vp * p.extend(1.0)
    }

    fn push(&mut self, layer: Layer, mirror: bool, pts: [Vec3; 4], uv: [[f32; 2]; 4], col: [Vec4; 4], kind: f32) {
        let mk = |s: &Self, mir: bool| -> Option<[Vertex; 4]> {
            let mut out = [Vertex::zeroed(); 4];
            for i in 0..4 {
                let c = s.clip(pts[i], mir);
                if c.w < 0.3 {
                    return None;
                }
                let mut rgb = Vec3::new(col[i].x, col[i].y, col[i].z);
                if mir {
                    rgb *= 0.34 * (1.0 - pts[i].y / 45.0).clamp(0.06, 1.0);
                }
                out[i] = Vertex { pos: c.to_array(), col: [rgb.x, rgb.y, rgb.z, col[i].w], uv: uv[i], kind, _pad: 0.0 };
            }
            Some(out)
        };
        if let Some(v) = mk(self, false) {
            match layer {
                Layer::Pre => self.f.p.extend_from_slice(&v),
                Layer::Fill => self.f.b.extend_from_slice(&v),
                Layer::Add => self.f.a.extend_from_slice(&v),
            }
        }
        if mirror && layer != Layer::Fill {
            if let Some(v) = mk(self, true) {
                self.f.m.extend_from_slice(&v);
            }
        }
    }

    /// Camera-facing light beam from a to b.
    fn beam(&mut self, layer: Layer, a: Vec3, b: Vec3, w: f32, ca: Vec4, cb: Vec4, mirror: bool) {
        let d = (b - a).normalize_or_zero();
        let (a, b) = (a - d * w * 0.5, b + d * w * 0.5);
        let mid = (a + b) * 0.5;
        let perp = d.cross(self.eye - mid).normalize_or_zero() * w;
        self.push(
            layer,
            mirror,
            [a - perp, a + perp, b + perp, b - perp],
            [[0.0, -1.0], [0.0, 1.0], [1.0, 1.0], [1.0, -1.0]],
            [ca, ca, cb, cb],
            K_BEAM,
        );
    }

    /// Hard-edged camera-facing bar: no soft falloff, so it stays a crisp line.
    fn bar(&mut self, layer: Layer, a: Vec3, b: Vec3, w: f32, ca: Vec4, cb: Vec4, mirror: bool) {
        let d = (b - a).normalize_or_zero();
        let mid = (a + b) * 0.5;
        let perp = d.cross(self.eye - mid).normalize_or_zero() * (w * 0.5);
        self.push(layer, mirror, [a - perp, a + perp, b + perp, b - perp], [[0.0; 2]; 4], [ca, ca, cb, cb], K_SOLID);
    }

    /// Vertical light-wall standing on the floor between a and b.
    fn wall(&mut self, a: Vec3, b: Vec3, hgt: f32, ca: Vec4, cb: Vec4) {
        let up = Vec3::Y * hgt;
        self.push(Layer::Add, true, [a, a + up, b + up, b], [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]], [ca, ca, cb, cb], K_WALL);
    }

    /// Camera-facing glow billboard with explicit half-extents.
    fn billboard(&mut self, layer: Layer, c: Vec3, hw: f32, hh: f32, col: Vec4, kind: f32, mirror: bool) {
        let (r, u) = (self.right * hw, self.up * hh);
        self.push(layer, mirror, [c - r - u, c + r - u, c + r + u, c - r + u], [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]], [col; 4], kind);
    }

    // screen-space (pixels, origin top-left)
    fn screen(&mut self, layer: Layer, x: f32, y: f32, w: f32, h: f32, uv: [[f32; 2]; 4], col: Vec4, kind: f32) {
        let cv = |px: f32, py: f32| Vec4::new(px / self.w * 2.0 - 1.0, 1.0 - py / self.h * 2.0, 0.0, 1.0);
        let pts = [cv(x, y + h), cv(x + w, y + h), cv(x + w, y), cv(x, y)];
        let uv4 = [uv[3], uv[2], uv[1], uv[0]];
        let verts: Vec<Vertex> = (0..4)
            .map(|i| Vertex { pos: pts[i].to_array(), col: col.to_array(), uv: uv4[i], kind, _pad: 0.0 })
            .collect();
        match layer {
            Layer::Add | Layer::Pre => self.f.h.extend_from_slice(&verts),
            Layer::Fill => self.f.c.extend_from_slice(&verts),
        }
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, col: Vec4) {
        self.screen(Layer::Add, x, y, w, h, [[0.0; 2]; 4], col, K_SOLID);
    }

    /// `size` is the glyph cell's edge in pixels; `ls` extra letter spacing in pixels.
    fn text_ex(&mut self, font: u8, x: f32, y: f32, size: f32, ls: f32, s: &str, col: Vec4) {
        let at = self.atlas;
        let mut cx = x;
        for ch in s.chars() {
            if let Some(g) = at.map.get(&(font, ch)).copied() {
                self.screen(
                    Layer::Add,
                    cx,
                    y,
                    size,
                    size,
                    [[g.u0, g.v0], [g.u0 + g.du, g.v0], [g.u0 + g.du, g.v0 + g.dv], [g.u0, g.v0 + g.dv]],
                    col,
                    K_TEXT,
                );
                cx += g.adv * size + ls;
            } else {
                cx += 0.5 * size + ls;
            }
        }
    }

    fn text(&mut self, x: f32, y: f32, size: f32, s: &str, col: Vec4) {
        self.text_ex(F_MONO, x, y, size, 0.0, s, col);
    }

    fn text_width_ex(&self, font: u8, size: f32, ls: f32, s: &str) -> f32 {
        s.chars().map(|c| self.atlas.map.get(&(font, c)).map_or(0.5, |g| g.adv) * size + ls).sum()
    }

    fn text_width(&self, size: f32, s: &str) -> f32 {
        self.text_width_ex(F_MONO, size, 0.0, s)
    }
}

// ───────────────────────── scene ─────────────────────────
// Tron palette: programs = Twinise orange, users = blue
/// the theme accent (#E68E0D on the lAvArt desktop)
fn orange() -> Vec3 {
    brand::get().accent
}
/// white-hot light channel colour: the accent run halfway to white (#FFC780-ish for the amber accent)
fn hot_glow() -> Vec3 {
    let o = orange();
    Vec3::new(1.0, 1.0, 1.0).lerp(o, 0.5) + Vec3::new(0.05, 0.0, -0.025)
}
/// the theme's foreground grey, as a display value
fn fg_grey() -> Vec3 {
    brand::get().fg
}
const BLUE: Vec3 = Vec3::new(0.05, 0.50, 1.0);
const BLUE_HOT: Vec3 = Vec3::new(0.55, 0.80, 1.0);
const RIM: Vec3 = Vec3::new(0.55, 0.82, 1.0); // architecture light: white-teal
const SENSE: Vec3 = Vec3::new(0.18, 0.71, 1.0); // sensing / the twin
/// Height of a lightcycle's light wall; the moving actor is exactly this tall.
const TRAIL_H: f32 = 2.4;
const MARK_W: f32 = 27.0;

use crate::sim::logo_floor;

/// A quad with its last corner repeated: how a triangle goes through the quad-only renderer.
fn tri4(t: [Vec3; 3]) -> [Vec3; 4] {
    [t[0], t[1], t[2], t[2]]
}

/// A logo point (2048² logo space) on an upright mark centred on `center`, `w` wide, turned by `yaw`; `depth` runs
/// along the face normal (toward the viewer is +). The camera looks down +z, so screen-right is world -x: mirrored to
/// keep the mark's true handedness.
fn mark_world(center: Vec3, w: f32, yaw: f32, (u, v): (f32, f32), depth: f32) -> Vec3 {
    let (cy, sy) = (yaw.cos(), yaw.sin());
    let vc = brand::get().logo.vc();
    let (x, y) = (-(u - 1024.0) / 2048.0 * w, (vc - v) / 2048.0 * w);
    let d = depth * w / 27.0;
    center + Vec3::new(x * cy + sy * d, y, x * sy - cy * d)
}

/// Upright logo: obsidian armour plates with lit edges, light channels for the glow shapes.
fn draw_mark_basic(p: &mut Painter, center: Vec3, w: f32, yaw: f32, e: f32) {
    let k = w / 27.0;
    let at = |q: (f32, f32), d: f32| mark_world(center, w, yaw, q, d);
    for f in &brand::get().logo.facets {
        let armour = f.role != Role::Glow;
        if armour {
            for t in &f.tris {
                p.push(Layer::Fill, false, tri4(t.map(|q| at(q, -1.1))), [[0.0; 2]; 4], [Vec4::new(0.004, 0.005, 0.009, 1.0); 4], K_ARMOUR);
            }
            for t in &f.tris {
                p.push(Layer::Fill, false, tri4(t.map(|q| at(q, 0.0))), [[0.0; 2]; 4], [Vec4::new(0.020, 0.026, 0.042, 0.96); 4], K_ARMOUR);
            }
        } else {
            for t in &f.tris {
                p.push(Layer::Add, true, tri4(t.map(|q| at(q, 0.05))), [[0.0; 2]; 4], [hot_glow().extend(0.55 + 0.5 * e); 4], K_SOLID);
            }
        }
        let o = orange();
        let c = Vec4::new(o.x, o.y, o.z, (0.85 + 0.45 * e) * f.role.gain());
        for l in &f.loops {
            for i in 0..l.len() {
                p.beam(Layer::Add, at(l[i], 0.05), at(l[(i + 1) % l.len()], 0.05), if armour { 0.15 * k } else { 0.10 * k }, c, c, true);
            }
        }
    }
}

fn srgb_to_lin(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn tm_inv(l: f32) -> f32 {
    -(1.0 - l.min(0.999)).ln() / 1.35
}

/// HDR value to add so that `col` at opacity `a` reads over the floor exactly as it does over #121212 on the desktop
/// (the display output is gamma-encoded and tone-mapped, so naive alpha-blended values come out far too bright).
fn bgc(col: Vec3, a: f32) -> Vec4 {
    // the display path reads ~0.0026 darker in linear light than the maths predicts (measured), so add it back
    const BLACK: f32 = 0.0026;
    let bg_d = brand::get().bg; // the theme background (#121212), as a display value
    let f = |c: f32| (tm_inv(srgb_to_lin(bg_d + a.clamp(0.0, 1.0) * (c - bg_d)) + BLACK) - tm_inv(srgb_to_lin(bg_d) + BLACK)).max(0.0);
    Vec4::new(f(col.x), f(col.y), f(col.z), 1.0)
}

fn smooth01(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn frac(x: f32) -> f32 {
    x - x.floor()
}

fn rr(i: f32, k: f32, s: f32) -> f32 {
    frac((i * 127.1 + k * 311.7 + s * 74.7).sin() * 43758.5453)
}

/// A point of the logo (2048² source space) on the plane that folds up from the floor about its base edge.
/// fold = 0: lying flat, top toward +z · fold = π/2: standing, facing the camera at -z. `depth` is along the face normal.
fn mark_pt(x: f32, zb: f32, w: f32, fold: f32, (uu, vv): (f32, f32), depth: f32) -> Vec3 {
    let (sf, cf) = fold.sin_cos();
    let xx = x - (uu - 1024.0) / 2048.0 * w;
    let ss = (1024.0 - vv) / 2048.0 * w + w * 0.5;
    Vec3::new(xx, ss * sf + 0.04 + depth * cf, zb + ss * cf - depth * sf)
}

/// The logo in the desktop background's line-art style (u = 0) morphing into dark armour with light channels (u = 1).
/// `al` fades the whole line-art mark in (when it is drawn onto a wallpaper that does not carry it).
fn draw_mark_fold(p: &mut Painter, x: f32, zb: f32, w: f32, fold: f32, u: f32, e: f32, breathe: f32, spread: f32, reflect: bool, al: f32) {
    let k = w / 27.0;
    let fg = fg_grey();
    let at = |q: (f32, f32), d: f32| mark_pt(x, zb, w, fold, q, d);
    let o = orange();
    // drawn onto a busy photo rather than over the matching graphite sheet: the line art needs more presence
    let boost = if brand::get().logo_on_wallpaper { 1.0 } else { 2.2 };
    for f in &brand::get().logo.facets {
        // layers of the mark separate in depth (parallax) while it rises, then settle together
        let off = f.role.depth() * spread * k;
        let glow = f.role == Role::Glow;
        if !glow {
            if u > 0.01 {
                for t in &f.tris {
                    p.push(Layer::Fill, false, tri4(t.map(|q| at(q, off - 1.1 * k))), [[0.0; 2]; 4], [Vec4::new(0.004, 0.005, 0.009, u); 4], K_ARMOUR);
                }
                for t in &f.tris {
                    p.push(Layer::Fill, false, tri4(t.map(|q| at(q, off))), [[0.0; 2]; 4], [Vec4::new(0.020, 0.026, 0.042, 0.96 * u); 4], K_ARMOUR);
                }
            }
            let br = 0.5 + 0.5 * breathe;
            let c = match f.role {
                Role::Base => bgc(fg, 0.05 * boost),
                Role::Armour => bgc(o, 0.10 * br * boost),
                _ => bgc(o, 0.075 * br * boost),
            };
            let c = Vec4::new(c.x * (1.0 - u) * al, c.y * (1.0 - u) * al, c.z * (1.0 - u) * al, 1.0);
            if u < 0.99 {
                for t in &f.tris {
                    p.push(Layer::Add, reflect, tri4(t.map(|q| at(q, off))), [[0.0; 2]; 4], [c; 4], K_SOLID);
                }
            }
        } else {
            let hot = hot_glow();
            let bgv = bgc(o, 0.75);
            let arm = hot * (0.55 + 0.5 * e);
            let c = Vec3::new(bgv.x, bgv.y, bgv.z).lerp(arm, u) * al;
            for t in &f.tris {
                p.push(Layer::Add, reflect, tri4(t.map(|q| at(q, off + 0.05))), [[0.0; 2]; 4], [Vec4::new(c.x, c.y, c.z, 1.0); 4], K_SOLID);
            }
        }
        let gain = f.role.gain();
        let fa = if glow { u } else { 1.0 };
        let bgo = bgc(fg, 0.32 * boost);
        let arm = o * ((0.85 + 0.45 * e) * gain);
        let oc3 = (Vec3::new(bgo.x, bgo.y, bgo.z) * (1.0 - u) * fa.max(1.0 - u) + arm * u * fa) * al;
        let oc = Vec4::new(oc3.x, oc3.y, oc3.z, 1.0);
        let oa = (0.85 + 0.45 * e) * gain * fa;
        for l in &f.loops {
            for i in 0..l.len() {
                let (pa, pb) = (at(l[i], off + 0.05), at(l[(i + 1) % l.len()], off + 0.05));
                p.bar(Layer::Add, pa, pb, (0.04 + 0.07 * u) * k, oc, oc, reflect);
                if u > 0.2 {
                    let g = Vec4::new(o.x, o.y, o.z, oa * 0.22 * u * al);
                    p.beam(Layer::Add, pa, pb, 0.11 * k, g, g, reflect);
                }
            }
        }
    }
}

/// The desktop background, laid on the floor: drifting hairline grid (drawn by the floor shader), the mark's
/// construction lines, signal sweeps, telemetry nodes with routed traces, and the scan line over the mark.
fn draw_emerge(p: &mut Painter, sim: &Sim, prog: f32, intro: f32) {
    let br = brand::get();
    let tt = sim.scen_t;
    let cell = EMERGE_CELL;
    let drift = tt * cell / 28.0;
    let fg = fg_grey();
    let accent = orange();
    let ofs = br.mark_offset * 57.1 * 0.4831 * sim.aspect;
    let zc = FLAT_ZB + EMERGE_MARK_W * br.logo.mid_h() + 1.1;
    let (c0, r0) = ((ofs / cell).round(), (zc / cell).round());
    let gx = |i: f32| (c0 + i) * cell - drift;
    let gz = |j: f32| (r0 + j) * cell + drift;
    let cols = 36.0f32;
    let rows = 22.0f32;
    let lift = smooth01((prog - 0.10) / 0.5);
    let la = 1.0 - smooth01(prog / 0.30); // captions / labels fade with the lockup
    let sc = p.h / 2000.0;
    let fade_lines = (1.0 - smooth01((prog - 0.5) / 0.4)) * intro;
    let ef = 1.0 - smooth01((prog - 0.55) / 0.40); // sweeps, nodes and probe dissolve ahead of the handover
    let y = 0.03 + 0.9 * lift; // sweeps ride a little above the floor

    // construction lines extended from the mark's edges, like a drafting sheet
    if br.drafting {
        let f = |q: (f32, f32)| mark_pt(0.0, FLAT_ZB, EMERGE_MARK_W, 0.0, q, 0.0);
        for &(q, d) in &br.logo.guides {
            let (a, b, al) = if d.0.abs() < 1e-3 {
                ((q.0, -3000.0), (q.0, 5000.0), 0.05) // verticals a touch fainter
            } else if d.1.abs() < 1e-3 {
                ((-3000.0, q.1), (5000.0, q.1), 0.07)
            } else {
                let t = 3000.0 * std::f32::consts::SQRT_2;
                ((q.0 - d.0 * t, q.1 - d.1 * t), (q.0 + d.0 * t, q.1 + d.1 * t), 0.07)
            };
            let c = bgc(fg, al * fade_lines);
            p.bar(Layer::Add, f(a), f(b), 0.03, c, c, true);
        }
    }
    if !br.desktop_grid {
        draw_drafting_marks(p, sim, prog, intro);
        return;
    }

    // signal sweeps riding the grid
    for i in 0..9 {
        let fi = i as f32;
        let period = 6.0 + rr(fi, 0.0, 1.0) * 7.0;
        let phase = rr(fi, 0.0, 2.0) * period;
        let cyc = (tt + phase) / period;
        let (k, uu) = (cyc.floor(), cyc - cyc.floor());
        let vertical = i % 3 == 2;
        let reverse = rr(fi, k, 4.0) > 0.5;
        let len = cell * (3.0 + (rr(fi, k, 5.0) * 5.0).floor());
        let (half, lanes) = if vertical { (rows * cell * 0.5, 17.0) } else { (cols * cell * 0.5, 17.0) };
        let lane = (rr(fi, k, 3.0) * lanes).floor() - 8.0;
        let span = half * 2.0 + len;
        let pos = (if reverse { 1.0 - uu } else { uu }) * span - len; // from the left / top edge
        let head = if i % 2 == 0 { accent } else { fg };
        let alpha = (std::f32::consts::PI * uu).sin() * ef;
        let (a, b, tail_s, head_s) = {
            let (s0, s1) = (pos, pos + len);
            if vertical {
                let x = gx(lane * 1.7);
                let (za, zb) = (zc + half - s0, zc + half - s1);
                (Vec3::new(x, y, za), Vec3::new(x, y, zb), s0, s1)
            } else {
                let zz = gz(lane);
                let (xa, xb) = (ofs + half - s0, ofs + half - s1);
                (Vec3::new(xa, y, zz), Vec3::new(xb, y, zz), s0, s1)
            }
        };
        let _ = (tail_s, head_s);
        let (ta, tb) = if reverse { (b, a) } else { (a, b) }; // tail → head
        let mid = ta.lerp(tb, 0.5);
        let cf = |al: f32| bgc(fg, al * alpha);
        let ch = bgc(head, 0.9 * alpha);
        p.bar(Layer::Add, ta, mid, 0.035, cf(0.0), cf(0.22), true);
        p.bar(Layer::Add, mid, tb, 0.035, cf(0.22), ch, true);
        p.bar(Layer::Add, tb - (tb - ta).normalize_or_zero() * 0.12, tb, 0.14, ch, ch, true);
    }

    // telemetry nodes: ring, halo, core, routed trace and pad
    let pulse = 0.5 + 0.5 * (tt * 2.0 * std::f32::consts::PI / 2.4).sin();
    for i in 0..8 {
        let fi = i as f32;
        let period = 9.0 + rr(fi, 0.0, 6.0) * 6.0;
        let phase = rr(fi, 0.0, 7.0) * period;
        let cyc = (tt + phase) / period;
        let (k, uu) = (cyc.floor(), cyc - cyc.floor());
        let ci = 2.0 + (rr(fi, k, 8.0) * (cols - 5.0)).floor();
        let ri = 2.0 + (rr(fi, k, 9.0) * (rows - 5.0)).floor();
        let ti = (ci + ((rr(fi, k, 10.0) - 0.5) * 12.0).round()).clamp(1.0, cols - 2.0);
        let tj = (ri + ((rr(fi, k, 11.0) - 0.5) * 8.0).round()).clamp(1.0, rows - 2.0);
        let (dx, dy) = ((ti - ci) * cell, (tj - ri) * cell);
        let total = dx.abs() + dy.abs();
        let drawn = smooth01((uu - 0.08) / 0.4) * total;
        let h_len = dx.abs().min(drawn);
        let v_len = (drawn - dx.abs()).max(0.0);
        let env = smooth01(uu / 0.08).min(smooth01((1.0 - uu) / 0.14));
        let y = 0.03 + 1.6 * lift; // telemetry sits higher still, so layers slide past each other
        let p0 = Vec3::new(gx(ci - cols * 0.5), y, gz(rows * 0.5 - ri));
        let ac = |a: f32| bgc(accent, a * env);
        if i % 4 != 3 {
            let sx = -dx.signum(); // screen-right is world -x
            let sz = -dy.signum(); // screen-down is world -z
            let elbow = Vec3::new(p0.x + sx * h_len, y, p0.z);
            p.bar(Layer::Add, p0, elbow, 0.03, ac(0.55), ac(0.55), true);
            let corner = Vec3::new(p0.x + sx * dx.abs(), y, p0.z);
            if v_len > 0.0 {
                p.bar(Layer::Add, corner, Vec3::new(corner.x, y, corner.z + sz * v_len), 0.03, ac(0.55), ac(0.55), true);
            }
            if drawn >= total - 0.01 {
                let (pc, hs) = (Vec3::new(corner.x, y, corner.z + sz * dy.abs()), 0.2);
                let (q0, q1, q2, q3) = (pc + Vec3::new(-hs, 0.0, -hs), pc + Vec3::new(hs, 0.0, -hs), pc + Vec3::new(hs, 0.0, hs), pc + Vec3::new(-hs, 0.0, hs));
                let pf = bgc(fg, 0.55 * env);
                for (a, b) in [(q0, q1), (q1, q2), (q2, q3), (q3, q0)] {
                    p.bar(Layer::Add, a, b, 0.03, pf, pf, true);
                }
            }
        }
        let ph = frac((tt + phase) / 2.4);
        let rad = 1.45 * (0.12 + 0.88 * ph);
        let seg = 20;
        let rc = ac(0.6 * (1.0 - ph));
        for sgi in 0..seg {
            let (a0, a1) = (sgi as f32 / seg as f32 * std::f32::consts::TAU, (sgi + 1) as f32 / seg as f32 * std::f32::consts::TAU);
            p.bar(Layer::Add, p0 + Vec3::new(a0.cos() * rad, 0.0, a0.sin() * rad), p0 + Vec3::new(a1.cos() * rad, 0.0, a1.sin() * rad), 0.04, rc, rc, true);
        }
        if lift > 0.05 {
            let dl = ac(0.30 * lift);
            p.bar(Layer::Add, Vec3::new(p0.x, 0.03, p0.z), p0, 0.03, dl, dl, false);
        }
        // readout label, as on the desktop
        if la > 0.01 {
            if let Some((sx, sy)) = p.to_screen(p0) {
                let id = (rr(fi, k, 12.0) * 9000.0).floor() as i32 + 1000;
                let reading = 18.0 + rr(fi, tt.floor(), 13.0) * 0.6 + rr(fi, k, 14.0) * 9.0;
                p.text_ex(F_MONO, sx + 24.0 * sc, sy - 40.0 * sc, 22.0 * sc * 1.2, 3.0 * sc, &format!("N-{} {:.1}", id, reading), bgc(fg, 0.5 * env * la));
            }
        }
        p.billboard(Layer::Add, p0 + Vec3::Y * 0.1, 0.4, 0.4, ac(0.10 + 0.10 * pulse), K_GLOW, true);
        p.billboard(Layer::Add, p0 + Vec3::Y * 0.12, 0.14, 0.14, bgc(accent, env), K_GLOW, true);
    }

    // registration crosses on every major intersection
    {
        let (m0, n0) = ((c0 / 4.0).round(), (r0 / 4.0).round());
        let c = bgc(fg, 0.22 * fade_lines);
        for ii in -6..=6 {
            for jj in -4..=4 {
                let (cx, cz) = ((m0 + ii as f32) * 4.0 * cell - drift, (n0 + jj as f32) * 4.0 * cell + drift);
                p.bar(Layer::Add, Vec3::new(cx - 0.25, 0.03, cz), Vec3::new(cx + 0.25, 0.03, cz), 0.02, c, c, false);
                p.bar(Layer::Add, Vec3::new(cx, 0.03, cz - 0.25), Vec3::new(cx, 0.03, cz + 0.25), 0.02, c, c, false);
            }
        }
    }

    draw_drafting_marks(p, sim, prog, intro);
}

/// The figure caption, dimension line and vertex probe around the flat logo.
fn draw_drafting_marks(p: &mut Painter, sim: &Sim, prog: f32, intro: f32) {
    let br = brand::get();
    if !br.drafting {
        return;
    }
    let tt = sim.scen_t;
    let fg = fg_grey();
    let accent = orange();
    let la = (1.0 - smooth01(prog / 0.30)) * intro;
    let sc = p.h / 2000.0;
    // figure caption above the mark and its dimension line below (screen-space, riding the flat mark)
    if la > 0.01 {
        let f = |q: (f32, f32)| mark_pt(0.0, FLAT_ZB, EMERGE_MARK_W, 0.0, q, 0.0);
        let bb = br.logo.bbox;
        if let (Some((x0, y0)), Some((x1, y1))) = (p.to_screen(f((bb.0, bb.1))), p.to_screen(f((bb.2, bb.3)))) {
            let fs = 22.0 * sc * 1.2;
            p.text_ex(F_MONO, x0.min(x1), y0.min(y1) - 86.0 * sc, fs, 4.0 * sc, &format!("FIG. 01  —  {} MARK", br.name), bgc(fg, 0.5 * la));
            let (xa, xb) = (x0.min(x1), x0.max(x1));
            let yd = y0.max(y1) + 58.0 * sc;
            let label = format!("{:.0} × {:.0}", br.logo.src_size.0, br.logo.src_size.1);
            let label = label.as_str();
            let lw = p.text_width_ex(F_MONO, fs, 4.0 * sc, label);
            let mid = 0.5 * (xa + xb);
            p.rect(xa, yd, (mid - lw * 0.5 - 16.0 * sc) - xa, 2.0 * sc, bgc(fg, 0.25 * la));
            p.rect(mid + lw * 0.5 + 16.0 * sc, yd, xb - (mid + lw * 0.5 + 16.0 * sc), 2.0 * sc, bgc(fg, 0.25 * la));
            p.rect(xa, yd - 10.0 * sc, 2.0 * sc, 22.0 * sc, bgc(fg, 0.4 * la));
            p.rect(xb - 2.0 * sc, yd - 10.0 * sc, 2.0 * sc, 22.0 * sc, bgc(fg, 0.4 * la));
            p.text_ex(F_MONO, mid - lw * 0.5, yd - fs * 0.45, fs, 4.0 * sc, label, bgc(fg, 0.55 * la));
        }
    }

    // vertex probe: walks the mark's corners and reads out source coordinates, as on the desktop
    {
        let pa = (1.0 - smooth01((prog - 0.15) / 0.40)) * intro;
        let verts = &br.logo.verts;
        if pa > 0.01 && !verts.is_empty() {
            let step = 1.8;
            let (idx, uu) = (((tt / step).floor() as usize) % verts.len().max(1), frac(tt / step));
            let v = verts[idx];
            let fold = std::f32::consts::FRAC_PI_2 * { let x = ((prog - 0.30) / 0.685).clamp(0.0, 1.0); x * x * x * (x * (x * 6.0 - 15.0) + 10.0) };
            let env = smooth01(uu / 0.15).min(smooth01((1.0 - uu) / 0.20));
            if let Some((sx, sy)) = p.to_screen(mark_pt(0.0, crate::sim::rise_zb(fold), EMERGE_MARK_W, fold, v, 0.06)) {
                let al = pa * env;
                let ac = bgc(accent, al);
                let fgc = |a: f32| bgc(fg, a * al);
                p.rect(sx - 18.0 * sc, sy, 38.0 * sc, 2.0 * sc, ac);
                p.rect(sx, sy - 18.0 * sc, 2.0 * sc, 38.0 * sc, ac);
                let hs = 11.0 * sc;
                for (x, y, w, h) in [(sx - hs, sy - hs, 2.0 * hs, 2.0 * sc), (sx - hs, sy + hs, 2.0 * hs, 2.0 * sc), (sx - hs, sy - hs, 2.0 * sc, 2.0 * hs), (sx + hs, sy - hs, 2.0 * sc, 2.0 * hs + 2.0 * sc)] {
                    p.rect(x, y, w, h, fgc(0.7));
                }
                let fs = 24.0 * sc * 1.2;
                let label = format!("X {:.2}   Y {:.2}", v.0, v.1);
                let lw = p.text_width_ex(F_MONO, fs, 3.0 * sc, &label);
                let flip = sx > p.w - 480.0 * sc;
                let tx = if flip { sx - 64.0 * sc - lw } else { sx + 64.0 * sc };
                p.text_ex(F_MONO, tx, sy + 30.0 * sc, fs, 3.0 * sc, &label, fgc(0.8));
            }
        }
    }

}

pub struct Scene {
    pub frame: Frame,
    pub atlas: Atlas,
    /// HUD text size multiplier (larger for recordings that get downscaled)
    pub hud_scale: f32,
}

pub struct Camera {
    pub vp: Mat4,
    pub eye: Vec3,
}

pub fn camera(eye: Vec3, target: Vec3, up: Vec3, aspect: f32) -> (Camera, Vec3, Vec3) {
    let view = Mat4::look_at_rh(eye, target, up);
    let proj = Mat4::perspective_rh(0.9, aspect, 0.5, 900.0);
    (Camera { vp: proj * view, eye }, eye, target)
}

impl Scene {
    pub fn new() -> Self {
        Scene { frame: Frame::default(), atlas: Atlas::build(), hud_scale: 1.0 }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build(&mut self, sim: &Sim, t: f32, w: f32, h: f32, fps: f32, _fade: f32) -> (Camera, [Vec4; 4], [Vec4; 4]) {
        self.frame.clear();
        let (cam, eye, target) = camera(sim.cam_eye, sim.cam_target, sim.cam_up, w / h);
        let fwd = (target - eye).normalize();
        let right = fwd.cross(sim.cam_up).normalize();
        let up = right.cross(fwd);
        let mut p = Painter { vp: cam.vp, eye, right, up, w, h, f: &mut self.frame, atlas: &self.atlas };

        let e = sim.energy;
        let center = Vec3::new(0.0, MARK_Y + sim.mono_y, MARK_Z);
        let twin = sim.scenario == Scenario::Twin;
        let em = sim.scenario == Scenario::Emerge;
        let prog = sim.emerge_prog();
        // palette: the desktop background's graphite/grey/amber until the battle, then Tron blue + red
        let tr = sim.tron;
        let lerp3 = |a: Vec3, b: Vec3, t: f32| a + (b - a) * t;
        let fg0 = fg_grey();
        let blue = lerp3(fg0, BLUE, tr);
        let rim = lerp3(fg0, RIM, tr * 0.85);
        let sense = lerp3(orange(), SENSE, tr);
        let node_a = if em { smooth01((prog - 0.35) / 0.4) } else { 1.0 };
        if em {
            // a wallpaper that does not carry the logo: the line-art mark is drawn onto it first
            let intro = if brand::get().logo_on_wallpaper { 1.0 } else { smooth01((sim.scen_t - 0.8) / 2.6) };
            draw_emerge(&mut p, sim, prog, intro);
            let sm5 = |x: f32| {
                let x = x.clamp(0.0, 1.0);
                x * x * x * (x * (x * 6.0 - 15.0) + 10.0)
            };
            let fold = std::f32::consts::FRAC_PI_2 * sm5((prog - 0.30) / 0.685);
            let style = smooth01((prog - 0.35) / 0.55);
            draw_mark_fold(&mut p, 0.0, crate::sim::rise_zb(fold), EMERGE_MARK_W, fold, style, 0.3, 0.5 + 0.5 * (sim.scen_t * 2.0 * std::f32::consts::PI / 2.4).sin(), 3.2 * (std::f32::consts::PI * ((prog - 0.30) / 0.60).clamp(0.0, 1.0)).sin(), false, intro);

            // the wireframe copy stays exactly where the flat mark lay, while the coloured mark stands up out of it;
            // its chosen lines then become the first trails
            if fold > 0.01 {
                let c = Vec4::new(orange().x * 0.24, orange().y * 0.24, orange().z * 0.24, 1.0);
                for ed in &brand::get().logo.edges {
                    p.bar(Layer::Add, mark_pt(0.0, FLAT_ZB, EMERGE_MARK_W, 0.0, ed.a, 0.0), mark_pt(0.0, FLAT_ZB, EMERGE_MARK_W, 0.0, ed.b, 0.0), 0.07, c, c, false);
                }
            }
            // the chosen lines brighten and rise as walls before the handover, so the trails grow out of the copy
            if prog > 0.85 {
                let r = smooth01((prog - 0.85) / 0.15);
                let rw = smooth01((prog - 0.93) / 0.07);
                for path in &sim.seed_paths {
                    for w2 in path.windows(2) {
                        let (a3, b3) = (Vec3::new(w2[0].x, 0.04, w2[0].y), Vec3::new(w2[1].x, 0.04, w2[1].y));
                        let c = Vec4::new(orange().x * 0.36 * r, orange().y * 0.36 * r, orange().z * 0.36 * r, 1.0);
                        p.bar(Layer::Add, a3, b3, 0.07, c, c, false);
                        if rw > 0.001 {
                            let cw = Vec4::new(orange().x, orange().y, orange().z, 0.9 * rw);
                            p.wall(a3, b3, TRAIL_H * 0.10 * rw, cw, cw);
                        }
                    }
                }
            }
        }

        // sensor nodes: small floor pads that light up once the scan front passes; they feed the twin
        let mut digital_sum = 0.0f32;
        for tw in &sim.towers {
            let d = ((sim.scan_x + 8.0 - tw.x) / 16.0).clamp(0.0, 1.0);
            let d = d * d * (3.0 - 2.0 * d);
            digital_sum += d;
            let a = (0.10 + 0.55 * d) * node_a * (0.30 + 0.70 * tr);
            let c = Vec4::new(sense.x, sense.y, sense.z, a);
            let h = 1.7;
            let (p0, p1, p2, p3) = (Vec3::new(tw.x - h, 0.07, tw.z - h), Vec3::new(tw.x + h, 0.07, tw.z - h), Vec3::new(tw.x + h, 0.07, tw.z + h), Vec3::new(tw.x - h, 0.07, tw.z + h));
            for (a0, a1) in [(p0, p1), (p1, p2), (p2, p3), (p3, p0)] {
                p.bar(Layer::Add, a0, a1, 0.08 + 0.06 * tr, c, c, true);
            }
            if d > 0.2 {
                // over a real wallpaper the cores wait for the nodes to fade in (on the lAvArt sheet they belong to it)
                let ca = if brand::get().desktop_grid { 1.0 } else { node_a };
                let core = Vec4::new(rim.x, rim.y, rim.z, 0.5 * d * ca);
                p.bar(Layer::Add, Vec3::new(tw.x, 0.07, tw.z), Vec3::new(tw.x, 1.6 * d, tw.z), 0.14, core, core, true);
            }
        }
        let sync = 100.0 * digital_sum / sim.towers.len().max(1) as f32;
        // the scan plane itself: a vertical sheet of sensing light sweeping across the district
        if matches!(sim.scenario, Scenario::Arena | Scenario::Twin) {
            let za = (eye.z + 6.0).max(-90.0);
            let c = Vec4::new(sense.x, sense.y, sense.z, 0.028 * smooth01(sim.scen_t / 3.0));
            p.wall(Vec3::new(sim.scan_x, 0.0, za), Vec3::new(sim.scan_x, 0.0, za + 320.0), 24.0, c, c);
        }
        if twin {
        // horizon bloom + anamorphic streaks behind the monolith
        p.billboard(Layer::Pre, center + Vec3::new(0.0, 0.0, 6.0), 40.0, 30.0, Vec4::new(orange().x, orange().y, orange().z, 0.05 + 0.06 * e), K_GLOW, true);
        p.billboard(Layer::Pre, Vec3::new(0.0, center.y, MARK_Z + 6.0), 150.0, 3.2, Vec4::new(orange().x, orange().y, orange().z, 0.05 + 0.16 * e), K_STREAK, true);
        p.billboard(Layer::Pre, Vec3::new(0.0, 1.2, MARK_Z + 40.0), 190.0, 2.0, Vec4::new(blue.x, blue.y, blue.z, 0.24), K_STREAK, false);

        // monolith: obsidian armour + embedded light channels
        let yaw = 0.20 * (t * 0.23).sin();
        draw_mark_basic(&mut p, center, MARK_W, yaw, e);

        // digital twin of the mark: cyan wire ghost, scan plane, sync column to the floor
        {
            let lg = &brand::get().logo;
            let vc = lg.vc();
            let ghost = |q: (f32, f32)| mark_world(center, MARK_W, yaw, ((q.0 - 1024.0) * 1.07 + 1024.0, (q.1 - vc) * 1.07 + vc), 2.2);
            let c = Vec4::new(blue.x, blue.y, blue.z, 0.34 + 0.28 * e);
            for f in lg.facets.iter().filter(|f| f.role != Role::Glow) {
                for l in &f.loops {
                    for i in 0..l.len() {
                        p.beam(Layer::Add, ghost(l[i]), ghost(l[(i + 1) % l.len()]), 0.07, c, c, true);
                    }
                }
            }
            let col = Vec4::new(blue.x, blue.y, blue.z, 0.24);
            p.beam(Layer::Add, Vec3::new(0.0, 0.0, MARK_Z), center - Vec3::Y * 13.0, 0.32, col, col * 0.3, false);
        }

        // circuit traces + hard-edged data pulses (no organic curves: right angles only)
        for tw in &sim.towers {
            let pts = trace(tw);
            let c = Vec4::new(blue.x, blue.y, blue.z, 0.08);
            for i in 0..3 {
                p.beam(Layer::Add, pts[i], pts[i + 1], 0.10, c, c, true);
            }
        }
        for pk in &sim.packets {
            let col = if pk.up { Vec3::new(0.30, 0.65, 1.0) } else { orange() };
            let f = pk.fade();
            let steps = 6;
            for k in 0..steps {
                let (d0, d1) = (pk.d - Packet::LEN * k as f32 / steps as f32, pk.d - Packet::LEN * (k + 1) as f32 / steps as f32);
                if d1 > pk.total() || d0 < 0.0 {
                    continue;
                }
                let (a, b) = (pk.pos(d0), pk.pos(d1));
                let (i0, i1) = (1.0 - k as f32 / steps as f32, 1.0 - (k + 1) as f32 / steps as f32);
                p.beam(
                    Layer::Add,
                    a,
                    b,
                    0.20,
                    Vec4::new(col.x, col.y, col.z, 0.95 * i0 * i0 * f),
                    Vec4::new(col.x, col.y, col.z, 0.95 * i1 * i1 * f),
                    true,
                );
            }
        }

        }

        // ── light-barrier perimeter: rises out of the floor across the end of the emergence ──
        if sim.scenario == Scenario::Arena || (em && prog > 0.62) {
            let ba = if em { smooth01((prog - 0.62) / 0.36) } else { 1.0 };
            let cn = [(-ARENA_X, ARENA_Z0), (ARENA_X, ARENA_Z0), (ARENA_X, ARENA_Z1), (-ARENA_X, ARENA_Z1)];
            for i in 0..4 {
                let (a, b) = (cn[i], cn[(i + 1) % 4]);
                let c = Vec4::new(blue.x, blue.y, blue.z, 0.09 * ba);
                p.wall(Vec3::new(a.0, 0.0, a.1), Vec3::new(b.0, 0.0, b.1), 4.0 * (0.2 + 0.8 * ba), c, c);
            }
        }
        if sim.scenario == Scenario::Arena {
            let pulse = sim.pulses.iter().map(|q| q.strength).fold(0.0f32, f32::max);
            draw_mark_fold(&mut p, 0.0, EMERGE_ZB, EMERGE_MARK_W, std::f32::consts::FRAC_PI_2, 1.0, e + 0.6 * pulse, 0.5 + 0.5 * (t * 2.6).sin(), 0.0, false, 1.0); // the mirror image is gone for good: its lines became the trails
        }

        // ── Transit: lane lights streaming past, exit portal ahead ──
        if sim.scenario == Scenario::Run {
            let z0 = eye.z;
            // four lightcycle trails stream past: tall light walls, fading with distance
            const SPAN: f32 = 420.0;
            let fade = |z: f32| (1.0 - (z - z0) / SPAN).clamp(0.0, 1.0).powf(1.4);
            let lanes = [-13.0f32, -8.5, 8.5, 13.0]; // the middle stays open so the logo is clear
            let seg = 14.0;
            for (i, lane) in lanes.iter().enumerate() {
                let col = if i % 2 == 0 { orange() } else { BLUE };
                for k in 0..(SPAN / seg) as i32 {
                    let (za, zb) = (z0 + 2.0 + k as f32 * seg, z0 + 2.0 + (k + 1) as f32 * seg);
                    p.wall(Vec3::new(*lane, 0.0, za), Vec3::new(*lane, 0.0, zb), TRAIL_H * 0.7, Vec4::new(col.x, col.y, col.z, 0.45 * fade(za)), Vec4::new(col.x, col.y, col.z, 0.45 * fade(zb)));
                }
            }
            // thin Tron lines streaming past at the camera's speed: world-fixed, so they rush by as it advances
            {
                let cell = 9.0;
                let k0 = (z0 / cell).floor() as i32;
                for k in k0..k0 + (SPAN / cell) as i32 {
                    for n in 0..2 {
                        let id = k as f32 * 2.0 + n as f32;
                        if rr(id, 0.0, 31.0) < 0.35 {
                            continue;
                        }
                        let side = if rr(id, 0.0, 32.0) < 0.5 { -1.0 } else { 1.0 };
                        let x = side * (3.5 + rr(id, 0.0, 33.0) * 24.0);
                        let (z, len) = (k as f32 * cell + rr(id, 0.0, 34.0) * cell, 4.0 + rr(id, 0.0, 35.0) * 16.0);
                        let col = if rr(id, 0.0, 36.0) < 0.5 { orange() } else { BLUE_HOT };
                        let f = fade(z);
                        let y = if rr(id, 0.0, 37.0) < 0.6 { 0.05 } else { 0.3 + rr(id, 0.0, 38.0) * 3.5 };
                        let c = Vec4::new(col.x, col.y, col.z, 0.8 * f);
                        p.bar(Layer::Add, Vec3::new(x, y, z), Vec3::new(x, y, z + len), 0.08, Vec4::new(col.x, col.y, col.z, 0.0), c, true);
                    }
                }
            }
            // data riding the trails: bright dashes at their own speeds, each with a hex readout
            let sc = h / 1400.0;
            for (i, lane) in lanes.iter().enumerate() {
                let col = if i % 2 == 0 { orange() } else { BLUE_HOT };
                let side = if i < 2 { 1.0 } else { -1.0 }; // corridor-facing side of the wall
                for j in 0..10 {
                    let id = (i * 10 + j) as f32;
                    let v = 55.0 + rr(id, 0.0, 21.0) * 90.0;
                    let zw = z0 + (rr(id, 0.0, 22.0) * SPAN + t * v - z0).rem_euclid(SPAN);
                    let len = 5.0 + rr(id, 0.0, 23.0) * 12.0;
                    let y = 0.35 + rr(id, 0.0, 24.0) * (TRAIL_H - 0.7);
                    let f = fade(zw);
                    let x = lane + side * 0.12;
                    let (tail, head) = (Vec3::new(x, y, zw - len), Vec3::new(x, y, zw));
                    p.bar(Layer::Add, tail, head, 0.12, Vec4::new(col.x, col.y, col.z, 0.0), Vec4::new(col.x * 1.2, col.y * 1.2, col.z * 1.2, 0.95 * f), true);
                    p.billboard(Layer::Add, head, 0.45, 0.45, Vec4::new(col.x, col.y, col.z, 0.55 * f), K_GLOW, true);
                    if j < 2 && f > 0.25 && f < 0.85 {
                        if let Some((sx, sy)) = p.to_screen(head + Vec3::new(side * 0.5, 0.6, 0.0)) {
                            let px = 13.0 * sc * (1.0 + f);
                            let word = (rr(id, (t * 3.0).floor(), 25.0) * 65535.0) as u32;
                            p.text(sx, sy - px, px, &format!("{:04X}:{:04X}", (rr(id, 0.0, 26.0) * 65535.0) as u32, word), Vec4::new(col.x, col.y, col.z, 0.7 * f));
                        }
                    }
                }
            }
            let prog = (sim.scen_t / sim.scenario.duration()).clamp(0.0, 1.0);
let zp = eye.z + 520.0 - 330.0 * prog;
            p.billboard(Layer::Pre, Vec3::new(0.0, 22.0, zp + 8.0), 70.0, 70.0, Vec4::new(0.75, 0.88, 1.0, 0.10 + 0.35 * prog), K_GLOW, true);
            draw_mark_basic(&mut p, Vec3::new(0.0, 24.0, zp), 95.0, 0.0, 0.35 + 0.8 * prog);
        }

        // the logo's wireframe copy, flat on the floor: six of its lines become the first light trails (below);
        // every other line slides out of existence along its own direction
        if sim.seam_arena && sim.scenario == Scenario::Arena && sim.scen_t < 3.2 {
            let tt = sim.scen_t;
            for (ei, ed) in brand::get().logo.edges.iter().enumerate() {
                {
                    let idx = ei + 1;
                    let (a, b) = (logo_floor(ed.a), logo_floor(ed.b));
                    let l = (b - a).length();
                    if l < 1e-3 {
                        continue;
                    }
                    let d = (b - a) / l;
                    let selected = sim.seed_sel.get(ei).copied().unwrap_or(false);
                    let (a3, b3) = (Vec3::new(a.x, 0.04, a.y), Vec3::new(b.x, 0.04, b.y));
                    if selected {
                        let al = 1.0 - smooth01(tt / 0.9);
                        let c = Vec4::new(orange().x * 0.24 * al, orange().y * 0.24 * al, orange().z * 0.24 * al, 1.0);
                        p.bar(Layer::Add, a3, b3, 0.07, c, c, false);
                    } else {
                        let tau = smooth01((tt - 0.02 - 0.03 * idx as f32) / 1.6);
                        if tau < 1.0 {
                            let e_ext = 16.0;
                            let tail = a + d * ((l + e_ext) * tau);
                            let head = b + d * (e_ext * tau);
                            let al = 1.0 - tau;
                            let ct = Vec4::new(orange().x * 0.0, orange().y * 0.0, orange().z * 0.0, 1.0);
                            let ch = Vec4::new(orange().x * (0.24 + 0.5 * tau) * al, orange().y * (0.24 + 0.5 * tau) * al, orange().z * (0.24 + 0.5 * tau) * al, 1.0);
                            p.bar(Layer::Add, Vec3::new(tail.x, 0.04, tail.y), Vec3::new(head.x, 0.04, head.y), 0.07 + 0.05 * tau, ct, ch, false);
                            // diagonals leave a staircase of light wall (only grid-axis moves exist in this world)
                            if d.x.abs() > 0.3 && d.y.abs() > 0.3 {
                                let step = 1.6;
                                let n = ((head - tail).length() / step).ceil() as i32;
                                let (sx, sz) = (d.x.signum() * step, d.y.signum() * step);
                                let mut cur = Vec2::new(tail.x, tail.y);
                                let wall_a = 0.55 * al * tau.sqrt();
                                let wc = Vec4::new(orange().x, orange().y, orange().z, wall_a);
                                for _ in 0..n.min(40) {
                                    let mid = Vec2::new(cur.x + sx, cur.y);
                                    let nxt = Vec2::new(cur.x + sx, cur.y + sz);
                                    p.wall(Vec3::new(cur.x, 0.0, cur.y), Vec3::new(mid.x, 0.0, mid.y), 1.0 * al, wc, wc);
                                    p.wall(Vec3::new(mid.x, 0.0, mid.y), Vec3::new(nxt.x, 0.0, nxt.y), 1.0 * al, wc, wc);
                                    cur = nxt;
                                }
                            }
                            p.billboard(Layer::Add, Vec3::new(head.x, 0.2, head.y), 0.5, 0.5, Vec4::new(orange().x, orange().y, orange().z, 0.5 * al * tau), K_GLOW, false);
                        }
                    }
                }
            }
        }

        // real Omarchy repo activity: one falling line per event (blue = people arriving, amber = code changing,
        // pale = conversation); taller and brighter for heavier events
        for d in &sim.drops {
            let col = d.kind.color();
            let a = 0.65 + 0.35 * d.weight;
            let bot = Vec3::new(d.x, d.y, d.z);
            let top = Vec3::new(d.x, d.y + d.len, d.z);
            p.bar(Layer::Add, bot, top, 0.16 + 0.16 * d.weight, Vec4::new(col.x, col.y, col.z, a), Vec4::new(col.x, col.y, col.z, 0.0), true);
            p.billboard(Layer::Add, bot, 0.5, 0.5, Vec4::new(col.x, col.y, col.z, 0.5 * a), K_GLOW, true);
            // who's behind it, riding the falling line
            if let Some((sx, sy)) = p.to_screen(bot + Vec3::Y * 0.6) {
                let lf = (h * 0.0118).max(12.0) * self.hud_scale * 0.9;
                p.text(sx + lf * 0.5, sy - lf * 0.5, lf, &d.who, Vec4::new(col.x, col.y, col.z, 0.9 * a));
            }
        }

        // event trails: the landed activity runs the grid in its own colour and pattern
        for s in &sim.streaks {
            let n = s.pts.len();
            if n < 2 {
                continue;
            }
            let col = s.kind.color();
            let hgt = TRAIL_H * s.height;
            let total: f32 = s.pts.windows(2).map(|w| (w[1] - w[0]).length()).sum::<f32>().max(1e-3);
            let mut acc = 0.0;
            for wdw in s.pts.windows(2) {
                let (a, b) = (wdw[0], wdw[1]);
                let (fa, fb) = (acc / total, (acc + (b - a).length()) / total);
                acc += (b - a).length();
                let ga = |f: f32| (0.2 + 0.8 * f.powf(1.1)) * (0.35 + 0.65 * s.live);
                p.wall(Vec3::new(a.x, 0.0, a.y), Vec3::new(b.x, 0.0, b.y), hgt, Vec4::new(col.x, col.y, col.z, ga(fa)), Vec4::new(col.x, col.y, col.z, ga(fb)));
            }
            if s.live > 0.0 {
                let hd = s.pts[n - 1];
                let foot = Vec3::new(hd.x, 0.0, hd.y);
                let c = Vec4::new(col.x * 0.9 + 0.1, col.y * 0.9 + 0.1, col.z * 0.9 + 0.1, 0.85 * s.live);
                p.bar(Layer::Add, foot, foot + Vec3::Y * hgt, 0.10, c, c, true);
                p.billboard(Layer::Add, foot + Vec3::Y * 0.2, 0.6, 0.6, Vec4::new(col.x, col.y, col.z, 0.45 * s.live), K_GLOW, true);
                if let Some(who) = &s.who {
                    if let Some((sx, sy)) = p.to_screen(foot + Vec3::Y * (hgt + 0.4)) {
                        let lf = (h * 0.0118).max(12.0) * self.hud_scale * 0.8;
                        p.text(sx + lf * 0.4, sy - lf * 0.4, lf, who, Vec4::new(col.x, col.y, col.z, 0.8 * s.live));
                    }
                }
            }
        }

        // cycles: walls, heads, spawn flares
        for c in &sim.cycles {
            if !c.alive {
                continue;
            }
            let mut col = c.team.color();
            let mut trail_h = TRAIL_H;
            if c.seeded {
                // born from the logo: amber like the mark, rising off the floor, then turning Tron blue or red
                let amber = orange();
                col = amber + (col - amber) * smooth01((c.age - 0.6) / 3.0);
                trail_h = TRAIL_H * (0.10 + 0.90 * smooth01(c.age / 1.2));
            }
            let n = c.pts.len();
            let total: f32 = c.pts.windows(2).map(|w| (w[1] - w[0]).length()).sum::<f32>().max(1e-3);
            let mut acc = 0.0;
            for wdw in c.pts.windows(2) {
                let (a, b) = (wdw[0], wdw[1]);
                let (fa, fb) = (acc / total, (acc + (b - a).length()) / total);
                acc += (b - a).length();
                let flat = if c.seeded { 1.0 - smooth01((c.age - 0.4) / 2.2) } else { 0.0 };
                let ga = |f: f32| (f.powf(1.3) * (1.0 - flat) + flat) * 0.95;
                let ca = Vec4::new(col.x, col.y, col.z, ga(fa));
                let cb = Vec4::new(col.x, col.y, col.z, ga(fb));
                p.wall(Vec3::new(a.x, 0.0, a.y), Vec3::new(b.x, 0.0, b.y), trail_h, ca, cb);
            }
            if n >= 2 {
                // the actor is a single crisp vertical line, exactly as tall as the light wall it leaves
                let foot = Vec3::new(c.pos.x, 0.0, c.pos.y);
                let tip = Vec3::new(c.pos.x, trail_h, c.pos.y);
                let base = Vec4::new(col.x * 0.85, col.y * 0.85, col.z * 0.85, 0.85);
                let top = Vec4::new(col.x * 0.85 + 0.12, col.y * 0.85 + 0.12, col.z * 0.85 + 0.12, 0.85);
                p.bar(Layer::Add, foot, tip, 0.11, base, top, true);
            }
            if c.spawn_flash > 0.0 {
                let h3 = Vec3::new(c.pos.x, 6.0 * c.spawn_flash, c.pos.y);
                p.beam(Layer::Add, Vec3::new(c.pos.x, 0.0, c.pos.y), h3 + Vec3::Y * 22.0 * c.spawn_flash, 0.7, Vec4::new(col.x, col.y, col.z, c.spawn_flash), Vec4::new(1.0, 1.0, 1.0, 0.0), true);
            }
        }

        // derezzed debris: cubes from light cycles; triangles, prisms, slabs and cubes from the event trails
        for s in &sim.shards {
            let q = Quat::from_axis_angle(s.axis, s.ang);
            let (ex, ey, ez) = (q * Vec3::X * s.size, q * Vec3::Y * s.size, q * Vec3::Z * s.size);
            let a = (s.life / s.max * 2.2).min(1.0) * 0.55;
            let dig = ((1.0 - s.life / s.max - 0.15) / 0.5).clamp(0.0, 1.0);
            let sc = s.color.lerp(Vec3::new(0.35, 0.65, 1.0), dig);
            let c = Vec4::new(sc.x, sc.y, sc.z, a);
            // a lit-edged face spanned by u and v around its centre
            let face = |p: &mut Painter, m: Vec3, u: Vec3, v: Vec3, c: Vec4| {
                p.push(Layer::Add, true, [m - u - v, m + u - v, m + u + v, m - u + v], [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]], [c; 4], K_SHARD);
            };
            match s.shape {
                Shape::Rect => face(&mut p, s.p, ex * 1.9, ez * 0.55, c),
                Shape::Cube => {
                    let (hx, hy, hz) = (ex * 0.62, ey * 0.62, ez * 0.62);
                    let cf = c * Vec4::new(1.0, 1.0, 1.0, 0.55); // six faces overlap: each one dimmer
                    for (n, u, v) in [(hx, hy, hz), (hy, hz, hx), (hz, hx, hy)] {
                        face(&mut p, s.p + n, u, v, cf);
                        face(&mut p, s.p - n, u, v, cf);
                    }
                }
                Shape::Prism => {
                    // triangular cross-section in the x/y plane, extruded along z
                    let tri = [ey * 0.8, -ey * 0.4 + ex * 0.7, -ey * 0.4 - ex * 0.7];
                    let half = ez * 0.9;
                    let cf = c * Vec4::new(1.0, 1.0, 1.0, 0.7);
                    for i in 0..3 {
                        let (ta, tb) = (tri[i], tri[(i + 1) % 3]);
                        face(&mut p, s.p + (ta + tb) * 0.5, (tb - ta) * 0.5, half, cf);
                    }
                }
                Shape::Tri => {
                    let t = [s.p + ey * 1.1, s.p - ey * 0.55 + ex * 0.95, s.p - ey * 0.55 - ex * 0.95];
                    p.push(Layer::Add, true, tri4(t), [[0.0; 2]; 4], [c * Vec4::new(1.0, 1.0, 1.0, 0.25); 4], K_SOLID);
                    let w = s.size * 0.16;
                    for i in 0..3 {
                        p.bar(Layer::Add, t[i], t[(i + 1) % 3], w, c, c, true);
                    }
                }
            }
        }

        // ── HUD (Twinise editorial instrument) ──
        let fs = (h * 0.0118).max(12.0) * self.hud_scale;
        let m = h * 0.034;
        let hud_a = if em { smooth01((prog - 0.55) / 0.3) } else { 1.0 };
        let ink = Vec4::new(0.80, 0.84, 0.90, 0.55 * hud_a);
        let dim = Vec4::new(0.80, 0.84, 0.90, 0.30 * hud_a);
        let hot = Vec4::new(orange().x * 1.1, orange().y * 1.2, orange().z * 1.4, 0.95 * hud_a);
        let live = 0.55 + 0.45 * (t * 3.0).sin();
        let ty = m;
        p.text(m, ty, fs, &format!("SYS.ID // {}-01  ·  SECTOR {:02}", brand::get().name, sim.sector), ink);
        let s = &match sim.scenario {
            Scenario::Emerge => format!("[ {} // ENTERING THE GRID ]", brand::get().name),
            Scenario::Arena => "[ GAME // LIGHT CYCLES ]".to_string(),
            Scenario::Twin => "[ DIGITAL TWIN SYSTEMS ]".to_string(),
            Scenario::Run => format!("[ TRANSIT // ENTERING SECTOR {:02} ]", sim.sector + 1),
        };
        let sw = p.text_width(fs, s);
        p.text((w - sw) * 0.5, ty, fs, s, ink);
        let st = "STATUS // ● LIVE";
        let stw = p.text_width(fs, st);
        p.text(w - m - stw, ty, fs, "STATUS // ", dim);
        p.text(w - m - p.text_width(fs, "● LIVE"), ty, fs, "● LIVE", hot * Vec4::new(1.0, 1.0, 1.0, live));
        let _ = stw;

        let ly = ty + fs * 1.9;
        let lw = w - 2.0 * m;
        let th = (h / 1400.0).max(1.0);
        p.rect(m, ly, lw, th, Vec4::new(0.8, 0.85, 0.9, 0.16 * hud_a));
        p.rect(m, ly, lw * 0.18, th, orange().extend(0.9 * hud_a));
        let by = h - m - fs;
        let ly2 = by - fs * 0.9;
        p.rect(m, ly2, lw, th, Vec4::new(0.8, 0.85, 0.9, 0.16 * hud_a));
        p.rect(m + lw * 0.82, ly2, lw * 0.18, th, orange().extend(0.9 * hud_a));

        let secs = t as u32;
        let left = match sim.scenario {
            Scenario::Emerge => format!(
                "T+ {:02}:{:02}:{:02}  ·  GRID INIT {:3.0}%  ·  {:.0} FPS",
                secs / 3600, secs / 60 % 60, secs % 60, prog * 100.0, fps
            ),
            Scenario::Arena => format!(
                "T+ {:02}:{:02}:{:02}  ·  UNITS {:02}  ·  ORANGE {:02}  ·  BLUE {:02}  ·  {:.0} FPS",
                secs / 3600, secs / 60 % 60, secs % 60, sim.alive(), sim.kills[0], sim.kills[1], fps
            ),
            Scenario::Twin => format!(
                "T+ {:02}:{:02}:{:02}  ·  TWIN SYNC {:5.1}%  ·  {:.0} FPS",
                secs / 3600, secs / 60 % 60, secs % 60, sync, fps
            ),
            Scenario::Run => format!(
                "T+ {:02}:{:02}:{:02}  ·  VELOCITY {:03.0} u/s  ·  {:.0} FPS",
                secs / 3600, secs / 60 % 60, secs % 60, sim.run_speed, fps
            ),
        };
        p.text(m, by, fs, &left, ink);
        let mid = "[ OMARCHY LINUX ]";
        p.text((w - p.text_width(fs, mid)) * 0.5, by, fs, mid, ink);
        let rt = brand::get().footer.as_str();
        p.text(w - m - p.text_width(fs, rt), by, fs, rt, ink);

        // game readouts: kill feed (left) and camera / sector clock (right)
        let line_y = ly + fs * 1.5;
        if sim.scenario == Scenario::Arena {
            for (i, (msg, life, fc)) in sim.feed.iter().rev().enumerate() {
                let c = *fc;
                let a = (life / 1.5).min(1.0) * 0.9;
                p.text(m, line_y + i as f32 * fs * 1.35, fs, msg, Vec4::new(c.x, c.y + 0.12, c.z + 0.05, a));
                let _ = Team::Red;
            }
        }
        let remain = format!("SECTOR CLOSES IN {:02}s", (sim.scenario.duration() - sim.scen_t).max(0.0) as u32);
        if !matches!(sim.scenario, Scenario::Run | Scenario::Emerge) && !sim.hold_arena {
            p.text(w - m - p.text_width(fs, &remain), line_y + fs * 1.35, fs, &remain, dim);
        }

        // live feed readout
        if matches!(sim.scenario, Scenario::Arena | Scenario::Twin) {
            let fy = line_y + fs * 2.9;
            let head = match sim.online {
                Some(true) => "FEED // OMACOM/OMARCHY · LIVE",
                Some(false) => "FEED // OMACOM/OMARCHY · OFFLINE REPLAY",
                None => "FEED // CONNECTING",
            };
            p.text(w - m - p.text_width(fs, head), fy, fs, head, dim);
            if sim.stars > 0 {
                let st = format!("STARS {}", sim.stars);
                p.text(w - m - p.text_width(fs, &st), fy + fs * 1.35, fs, &st, ink);
            }
            // who did what, newest on top, colour-coded by event type
            for (i, l) in sim.ev_log.iter().enumerate() {
                let a = smooth01(l.life / 0.3) * (1.0 - smooth01((l.life - 24.0) / 4.0)) * (1.0 - 0.12 * i as f32);
                if a < 0.01 {
                    continue;
                }
                let c = l.kind.color();
                let y = fy + fs * (2.9 + 1.45 * i as f32);
                let rest = format!("  {}  · {}", l.what, l.age);
                let (ww, rw) = (p.text_width(fs, &l.who), p.text_width(fs, &rest));
                let x0 = w - m - ww - rw;
                p.rect(x0 - fs * 0.9, y + fs * 0.32, fs * 0.42, fs * 0.42, Vec4::new(c.x, c.y, c.z, a));
                p.text(x0, y, fs, &l.who, Vec4::new(c.x, c.y, c.z, 0.95 * a));
                p.text(x0 + ww, y, fs, &rest, Vec4::new(0.80 + 0.2 * c.x, 0.80 + 0.2 * c.y, 0.80 + 0.2 * c.z, 0.62 * a));
            }
            // legend, just above the bottom rule
            {
                let gap = fs * 1.4;
                let total: f32 = crate::feed::Kind::LEGEND.iter().map(|(_, n)| fs * 0.9 + p.text_width(fs, n) + gap).sum::<f32>() - gap;
                let mut x = w - m - total;
                let y = ly2 - fs * 1.7;
                for (k, n) in crate::feed::Kind::LEGEND {
                    let c = k.color();
                    p.rect(x, y + fs * 0.32, fs * 0.42, fs * 0.42, Vec4::new(c.x, c.y, c.z, 0.9));
                    x += fs * 0.9;
                    p.text(x, y, fs, n, Vec4::new(c.x, c.y, c.z, 0.75));
                    x += p.text_width(fs, n) + gap;
                }
            }
        }

        // corner brackets
        let bl = fs * 1.6;
        let bm = m * 0.45;
        for (x, y, sx, sy) in [(bm, bm, 1.0, 1.0), (w - bm, bm, -1.0, 1.0), (bm, h - bm, 1.0, -1.0), (w - bm, h - bm, -1.0, -1.0)] {
            p.rect(x.min(x + sx * bl), y, bl, th, dim);
            p.rect(x, y.min(y + sy * bl), th, bl, dim);
        }

        // the desktop background's lAvArt Studio lockup + ruler, fading as the world rises out of it
        let br = brand::get();
        if em && (br.desktop_grid || !br.wordmark.is_empty() || !br.tagline.is_empty()) {
            let intro = if br.logo_on_wallpaper { 1.0 } else { smooth01((sim.scen_t - 0.8) / 2.6) };
            let la = (1.0 - smooth01(prog / 0.30)) * intro;
            if la > 0.003 {
                let sc = h / 2000.0;
                let lx = 144.0 * sc;
                let fgc = |a: f32| bgc(fg_grey(), a * la);
                let acc = |a: f32| bgc(orange(), a * la);
                let pulse = 0.5 + 0.5 * (sim.scen_t * 2.0 * std::f32::consts::PI / 2.4).sin();
                let bottom = h - 144.0 * sc;
                // status row
                let ys = bottom - 22.0 * sc;
                let sfs = 24.0 * sc * 1.2;
                let yr = ys - 20.0 * sc;
                if br.desktop_grid {
                p.rect(lx, ys + 7.0 * sc, 12.0 * sc, 12.0 * sc, acc(0.55 + 0.45 * pulse));
                p.text_ex(F_MONO, lx + 30.0 * sc, ys, sfs, 4.0 * sc, "LIVE", fgc(0.7));
                let secs = sim.scen_t as u32;
                p.text_ex(F_MONO, lx + 150.0 * sc, ys, sfs, 4.0 * sc, &format!("NODES 08   T+ {:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60), fgc(0.45));
                // accent rule with a travelling signal
                p.rect(lx, yr, 440.0 * sc, 2.0 * sc, acc(0.35));
                let su = frac(sim.scen_t / 3.2);
                p.rect(lx + su * (440.0 - 88.0) * sc, yr, 88.0 * sc, 2.0 * sc, acc((std::f32::consts::PI * su).sin()));
                }
                // wordmark (e.g. "lAvArt Studio": first word bright, second dim)
                let em_px = h / 14.0;
                let dsz = em_px * 1.28;
                let yt = yr - 20.0 * sc - em_px * 1.15;
                let mut wx = lx;
                for (i, word) in br.wordmark.iter().take(2).enumerate() {
                    p.text_ex(F_DISPLAY, wx, yt, dsz, -2.0 * sc, word, fgc(if i == 0 { 0.92 } else { 0.45 }));
                    wx += p.text_width_ex(F_DISPLAY, dsz, -2.0 * sc, &format!("{word} ")) + 14.0 * sc;
                }
                // caption
                let ty = if br.wordmark.is_empty() { yr - 20.0 * sc - 24.0 * sc * 1.2 } else { yt - 20.0 * sc - 24.0 * sc * 1.2 };
                p.text_ex(F_MONO, lx, ty, 24.0 * sc * 1.2, 6.0 * sc, &br.tagline, fgc(0.55));
                if br.desktop_grid {
                // ruler along the bottom edge, riding the grid drift
                let cpx = 96.0 * sc;
                let drift = (sim.scen_t * cpx / 28.0) % (4.0 * cpx);
                let nt = (w / cpx) as i32 + 6;
                for i in -4..nt {
                    let x = i as f32 * cpx + drift;
                    let major = i.rem_euclid(4) == 0;
                    let hh = if major { 28.0 } else { 14.0 } * sc;
                    p.rect(x, h - hh, 2.0 * sc, hh, fgc(if major { 0.4 } else { 0.2 }));
                    if major {
                        p.text_ex(F_MONO, x + 10.0 * sc, h - 58.0 * sc, 20.0 * sc * 1.2, 2.0 * sc, &format!("{:03}", (720 + 4 * (i / 4)).rem_euclid(1000)), fgc(0.35));
                    }
                }
                }
            }
        }

        // scene-change flash
        if sim.flash > 0.002 {
            let a = sim.flash * sim.flash;
            p.screen(Layer::Add, 0.0, 0.0, w, h, [[0.0; 2]; 4], Vec4::new(1.0, 0.78, 0.6, a * 0.9), K_SOLID);
        }


        // shockwave uniforms
        let mut pu = [Vec4::ZERO; 4];
        let mut pc = [Vec4::ZERO; 4];
        for (i, s) in sim.pulses.iter().rev().take(4).enumerate() {
            pu[i] = Vec4::new(s.o.x, s.o.y, s.r, s.strength);
            pc[i] = Vec4::new(s.color.x, s.color.y, s.color.z, 1.0);
        }
        let _ = Vec2::ZERO;
        (cam, pu, pc)
    }
}
