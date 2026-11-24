//! The ASCII style: the same light-cycle battle, drawn the way a 1990s text-mode terminal would have drawn it.
//!
//! Everything on screen is a character in a fixed grid. The characters are the HUD font's glyphs, drawn on the scene
//! layer so they bloom like phosphor; the CRT look itself (curvature, scanlines, flicker) is done by the composite
//! pass in `pp.wgsl`. The simulation underneath is the very same one the 3D style uses: its cycles, trails, falling
//! GitHub events and debris are just read off from above and snapped to the grid.

use crate::brand;
use crate::feed::Kind;
use crate::scene::{Atlas, Frame, Vertex};
use crate::sim::{Scenario, Shape, Sim, ARENA_X, ARENA_Z0, ARENA_Z1};
use glam::{Vec2, Vec3};

const K_TEXT: f32 = 2.0;

/// the terminal's ink: classic Tron cyan
const INK: Vec3 = Vec3::new(0.30, 0.86, 1.0);
const HOT: Vec3 = Vec3::new(0.86, 1.0, 1.0);

const BOOT_SECS: f32 = 10.5;
const END_SECS: f32 = 6.5;

/// how long one battle runs before the end-of-line screen (GRIDFALL_ASCII_ARENA_SECS shortens it for testing)
fn arena_secs() -> f32 {
    std::env::var("GRIDFALL_ASCII_ARENA_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(150.0)
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Boot,
    Arena,
    End,
}

pub struct Ascii {
    phase: Phase,
    /// seconds into the current phase
    t: f32,
    /// seconds since the start (blinking cursors, glitches)
    total: f32,
    /// the logo as characters, per size it was asked for
    art: Vec<((usize, usize), Vec<Vec<char>>)>,
    end_kills: [u32; 2],
    end_sector: u32,
}

#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    col: Vec3,
    a: f32,
}

struct Screen {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
}

impl Screen {
    fn new(cols: usize, rows: usize) -> Self {
        Screen { cols, rows, cells: vec![Cell { ch: ' ', col: Vec3::ZERO, a: 0.0 }; cols * rows] }
    }

    fn put(&mut self, c: i32, r: i32, ch: char, col: Vec3, a: f32) {
        if c < 0 || r < 0 || c >= self.cols as i32 || r >= self.rows as i32 {
            return;
        }
        self.cells[r as usize * self.cols + c as usize] = Cell { ch, col, a };
    }

    fn text(&mut self, c: i32, r: i32, s: &str, col: Vec3, a: f32) {
        for (i, ch) in s.chars().enumerate() {
            if ch != ' ' {
                self.put(c + i as i32, r, ch, col, a);
            }
        }
    }

    fn centered(&mut self, r: i32, s: &str, col: Vec3, a: f32) {
        let c = (self.cols as i32 - s.chars().count() as i32) / 2;
        self.text(c, r, s, col, a);
    }

    fn centered_in(&mut self, c0: i32, c1: i32, r: i32, s: &str, col: Vec3, a: f32) {
        let c = c0 + (c1 - c0 - s.chars().count() as i32) / 2 + 1;
        self.text(c, r, s, col, a);
    }

    /// `+---[ TITLE ]---+`
    fn frame(&mut self, c0: i32, r0: i32, c1: i32, r1: i32, title: &str, col: Vec3) {
        for c in c0 + 1..c1 {
            self.put(c, r0, '-', col, 0.55);
            self.put(c, r1, '-', col, 0.55);
        }
        for r in r0 + 1..r1 {
            self.put(c0, r, '|', col, 0.55);
            self.put(c1, r, '|', col, 0.55);
        }
        for (c, r) in [(c0, r0), (c1, r0), (c0, r1), (c1, r1)] {
            self.put(c, r, '+', col, 0.9);
        }
        if !title.is_empty() {
            for (i, ch) in format!("[ {title} ]").chars().enumerate() {
                self.put(c0 + 2 + i as i32, r0, ch, HOT, if ch == ' ' { 0.0 } else { 0.95 });
            }
        }
    }
}

/// The arena's rectangle on the character grid (interior, inclusive).
struct Map {
    c0: i32,
    r0: i32,
    c1: i32,
    r1: i32,
}

impl Map {
    /// world (x, z) → grid cell; None when well outside the arena
    fn cell(&self, p: Vec2) -> Option<(i32, i32)> {
        let u = (p.x + ARENA_X) / (2.0 * ARENA_X);
        let v = (p.y - ARENA_Z0) / (ARENA_Z1 - ARENA_Z0);
        if !(-0.03..=1.03).contains(&u) || !(-0.03..=1.03).contains(&v) {
            return None;
        }
        let (u, v) = (u.clamp(0.0, 1.0), v.clamp(0.0, 1.0));
        Some((self.c0 + (u * (self.c1 - self.c0) as f32).round() as i32, self.r1 - (v * (self.r1 - self.r0) as f32).round() as i32))
    }
}

fn kind_char(k: Kind) -> char {
    match k {
        Kind::Star => 'S',
        Kind::Fork => 'F',
        Kind::Push => 'P',
        Kind::Pr => 'R',
        Kind::Merge => 'M',
        Kind::Comment => 'C',
        Kind::Issue => 'I',
        Kind::Other => '?',
    }
}

/// a colour at full strength: its brightest channel at 1
fn full(c: Vec3) -> Vec3 {
    let m = c.x.max(c.y).max(c.z).max(1e-3);
    c / m
}

fn fit(s: &str, n: usize) -> String {
    let s: String = s.chars().map(|c| if c.is_ascii() && !c.is_control() { c } else { '?' }).collect();
    if s.chars().count() <= n {
        s
    } else if n > 2 {
        s.chars().take(n - 2).collect::<String>() + ".."
    } else {
        s.chars().take(n).collect()
    }
}

fn hash(a: u32, b: u32) -> u32 {
    let mut h = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^ (h >> 15)
}

/// Garbage across a few rows, as a bad signal would do: used where one screen hands over to the next.
fn glitch(scr: &mut Screen, frame: u32, amount: f32) {
    let bands = (amount * 9.0) as u32;
    for k in 0..bands {
        let r = (hash(frame, k) % scr.rows as u32) as i32;
        let c0 = (hash(frame, k + 77) % scr.cols as u32) as i32;
        let len = 6 + (hash(frame, k + 31) % (scr.cols as u32 / 2).max(1)) as i32;
        for c in c0..(c0 + len).min(scr.cols as i32) {
            let ch = (b'!' + (hash(frame + c as u32, k) % 90) as u8) as char;
            scr.put(c, r, ch, INK, 0.35 + 0.5 * amount);
        }
    }
}

/// Draw a polyline onto the grid with `=` and `|`, fading toward the tail; returns its head cell.
fn trace(scr: &mut Screen, map: &Map, pts: &[Vec2], col: Vec3, a: f32) -> Option<(i32, i32)> {
    let cells: Vec<Option<(i32, i32)>> = pts.iter().map(|&p| map.cell(p)).collect();
    // every cell the path crosses, tail → head
    let mut path: Vec<(i32, i32, char)> = Vec::new();
    for w in cells.windows(2) {
        let (Some(a0), Some(b0)) = (w[0], w[1]) else { continue };
        let (dx, dy) = (b0.0 - a0.0, b0.1 - a0.1);
        let n = dx.abs().max(dy.abs()).max(1);
        let ch = if dy == 0 {
            '='
        } else if dx == 0 {
            '|'
        } else if dx.abs() >= dy.abs() * 2 {
            '='
        } else if dy.abs() >= dx.abs() * 2 {
            '|'
        } else if (dx > 0) == (dy > 0) {
            '\\'
        } else {
            '/'
        };
        for i in 0..=n {
            let p = (a0.0 + dx * i / n, a0.1 + dy * i / n);
            if path.last().map(|l| (l.0, l.1)) != Some(p) {
                path.push((p.0, p.1, ch));
            }
        }
    }
    // corners where the direction changes
    for i in 1..path.len().saturating_sub(1) {
        if path[i].2 != path[i - 1].2 && path[i].2 != path[i + 1].2 {
            path[i].2 = '+';
        } else if path[i].2 != path[i - 1].2 && path[i - 1].2 != '+' {
            path[i - 1].2 = '+';
        }
    }
    let n = path.len().max(1) as f32;
    for (i, &(c, r, ch)) in path.iter().enumerate() {
        let f = (i as f32 + 1.0) / n;
        scr.put(c, r, ch, col * (0.45 + 0.55 * f), a * (0.30 + 0.70 * f));
    }
    path.last().map(|p| (p.0, p.1))
}

/// The brand's logo as characters: bright `#` on its outline, a dim `:` over its body.
fn logo_art(max_cols: usize, max_rows: usize) -> Vec<Vec<char>> {
    let logo = &brand::get().logo;
    let (u0, v0, u1, v1) = logo.bbox;
    let (lw, lh) = ((u1 - u0).max(1.0), (v1 - v0).max(1.0));
    // a character cell is about twice as tall as it is wide
    let mut rows = max_rows.min(24).max(6);
    let mut cols = (lw / lh * rows as f32 * 2.0).round() as usize;
    if cols > max_cols {
        cols = max_cols;
        rows = ((cols as f32 / 2.0) * lh / lw).round().max(4.0) as usize;
    }
    let cols = cols.max(4);
    let tris: Vec<(&[(f32, f32); 3], bool)> = logo.facets.iter().flat_map(|f| f.tris.iter().map(move |t| (t, f.role == brand::Role::Glow))).collect();
    let inside = |x: f32, y: f32| -> Option<bool> {
        for (t, glow) in &tris {
            let s = |a: (f32, f32), b: (f32, f32)| (x - b.0) * (a.1 - b.1) - (a.0 - b.0) * (y - b.1);
            let (d1, d2, d3) = (s(t[0], t[1]), s(t[1], t[2]), s(t[2], t[0]));
            let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
            let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
            if !(neg && pos) {
                return Some(*glow);
            }
        }
        None
    };
    // coverage per cell from 3×3 samples
    let mut cover = vec![vec![0u8; cols]; rows];
    let mut glow = vec![vec![false; cols]; rows];
    for r in 0..rows {
        for c in 0..cols {
            for sy in 0..3 {
                for sx in 0..3 {
                    let x = u0 + (c as f32 + (sx as f32 + 0.5) / 3.0) / cols as f32 * lw;
                    let y = v0 + (r as f32 + (sy as f32 + 0.5) / 3.0) / rows as f32 * lh;
                    if let Some(g) = inside(x, y) {
                        cover[r][c] += 1;
                        glow[r][c] |= g;
                    }
                }
            }
        }
    }
    let solid = |r: i32, c: i32| r >= 0 && c >= 0 && (r as usize) < rows && (c as usize) < cols && cover[r as usize][c as usize] >= 5;
    let mut out = vec![vec![' '; cols]; rows];
    for r in 0..rows {
        for c in 0..cols {
            let n = cover[r][c];
            out[r][c] = if n == 0 {
                ' '
            } else if n < 5 {
                '+'
            } else if glow[r][c] {
                '@'
            } else {
                let (ri, ci) = (r as i32, c as i32);
                if !solid(ri - 1, ci) || !solid(ri + 1, ci) || !solid(ri, ci - 1) || !solid(ri, ci + 1) { '#' } else { ':' }
            };
        }
    }
    out
}

impl Ascii {
    /// the logo as characters, fitting in `cols` × `rows` (made once per size)
    fn art(&mut self, cols: usize, rows: usize) -> &Vec<Vec<char>> {
        let at = match self.art.iter().position(|a| a.0 == (cols, rows)) {
            Some(i) => i,
            None => {
                self.art.push(((cols, rows), logo_art(cols, rows)));
                self.art.len() - 1
            }
        };
        &self.art[at].1
    }

    pub fn new() -> Self {
        Ascii { phase: Phase::Boot, t: 0.0, total: 0.0, art: Vec::new(), end_kills: [0, 0], end_sector: 0 }
    }

    /// Advance the screen sequence: boot → the arena → end of line → boot …
    pub fn update(&mut self, sim: &mut Sim, dt: f32) {
        self.t += dt;
        self.total += dt;
        match self.phase {
            Phase::Boot if self.t >= BOOT_SECS => {
                self.phase = Phase::Arena;
                self.t = 0.0;
                // a fresh arena: the 3D style's own opening is skipped
                sim.jump(Scenario::Arena);
                sim.kills = [0, 0];
                sim.feed.clear();
            }
            Phase::Arena if self.t >= arena_secs() => {
                self.phase = Phase::End;
                self.t = 0.0;
                self.end_kills = sim.kills;
                self.end_sector = sim.sector;
            }
            Phase::End if self.t >= END_SECS => {
                self.phase = Phase::Boot;
                self.t = 0.0;
            }
            _ => {}
        }
    }

    pub fn build(&mut self, sim: &Sim, atlas: &Atlas, frame: &mut Frame, w: f32, h: f32) {
        frame.clear();
        let Some(m) = atlas.mono('M') else { return };
        // 160 × 50 on a 16:10 screen
        let s = (h / 50.0).max(12.0);
        let cw = m.adv * s;
        let cols = ((w / cw).floor() as usize).max(60);
        let rows = ((h / s).floor() as usize).max(24);
        let (ox, oy) = ((w - cols as f32 * cw) * 0.5, (h - rows as f32 * s) * 0.5);
        let mut scr = Screen::new(cols, rows);
        let frame_no = (self.total * 60.0) as u32;
        match self.phase {
            Phase::Boot => {
                self.boot(&mut scr, sim);
                if self.t < 0.35 {
                    glitch(&mut scr, frame_no / 2, 1.0 - self.t / 0.35);
                }
            }
            Phase::Arena => {
                self.arena(&mut scr, sim);
                if self.t < 0.6 {
                    glitch(&mut scr, frame_no / 2, 1.0 - self.t / 0.6);
                }
            }
            Phase::End => {
                self.end(&mut scr);
                if self.t > END_SECS - 0.5 {
                    glitch(&mut scr, frame_no / 2, (self.t - (END_SECS - 0.5)) / 0.5);
                }
            }
        }
        // the characters, as glyph quads on the scene layer
        for r in 0..rows {
            for c in 0..cols {
                let cell = scr.cells[r * cols + c];
                if cell.ch == ' ' || cell.a <= 0.003 {
                    continue;
                }
                let Some(g) = atlas.mono(cell.ch).or_else(|| atlas.mono('?')) else { continue };
                let (x0, y0) = (ox + c as f32 * cw, oy + r as f32 * s);
                let ndc = |x: f32, y: f32| [x / w * 2.0 - 1.0, 1.0 - y / h * 2.0, 0.0, 1.0];
                let col = [cell.col.x, cell.col.y, cell.col.z, cell.a];
                let v = |x: f32, y: f32, u: f32, vv: f32| Vertex { pos: ndc(x, y), col, uv: [u, vv], kind: K_TEXT, _pad: 0.0 };
                frame.a.extend_from_slice(&[
                    v(x0, y0 + s, g.u0, g.v0 + g.dv),
                    v(x0 + s, y0 + s, g.u0 + g.du, g.v0 + g.dv),
                    v(x0 + s, y0, g.u0 + g.du, g.v0),
                    v(x0, y0, g.u0, g.v0),
                ]);
            }
        }
    }

    // ───────────────────────── boot ─────────────────────────
    fn boot(&mut self, scr: &mut Screen, sim: &Sim) {
        let t = self.t;
        let b = brand::get();
        let link = match sim.online {
            None => "SYNC",
            Some(true) => "ONLINE",
            Some(false) => "CACHED",
        };
        let bar = ((t - 3.6) / 1.5).clamp(0.0, 1.0);
        let filled = (bar * 24.0) as usize;
        let lines: [(f32, String, Vec3); 6] = [
            (0.3, "GRIDFALL BIOS v1.0   (C) 1992 LAVART STUDIO".into(), HOT),
            (1.2, "MEMORY TEST ................ 640K OK".into(), INK),
            (1.9, "SCANNING IDENTITY DISCS .... 2 FOUND".into(), INK),
            (2.6, format!("LINK {} {} {}", b.repo.to_uppercase(), ".".repeat(24usize.saturating_sub(b.repo.len() + 2)), link), INK),
            (3.4, format!("LOADING GRID [{}{}] {:3.0}%", "#".repeat(filled), "-".repeat(24 - filled), bar * 100.0), INK),
            (5.4, "> RUN LIGHTCYCLE.EXE".into(), HOT),
        ];
        for (i, (start, text, col)) in lines.iter().enumerate() {
            let n = (((t - start) * 70.0).max(0.0) as usize).min(text.chars().count());
            if t >= *start {
                let shown: String = text.chars().take(n).collect();
                scr.text(3, 2 + i as i32, &shown, *col, 0.95);
                if n < text.chars().count() {
                    scr.put(3 + n as i32, 2 + i as i32, '_', HOT, 1.0);
                }
            }
        }
        // the logo draws itself top to bottom under a bright scan row
        if t >= 5.4 {
            let top = 10;
            let free_rows = (scr.rows as i32 - top - 6).max(6) as usize;
            // a wide wordmark shouldn't fill the whole screen
            let art = self.art((scr.cols * 6 / 10).min(scr.cols.saturating_sub(10)), free_rows).clone();
            let reveal = (((t - 5.6) / 2.4).clamp(0.0, 1.0) * art.len() as f32) as usize;
            let c0 = (scr.cols as i32 - art.first().map_or(0, |r| r.len()) as i32) / 2;
            for (r, row) in art.iter().enumerate().take(reveal + 1) {
                let scan = r == reveal && reveal < art.len();
                for (c, &ch) in row.iter().enumerate() {
                    if scan {
                        scr.put(c0 + c as i32, top + r as i32, if ch == ' ' { '-' } else { ch }, HOT, 1.0);
                    } else if ch != ' ' {
                        let (col, a) = match ch {
                            '#' => (HOT, 1.0),
                            '@' => (Vec3::ONE, 1.0),
                            ':' => (INK, 0.45),
                            _ => (INK, 0.75),
                        };
                        scr.put(c0 + c as i32, top + r as i32, ch, col, a);
                    }
                }
            }
            if t > 8.0 {
                let r = top + art.len() as i32 + 2;
                scr.centered(r, &format!("[ {} ]", b.name), HOT, 1.0);
                if !b.tagline.is_empty() {
                    scr.centered(r + 1, &fit(&b.tagline, scr.cols.saturating_sub(8)), INK, 0.7);
                }
                if (self.total * 2.0) as i32 % 2 == 0 {
                    scr.centered(r + 3, "> ENTER THE GRID <", HOT, 1.0);
                }
            }
        }
    }

    // ───────────────────────── end of line ─────────────────────────
    fn end(&mut self, scr: &mut Screen) {
        let t = self.t;
        let mid = scr.rows as i32 / 2 - 5;
        let lines: [(f32, String, Vec3); 7] = [
            (0.2, "*** END OF LINE ***".into(), HOT),
            (1.0, format!("SECTOR {:03} CLOSED", self.end_sector), INK),
            (1.6, format!("PROGRAMS SCORED ........ {:02}", self.end_kills[0]), brand::get().teams[0]),
            (2.2, format!("USERS SCORED ........... {:02}", self.end_kills[1]), brand::get().teams[1]),
            (3.2, "GRID RESET ................ OK".into(), INK),
            (3.9, "RETURNING TO IDENTITY DISC".into(), INK),
            (4.6, "> _".into(), HOT),
        ];
        for (i, (start, text, col)) in lines.iter().enumerate() {
            if t < *start {
                continue;
            }
            let n = (((t - start) * 60.0).max(0.0) as usize).min(text.chars().count());
            let shown: String = text.chars().take(n).collect();
            // centred on the whole line, so it types out in place
            let c = (scr.cols as i32 - text.chars().count() as i32) / 2;
            scr.text(c, mid + i as i32 * 2, &shown, full(*col), 0.95);
        }
    }

    // ───────────────────────── the arena ─────────────────────────
    fn arena(&mut self, scr: &mut Screen, sim: &Sim) {
        let b = brand::get();
        let (cols, rows) = (scr.cols as i32, scr.rows as i32);
        let panel_w = (cols / 4).clamp(26, 38);
        let top = 2;
        let bottom = rows - 3;
        let (ac0, ac1) = (0, cols - panel_w - 3);
        let map = Map { c0: ac0 + 1, r0: top + 1, c1: ac1 - 1, r1: bottom - 1 };

        // title bar
        let link = match sim.online {
            None => "SYNC",
            Some(true) => "ONLINE",
            Some(false) => "CACHED",
        };
        scr.text(1, 0, &format!("GRIDFALL :: {} GRID :: SECTOR {:03}", b.name, sim.sector), HOT, 1.0);
        let t = self.t as u32;
        let right = format!("LINK {link}   STARS {}   T+{:02}:{:02}", sim.stars, t / 60, t % 60);
        scr.text(cols - right.chars().count() as i32 - 1, 0, &right, INK, 0.85);
        for c in 0..cols {
            scr.put(c, 1, '=', INK, 0.25);
        }

        // arena: frame, then a faint lattice
        scr.frame(ac0, top, ac1, bottom, &format!("ARENA {:02}", sim.alive()), INK);
        for r in (map.r0..=map.r1).filter(|r| (map.r1 - r) % 4 == 0) {
            for c in (map.c0..=map.c1).filter(|c| (c - map.c0) % 8 == 0) {
                scr.put(c, r, '.', INK, 0.28);
            }
        }

        // event trails: one colour per kind of event, with the author's name at the head
        for st in &sim.streaks {
            if st.pts.len() < 2 {
                continue;
            }
            let col = full(st.kind.color());
            let head = trace(scr, &map, &st.pts, col, 0.85 * (0.4 + 0.6 * st.live));
            if let (Some((c, r)), true) = (head, st.live > 0.15) {
                scr.put(c, r, kind_char(st.kind), Vec3::ONE.lerp(col, 0.25), 1.0);
                if let Some(who) = &st.who {
                    let label = format!("@{}", fit(who, 14));
                    let len = label.chars().count() as i32;
                    let lc = if c + 2 + len > map.c1 { c - 1 - len } else { c + 2 };
                    scr.text(lc, r, &label, col, 0.9 * st.live);
                }
            }
        }

        // light-cycle trails, then the cycles
        for cy in &sim.cycles {
            if cy.alive && cy.pts.len() >= 2 {
                trace(scr, &map, &cy.pts, full(cy.team.color()), 1.0);
            }
        }

        // falling events: the kind's letter drops toward its landing point
        for d in &sim.drops {
            let Some((c, r)) = map.cell(Vec2::new(d.x, d.z)) else { continue };
            let col = full(d.kind.color());
            scr.put(c, r, 'o', col, 0.3);
            let lift = (d.y * 0.10).round() as i32;
            for k in 1..=lift.min(4) {
                scr.put(c, r - k, ':', col, 0.35);
            }
            let blink = if (self.total * 8.0) as i32 % 2 == 0 { 1.0 } else { 0.7 };
            scr.put(c, r - lift, kind_char(d.kind), col, blink);
        }

        // derez shock rings
        for p in &sim.pulses {
            let a = (p.strength * 0.9).clamp(0.0, 1.0);
            if a < 0.02 {
                continue;
            }
            let n = ((p.r * 3.0) as usize).clamp(14, 96);
            for k in 0..n {
                let th = k as f32 / n as f32 * std::f32::consts::TAU;
                if let Some((c, r)) = map.cell(Vec2::new(p.o.x + p.r * th.cos(), p.o.y + p.r * th.sin())) {
                    scr.put(c, r, if a > 0.5 { 'o' } else { '.' }, full(p.color), a);
                }
            }
        }

        // debris
        for sh in &sim.shards {
            let Some((c, r)) = map.cell(Vec2::new(sh.p.x, sh.p.z)) else { continue };
            let life = (sh.life / sh.max.max(0.01)).clamp(0.0, 1.0);
            let ch = if life > 0.55 {
                match sh.shape {
                    Shape::Cube => '#',
                    Shape::Prism => '^',
                    Shape::Rect => '=',
                    Shape::Tri => '*',
                }
            } else if life > 0.25 {
                '+'
            } else {
                '.'
            };
            scr.put(c, r - (sh.p.y * 0.12).round() as i32, ch, full(sh.color), 0.4 + 0.6 * life);
        }

        // the cycles' heads on top of everything
        for cy in &sim.cycles {
            if !cy.alive {
                continue;
            }
            if let Some((c, r)) = map.cell(cy.pos) {
                let col = full(cy.team.color());
                let ch = if cy.spawn_flash > 0.3 && (self.total * 14.0) as i32 % 2 == 0 { '*' } else { '@' };
                scr.put(c, r, ch, Vec3::ONE.lerp(col, 0.35), 1.0);
            }
        }

        self.panel(scr, sim, cols - panel_w - 1, top, cols - 1, bottom);

        // prompt line
        let repo = fit(&b.repo.to_uppercase(), 40);
        scr.text(1, rows - 1, &format!("C:\\GRIDFALL> LIGHTCYCLE.EXE /REPO:{repo}"), INK, 0.9);
        let plen = format!("C:\\GRIDFALL> LIGHTCYCLE.EXE /REPO:{repo}").chars().count() as i32;
        if (self.total * 2.0) as i32 % 2 == 0 {
            scr.put(2 + plen, rows - 1, '#', HOT, 1.0);
        }
        if !b.footer.is_empty() {
            let f = fit(&b.footer.to_uppercase(), 40);
            scr.text(cols - f.chars().count() as i32 - 1, rows - 1, &f, INK, 0.6);
        }
    }

    /// Right-hand column: score, event log, identity, kill feed, legend.
    fn panel(&mut self, scr: &mut Screen, sim: &Sim, c0: i32, top: i32, c1: i32, bottom: i32) {
        let b = brand::get();
        let inner = (c1 - c0 - 3).max(8) as usize;
        let teams = [full(b.teams[0]), full(b.teams[1])];

        // score
        scr.frame(c0, top, c1, top + 4, "SCORE", INK);
        let peak = sim.kills[0].max(sim.kills[1]).max(1) as f32;
        for (i, (name, col)) in [("PROGRAMS", teams[0]), ("USERS", teams[1])].into_iter().enumerate() {
            // kills[i] counts what team i derezzed
            let n = sim.kills[i];
            let bar = ((n as f32 / peak) * (inner as f32 - 14.0).max(1.0)).round() as usize;
            let line = format!("{name:<8} {n:02} {}", "|".repeat(bar));
            scr.text(c0 + 2, top + 1 + i as i32, &fit(&line, inner), col, 0.95);
        }
        scr.text(c0 + 2, top + 3, &fit(&format!("{} ALIVE   {} UNITS", sim.alive(), sim.cycles.len()), inner), INK, 0.6);

        // from the bottom up: the legend, then the kill feed
        let lt = bottom - 4;
        let feed_rows = 9;
        let ft = lt - feed_rows;
        // event log: two lines per event, the six most recent
        let (lg0, lg1) = (top + 5, top + 5 + 13);
        scr.frame(c0, lg0, c1, lg1, "EVENT LOG", INK);
        let mut r = lg0 + 1;
        for e in sim.ev_log.iter() {
            if r + 1 >= lg1 {
                break;
            }
            let col = full(e.kind.color());
            let fade = (1.0 - e.life / 40.0).clamp(0.6, 1.0);
            let head = format!("[{}] {}", kind_char(e.kind), fit(&e.who, inner.saturating_sub(16)));
            scr.text(c0 + 2, r, &head, col, fade);
            let age = fit(&e.age, 12);
            scr.text(c1 - 1 - age.chars().count() as i32, r, &age, INK, 0.5);
            scr.text(c0 + 6, r + 1, &fit(&e.what, inner.saturating_sub(4)), INK, 0.7 * fade);
            r += 2;
        }

        // identity: the logo in characters, in whatever room is left between the log and the kill feed
        let (id0, id1) = (lg1 + 1, ft - 1);
        if id1 - id0 >= 6 {
            scr.frame(c0, id0, c1, id1, "IDENTITY", INK);
            let (aw, ah) = (inner.saturating_sub(2), (id1 - id0 - 3) as usize);
            let mut art = self.art(aw, ah).clone();
            // a wide wordmark would be a few unreadable rows: the name alone says it better
            if art.len() < 7 {
                art.clear();
            }
            let w = art.first().map_or(0, |r| r.len()) as i32;
            let (ac, ar) = (c0 + 1 + (c1 - c0 - w) / 2, id0 + 1 + (ah as i32 - art.len() as i32) / 2);
            for (y, row) in art.iter().enumerate() {
                for (x, &ch) in row.iter().enumerate() {
                    let (col, a) = match ch {
                        ' ' => continue,
                        '#' => (HOT, 0.95),
                        '@' => (Vec3::ONE, 1.0),
                        ':' => (INK, 0.35),
                        _ => (INK, 0.7),
                    };
                    scr.put(ac + x as i32, ar + y as i32, ch, col, a);
                }
            }
            let name_row = if art.is_empty() { (id0 + id1) / 2 } else { id1 - 1 };
            scr.centered_in(c0, c1, name_row, &fit(&b.name, inner), HOT, 0.9);
        }

        // kill feed
        scr.frame(c0, ft, c1, lt - 1, "KILL FEED", INK);
        let mut r = ft + 1;
        for (msg, left, col) in sim.feed.iter().rev().take((feed_rows - 2) as usize) {
            scr.text(c0 + 2, r, &fit(msg, inner), full(*col), (left / 2.0).clamp(0.35, 1.0));
            r += 1;
        }

        // legend
        scr.frame(c0, lt, c1, bottom, "LEGEND", INK);
        let mut x = c0 + 2;
        let mut y = lt + 1;
        for (k, name) in Kind::LEGEND {
            let w = name.len() as i32 + 3;
            if x + w > c1 - 1 {
                x = c0 + 2;
                y += 1;
            }
            let col = full(k.color());
            scr.put(x, y, kind_char(k), col, 1.0);
            scr.text(x + 2, y, name, col, 0.7);
            x += w + 1;
        }
    }
}
