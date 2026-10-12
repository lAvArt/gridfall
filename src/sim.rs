//! Physics + behaviour: lightcycles on the grid, derezzed voxel shards,
//! shockwave pulses and the spring-damper monolith. Pure CPU, allocation-light.

use crate::brand;
use crate::feed::{self, Ev, Kind, Msg};
use glam::{Vec2, Vec3};
use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::Receiver;

pub const ARENA_X: f32 = 46.0;
pub const ARENA_Z0: f32 = 8.0;
pub const ARENA_Z1: f32 = 112.0;
pub const CELL: f32 = 4.0;
const TRAIL_LEN: f32 = 34.0;
const MAX_SHARDS: usize = 2600;
const GRAVITY: f32 = 17.0;
pub const MARK_Y: f32 = 17.0;
/// Emergence layout: the desktop background lies flat on the floor and its mark folds up at the far end.
pub const EMERGE_CELL: f32 = 2.65;
pub const EMERGE_MARK_W: f32 = 32.0;
pub const EMERGE_ZB: f32 = ARENA_Z1; // base edge of the standing mark: it stands on the arena's far wall
/// Base edge of the flat (unfolded) mark: it lies inside the arena, its top edge at the far wall, and its wireframe copy
/// stays right there while the coloured mark stands up. Shifted by whole major grid cells (4 × 3 × 2.65) so the mark keeps
/// its registration with the desktop background's grid.
pub const FLAT_ZB: f32 = EMERGE_ZB - 12.0 * EMERGE_CELL;

/// Base edge of the rising mark at a given fold angle: the base slides back to the far wall while the top edge rises
/// almost straight up from where it lay.
pub fn rise_zb(fold: f32) -> f32 {
    FLAT_ZB + (EMERGE_ZB - FLAT_ZB) * (1.0 - fold.cos())
}

/// A logo point (2048² logo space) laid flat on the arena floor in front of the standing mark.
pub fn logo_floor(q: (f32, f32)) -> Vec2 {
    let w = EMERGE_MARK_W;
    Vec2::new(-(q.0 - 1024.0) / 2048.0 * w, FLAT_ZB + (1024.0 - q.1) / 2048.0 * w + w * 0.5)
}

/// One logo line that becomes a light-cycle trail: its polyline, the head position and the heading (index into DIRS).
pub struct SeedLine {
    corners: Vec<Vec2>,
    head: Vec2,
    dir: usize,
}

fn paths_of(lines: &[SeedLine]) -> Vec<Vec<Vec2>> {
    lines
        .iter()
        .map(|l| {
            let mut v = l.corners.clone();
            v.push(l.head);
            v
        })
        .collect()
}

/// Up to six of the logo's longer lines become trails, each in a random direction. A diagonal turns into a two-leg
/// stair (the world only has axis moves). Returns the plan and which of the logo's edges were chosen.
fn plan_seeds(rng: &mut Rng) -> (Vec<SeedLine>, Vec<bool>) {
    let edges = &brand::get().logo.edges;
    // candidates: the longest seedable lines (curvy logos have many short ones)
    let mut idx: Vec<usize> = (0..edges.len()).filter(|&i| edges[i].seedable).collect();
    let len = |i: usize| (edges[i].b.0 - edges[i].a.0).hypot(edges[i].b.1 - edges[i].a.1);
    idx.sort_by(|&x, &y| len(y).total_cmp(&len(x)));
    idx.truncate(18);
    let n = idx.len();
    let take = n.min(6);
    for k in 0..take {
        let j = k + ((rng.f() * (n - k) as f32) as usize).min(n - 1 - k);
        idx.swap(k, j);
    }
    let mut sel = vec![false; edges.len()];
    let mut out = Vec::new();
    for &e in idx.iter().take(take) {
        sel[e] = true;
        let (mut a, mut b) = (logo_floor(edges[e].a), logo_floor(edges[e].b));
        if rng.chance(0.5) {
            std::mem::swap(&mut a, &mut b);
        }
        let (dx, dz) = (b.x - a.x, b.y - a.y);
        let line = if dx.abs() < 0.03 {
            SeedLine { corners: vec![a], head: b, dir: if dz > 0.0 { 0 } else { 2 } }
        } else if dz.abs() < 0.03 {
            SeedLine { corners: vec![a], head: b, dir: if dx > 0.0 { 1 } else { 3 } }
        } else {
            SeedLine { corners: vec![a, Vec2::new(b.x, a.y)], head: b, dir: if dz > 0.0 { 0 } else { 2 } }
        };
        out.push(line);
    }
    (out, sel)
}
const EMERGE_H: f32 = 57.1; // top-down camera height
pub const MARK_Z: f32 = 72.0;
const SCAN_SPEED: f32 = 15.0;
const SCAN_MIN: f32 = -140.0;
const SCAN_MAX: f32 = 140.0;

/// light-cycle team colours: programs (Twinise orange-red by default) and users (Tron blue)
fn team_col(i: usize) -> Vec3 {
    brand::get().teams[i]
}

const DIRS: [Vec2; 4] = [Vec2::new(0.0, 1.0), Vec2::new(1.0, 0.0), Vec2::new(0.0, -1.0), Vec2::new(-1.0, 0.0)];

pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    pub fn f(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    pub fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.f()
    }
    pub fn chance(&mut self, p: f32) -> bool {
        self.f() < p
    }
    fn unit(&mut self) -> Vec3 {
        loop {
            let v = Vec3::new(self.range(-1.0, 1.0), self.range(-1.0, 1.0), self.range(-1.0, 1.0));
            let l = v.length_squared();
            if l > 0.01 && l <= 1.0 {
                return v / l.sqrt();
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Team {
    Red,
    Cyan,
}
impl Team {
    pub fn color(self) -> Vec3 {
        match self {
            Team::Red => team_col(0),
            Team::Cyan => team_col(1),
        }
    }
}

pub struct Cycle {
    pub seeded: bool,
    pub id: usize,
    pub team: Team,
    pub pos: Vec2,
    pub dir: usize,
    pub speed: f32,
    pub alive: bool,
    pub respawn: f32,
    pub age: f32,
    pub spawn_flash: f32,
    straight: f32,
    force_turn: bool,
    prefer: Option<usize>,
    danger_prev: bool,
    react: bool,
    corners: Vec<Vec2>,
    /// Visible trail, tail → head (rebuilt every step).
    pub pts: Vec<Vec2>,
}

fn in_bounds(p: Vec2, margin: f32) -> bool {
    p.x.abs() < ARENA_X - margin && p.y > ARENA_Z0 + margin && p.y < ARENA_Z1 - margin
}

impl Cycle {
    fn spawn(team: Team, id: usize, rng: &mut Rng) -> Self {
        let pos = Vec2::new(
            (rng.range(-ARENA_X + 10.0, ARENA_X - 10.0) / CELL).round() * CELL,
            (rng.range(ARENA_Z0 + 12.0, ARENA_Z1 - 12.0) / CELL).round() * CELL,
        );
        let mut dir = (rng.f() * 4.0) as usize % 4;
        while !in_bounds(pos + DIRS[dir] * 14.0, 0.0) {
            dir = (dir + 1) % 4;
        }
        Cycle {
            seeded: false,
            id,
            team,
            pos,
            dir,
            speed: rng.range(13.0, 19.0),
            alive: true,
            respawn: 0.0,
            age: 0.0,
            spawn_flash: 1.0,
            straight: 0.0,
            force_turn: false,
            prefer: None,
            danger_prev: false,
            react: true,
            corners: vec![pos],
            pts: vec![pos, pos],
        }
    }

    /// A cycle born from one of the logo's lines: the line is its first trail, the head sits at the line's end.
    fn seeded(team: Team, id: usize, line: &SeedLine, rng: &mut Rng) -> Self {
        let mut c = Cycle::spawn(team, id, rng);
        c.seeded = true;
        c.pos = line.head;
        c.dir = line.dir;
        c.corners = line.corners.clone();
        // the logo's lines sit only ~1.5 units apart: run straight well clear of the neighbours before the first turn
        c.straight = -30.0;
        c.spawn_flash = 0.0;
        c.rebuild_trail();
        c
    }

    fn step(&mut self, dt: f32, rng: &mut Rng) {
        self.age += dt;
        self.spawn_flash = (self.spawn_flash - dt * 1.4).max(0.0);
        let d = DIRS[self.dir];
        let prev = self.pos;
        let travel = self.speed * dt;
        self.pos += d * travel;
        self.straight += travel;

        // Grid-line crossing → possible 90° turn, snapped exactly onto the line.
        let (a, b) = if d.x != 0.0 { (prev.x, self.pos.x) } else { (prev.y, self.pos.y) };
        let (ka, kb) = ((a / CELL).floor(), (b / CELL).floor());
        if ka != kb {
            let must = !in_bounds(self.pos + d * 9.0, 0.0);
            if must || self.force_turn || (self.straight > 14.0 && rng.chance(0.32)) {
                self.force_turn = false;
                let line = ka.max(kb) * CELL;
                let left = (b - line).abs();
                let cands: Vec<usize> = [(self.dir + 1) % 4, (self.dir + 3) % 4]
                    .into_iter()
                    .filter(|&c| in_bounds(self.pos + DIRS[c] * 10.0, 0.0))
                    .collect();
                let nd = match self.prefer.take() {
                    Some(c) if cands.contains(&c) => c,
                    _ if cands.is_empty() => (self.dir + 1) % 4,
                    _ => cands[(rng.f() * cands.len() as f32) as usize % cands.len()],
                };
                let corner = if d.x != 0.0 { Vec2::new(line, self.pos.y) } else { Vec2::new(self.pos.x, line) };
                self.corners.push(corner);
                self.dir = nd;
                self.pos = corner + DIRS[nd] * left;
                self.straight = 0.0;
            }
        }
        self.rebuild_trail();
    }

    fn rebuild_trail(&mut self) {
        self.pts.clear();
        self.pts.push(self.pos);
        let mut rem = TRAIL_LEN;
        for i in (0..self.corners.len()).rev() {
            let c = self.corners[i];
            let last = *self.pts.last().unwrap();
            let seg = (c - last).length();
            if seg >= rem {
                let dir = (c - last) / seg.max(1e-5);
                self.pts.push(last + dir * rem);
                self.corners.drain(0..i);
                break;
            }
            rem -= seg;
            self.pts.push(c);
        }
        self.pts.reverse();
    }
}

fn seg_dist(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    let l2 = ab.length_squared();
    let t = if l2 < 1e-6 { 0.0 } else { ((p - a).dot(ab) / l2).clamp(0.0, 1.0) };
    (p - (a + ab * t)).length()
}

pub struct Shard {
    pub p: Vec3,
    v: Vec3,
    pub axis: Vec3,
    pub ang: f32,
    w: f32,
    pub life: f32,
    pub max: f32,
    pub color: Vec3,
    pub size: f32,
}

pub struct Pulse {
    pub o: Vec2,
    pub r: f32,
    pub strength: f32,
    pub color: Vec3,
}

/// A sensor node on the floor: the source/sink of data pulses.
pub struct Tower {
    pub x: f32,
    pub z: f32,
}

/// Sensor nodes scattered over the floor of the district.
fn gen_nodes(rng: &mut Rng) -> Vec<Tower> {
    (0..26)
        .map(|i| {
            let side = if i % 2 == 0 { 1.0 } else { -1.0 };
            Tower { x: side * rng.range(14.0, 135.0), z: rng.range(26.0, 215.0) }
        })
        .collect()
}

/// Shot length in recording mode.
const SHOT_HOLD: f32 = 12.0;

#[derive(Clone, Copy, PartialEq)]
pub enum Scenario {
    Emerge,
    Arena,
    Run,
    Twin,
}
impl Scenario {
    pub fn duration(self) -> f32 {
        match self {
            Scenario::Emerge => 24.0,
            Scenario::Arena => 48.0,
            Scenario::Twin => 42.0,
            Scenario::Run => 8.5,
        }
    }
}

/// Data pulse running along a right-angled circuit trace on the grid:
/// building base → along z → along x → up the sync column into the hub.
pub struct Packet {
    pts: [Vec3; 4],
    seg: [f32; 3],
    total: f32,
    /// Distance travelled by the head along the trace.
    pub d: f32,
    speed: f32,
    /// true: sensor data inbound to the twin; false: sync command outbound.
    pub up: bool,
}
impl Packet {
    pub const LEN: f32 = 7.0;
    pub fn pos(&self, d: f32) -> Vec3 {
        let mut d = d.clamp(0.0, self.total);
        for i in 0..3 {
            if d <= self.seg[i] {
                return self.pts[i].lerp(self.pts[i + 1], d / self.seg[i].max(1e-4));
            }
            d -= self.seg[i];
        }
        self.pts[3]
    }
    pub fn total(&self) -> f32 {
        self.total
    }
    pub fn fade(&self) -> f32 {
        (self.d / 6.0).min(1.0) * ((self.total - self.d) / 6.0 + 0.4).clamp(0.0, 1.0)
    }
}

/// Circuit trace for a building (shared by static trace drawing and packets).
pub fn trace(tw: &Tower) -> [Vec3; 4] {
    [
        Vec3::new(tw.x * 0.85, 0.08, tw.z),
        Vec3::new(tw.x * 0.85, 0.08, MARK_Z),
        Vec3::new(0.0, 0.08, MARK_Z),
        Vec3::new(0.0, MARK_Y - 13.0, MARK_Z),
    ]
}

/// A falling line standing for one real Omarchy repo event.
pub struct Drop {
    pub x: f32,
    pub z: f32,
    pub y: f32,
    vy: f32,
    pub len: f32,
    pub kind: Kind,
    pub weight: f32,
    pub label: String,
    pub who: String,
    age_s: i64,
}

/// A light trail drawn by one landed repo event: it rides a planned grid path whose shape and pace depend on the event type.
pub struct Streak {
    path: Vec<Vec2>,
    s: f32,
    v: f32,
    accel: f32,
    vmax: f32,
    delay: f32,
    tail: f32,
    pub kind: Kind,
    /// wall height as a fraction of a light cycle's
    pub height: f32,
    /// set on the lead trail of an event: whose it is
    pub who: Option<String>,
    /// visible trail, tail → head
    pub pts: Vec<Vec2>,
    /// 1 while the head is travelling, falls to 0 as the trail drains into its end
    pub live: f32,
}

fn path_len(p: &[Vec2]) -> f32 {
    p.windows(2).map(|w| (w[1] - w[0]).length()).sum()
}

/// The stretch of a polyline between distances d0 and d1 along it.
fn path_cut(p: &[Vec2], d0: f32, d1: f32) -> Vec<Vec2> {
    let mut out = Vec::new();
    let mut acc = 0.0;
    for w in p.windows(2) {
        let l = (w[1] - w[0]).length();
        if l > 1e-5 && acc + l >= d0 && acc <= d1 {
            let at = |d: f32| w[0] + (w[1] - w[0]) * ((d - acc) / l).clamp(0.0, 1.0);
            if out.is_empty() {
                out.push(at(d0));
            }
            out.push(at(d1));
        }
        acc += l;
    }
    out
}

impl Streak {
    fn new(path: Vec<Vec2>, kind: Kind, v: f32, accel: f32, vmax: f32, tail: f32, height: f32) -> Self {
        Streak { path, s: 0.0, v, accel, vmax, delay: 0.0, tail, kind, height, who: None, pts: Vec::new(), live: 1.0 }
    }

    fn step(&mut self, dt: f32) -> bool {
        if self.delay > 0.0 {
            self.delay -= dt;
            return true;
        }
        let total = path_len(&self.path);
        self.v = (self.v + self.accel * dt).clamp(5.0, self.vmax);
        // once the head is home the trail drains into the end point
        let v = if self.s >= total { self.v.max(14.0) } else { self.v };
        self.s += v * dt;
        let tail = (self.s - self.tail).max(0.0);
        if tail >= total {
            return false;
        }
        self.pts = path_cut(&self.path, tail, self.s.min(total));
        self.live = if self.s < total { 1.0 } else { (1.0 - (self.s - total) / self.tail).max(0.0) };
        true
    }
}

/// One landed event, as listed in the HUD feed.
pub struct LogLine {
    pub who: String,
    pub what: String,
    pub kind: Kind,
    pub age: String,
    /// seconds since it landed
    pub life: f32,
}

pub struct Sim {
    net: Option<Receiver<Msg>>,
    queue: VecDeque<Ev>,
    history: Vec<Ev>,
    seen: HashSet<String>,
    pub drops: Vec<Drop>,
    /// light trails drawn by landed events
    pub streaks: Vec<Streak>,
    drop_timer: f32,
    pub stars: u32,
    /// None until the first poll answers; Some(false) = replaying cached events
    pub online: Option<bool>,
    /// most recent landed events first: who did what, shown in the HUD
    pub ev_log: VecDeque<LogLine>,
    /// recording mode: stay in the arena and cycle through all its camera angles
    pub hold_arena: bool,
    pub towers: Vec<Tower>,
    pub packets: Vec<Packet>,
    pub scenario: Scenario,
    pub scen_t: f32,
    run_to: Scenario,
    pub sector: u32,
    /// Transition flash 0..1 (ramps up before a scene change, decays after).
    pub flash: f32,
    flash_after: f32,
    pub cam_eye: Vec3,
    pub cam_target: Vec3,
    pub cam_up: Vec3,
    pub aspect: f32,
    /// the battle began straight out of the emergence: its first trails are the logo's own lines
    pub seam_arena: bool,
    seed_lines: Vec<SeedLine>,
    /// polylines of the planned seed lines (read by the renderer for the build-up before the handover)
    pub seed_paths: Vec<Vec<Vec2>>,
    /// which of the logo's 16 edges (3 facets × 4, sliver last) are the ones that turn into trails this time
    pub seed_sel: Vec<bool>,
    cam_ve: Vec3,
    cam_vt: Vec3,
    /// 0 = the desktop background's graphite/amber palette, 1 = full Tron blue + red
    pub tron: f32,
    snap_cam: bool,
    pub kills: [u32; 2],
    pub feed: Vec<(String, f32, Team)>,
    pub run_speed: f32,
    run_dist: f32,
    /// X of the sweeping scan front: the world at x < scan_x (screen-right) is digital, the rest physical.
    pub scan_x: f32,
    scan_dir: f32,
    pkt_timer: f32,
    pub cycles: Vec<Cycle>,
    pub shards: Vec<Shard>,
    pub pulses: Vec<Pulse>,
    /// Monolith vertical spring-damper state.
    pub mono_y: f32,
    mono_vy: f32,
    /// Light-channel energy (0..~2) kicked up by detonations, decays.
    pub energy: f32,
    pub rng: Rng,
    pub time: f32,
    since_derez: f32,
}

impl Sim {
    pub fn new(seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let cycles = (0..6)
            .map(|i| {
                let mut c = Cycle::spawn(if i % 2 == 0 { Team::Red } else { Team::Cyan }, i, &mut rng);
                c.alive = false;
                c.respawn = 1e9;
                c
            })
            .collect();
        let towers = gen_nodes(&mut rng);
        let (seed_lines, seed_sel) = plan_seeds(&mut rng);
        Sim {
            net: if std::env::args().any(|a| a == "--no-feed") { None } else { Some(feed::spawn()) },
            queue: VecDeque::new(),
            history: Vec::new(),
            seen: HashSet::new(),
            drops: Vec::new(),
            streaks: Vec::new(),
            drop_timer: 2.0,
            stars: 0,
            online: None,
            ev_log: VecDeque::new(),
            hold_arena: false,
            towers,
            packets: Vec::new(),
            scenario: Scenario::Emerge,
            scen_t: 0.0,
            run_to: Scenario::Twin,
            sector: 1,
            flash: 0.0,
            flash_after: 0.0,
            cam_eye: Vec3::new(0.0, 5.0, 0.0),
            cam_target: Vec3::new(0.0, 2.0, 40.0),
            cam_up: Vec3::Y,
            aspect: 1.6,
            seam_arena: false,
            seed_paths: paths_of(&seed_lines),
            seed_lines,
            seed_sel,
            cam_ve: Vec3::ZERO,
            cam_vt: Vec3::ZERO,
            tron: 0.0,
            snap_cam: true,
            kills: [0, 0],
            feed: Vec::new(),
            run_speed: 0.0,
            run_dist: 0.0,
            scan_x: 400.0,
            scan_dir: 1.0,
            pkt_timer: 0.0,
            cycles,
            shards: Vec::with_capacity(MAX_SHARDS),
            pulses: Vec::new(),
            mono_y: 0.0,
            mono_vy: 0.0,
            energy: 0.2,
            rng,
            time: 0.0,
            since_derez: 0.0,
        }
    }

    /// Jump straight to a scenario (used for previews / testing).
    pub fn jump(&mut self, to: Scenario) {
        if to == Scenario::Emerge {
            self.scenario = Scenario::Twin;
            self.run_to = Scenario::Emerge;
            self.advance();
            self.advance();
            self.flash_after = 0.0;
            return;
        }
        if to == Scenario::Run {
            self.scenario = Scenario::Arena;
            self.run_to = Scenario::Twin;
        } else {
            self.scenario = Scenario::Run;
            self.run_to = to;
        }
        self.advance();
        self.flash_after = 0.0;
    }

    pub fn alive(&self) -> usize {
        self.cycles.iter().filter(|c| c.alive).count()
    }

    fn derez(&mut self, idx: usize, killer: Option<usize>) {
        let victim_id = self.cycles[idx].id;
        let (team, head, pts) = {
            let c = &mut self.cycles[idx];
            c.alive = false;
            c.respawn = self.rng.range(2.5, 5.0);
            (c.team, c.pos, c.pts.clone())
        };
        let col = team.color();
        let h3 = Vec3::new(head.x, 0.6, head.y);

        // Head burst.
        for _ in 0..34 {
            let dir = self.rng.unit();
            let sp = self.rng.range(3.0, 14.0);
            let v = Vec3::new(dir.x * sp, dir.y.abs() * sp * 0.9 + 3.0, dir.z * sp);
            self.push_shard(h3, v, col, 0.32);
        }
        // Trail dissolves upward into voxels.
        let mut carry = 0.0;
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let l = (b - a).length();
            let mut s = carry;
            while s < l {
                let p = a + (b - a) * (s / l.max(1e-4));
                let v = Vec3::new(self.rng.range(-1.2, 1.2), self.rng.range(2.0, 9.0), self.rng.range(-1.2, 1.2));
                let y = self.rng.range(0.1, 1.1);
                self.push_shard(Vec3::new(p.x, y, p.y), v, col, 0.26);
                s += 2.0;
            }
            carry = s - l;
        }
        match killer {
            Some(k) if k != idx => {
                let kt = self.cycles[k].team;
                self.kills[if kt == Team::Red { 0 } else { 1 }] += 1;
                self.feed.push((format!("UNIT-{:02} DEREZZED UNIT-{:02}", self.cycles[k].id + 1, victim_id + 1), 7.0, kt));
            }
            _ => self.feed.push((format!("UNIT-{:02} CRASHED", victim_id + 1), 7.0, team)),
        }
        if self.feed.len() > 5 {
            self.feed.remove(0);
        }
        self.pulses.push(Pulse { o: head, r: 0.0, strength: 1.0, color: col });
        let dist = (head - Vec2::new(0.0, 70.0)).length();
        let kick = 1.0 / (1.0 + dist / 30.0);
        self.mono_vy += 5.5 * kick + 1.5;
        self.energy = (self.energy + 0.7 * kick + 0.25).min(2.0);
        self.since_derez = 0.0;
    }

    fn push_shard(&mut self, p: Vec3, v: Vec3, color: Vec3, size: f32) {
        if self.shards.len() >= MAX_SHARDS {
            return;
        }
        let white = self.rng.chance(0.07);
        let max = self.rng.range(1.8, 4.2);
        let axis = self.rng.unit();
        let w = self.rng.range(-9.0, 9.0);
        let sz = size * self.rng.range(0.45, 1.0);
        self.shards.push(Shard {
            p,
            v,
            axis,
            ang: 0.0,
            w,
            life: max,
            max,
            color: if white { Vec3::new(0.95, 0.97, 1.0) } else { color },
            size: sz,
        });
    }

    fn advance(&mut self) {
        let prev = self.scenario;
        let next = match prev {
            Scenario::Emerge => Scenario::Arena,
            Scenario::Arena => {
                self.run_to = Scenario::Twin;
                Scenario::Run
            }
            Scenario::Twin => {
                self.run_to = Scenario::Emerge;
                Scenario::Run
            }
            Scenario::Run => self.run_to,
        };
        // the background → 3D emergence flows straight into the battle: no flash, camera keeps going
        let seamless = prev == Scenario::Emerge && next == Scenario::Arena;
        self.seam_arena = false;
        self.scenario = next;
        self.scen_t = 0.0;
        self.flash_after = if seamless { 0.0 } else { 1.0 };
        self.shards.clear();
        self.pulses.clear();
        self.packets.clear();
        self.drops.clear();
        self.streaks.clear();
        self.snap_cam = !seamless;
        match next {
            Scenario::Run => {
                self.towers = Vec::new();
                self.scan_x = 400.0;
                self.run_dist = 0.0;
            }
            Scenario::Emerge => {
                let (l, sel) = plan_seeds(&mut self.rng);
                self.seed_paths = paths_of(&l);
                self.seed_lines = l;
                self.seed_sel = sel;
                self.sector += 1;
                self.towers = gen_nodes(&mut self.rng);
                self.scan_x = 400.0;
                self.energy = 0.2;
                self.mono_y = 0.0;
                self.mono_vy = 0.0;
                self.kills = [0, 0];
                self.feed.clear();
                for c in &mut self.cycles {
                    c.alive = false;
                    c.respawn = 1e9;
                }
            }
            Scenario::Arena | Scenario::Twin => {
                if !seamless {
                    self.sector += 1;
                }
                self.towers = gen_nodes(&mut self.rng);
                if seamless {
                    self.scan_x = SCAN_MAX;
                    self.scan_dir = -1.0;
                } else {
                    self.scan_x = SCAN_MIN;
                    self.scan_dir = 1.0;
                }
                self.energy = 0.2;
                self.mono_y = 0.0;
                self.mono_vy = 0.0;
                self.seam_arena = seamless;
                for i in 0..self.cycles.len() {
                    let team = self.cycles[i].team;
                    self.cycles[i] = if seamless && i < self.seed_lines.len() {
                        let line = std::mem::replace(&mut self.seed_lines[i], SeedLine { corners: Vec::new(), head: Vec2::ZERO, dir: 0 });
                        Cycle::seeded(team, i, &line, &mut self.rng)
                    } else {
                        Cycle::spawn(team, i, &mut self.rng)
                    };
                    if next == Scenario::Twin {
                        self.cycles[i].alive = false;
                        self.cycles[i].respawn = 1e9;
                    }
                }
                self.kills = [0, 0];
                self.feed.clear();
            }
        }
    }

    fn update_arena(&mut self, dt: f32) {
        // Look-ahead avoidance: usually react to a wall ahead by turning at the next grid line, sometimes don't.
        for i in 0..self.cycles.len() {
            if !self.cycles[i].alive {
                continue;
            }
            let (pos, dir) = (self.cycles[i].pos, self.cycles[i].dir);
            let mut danger = false;
            'look: for k in [3.0f32, 6.0, 9.0] {
                let probe = pos + DIRS[dir] * k;
                for (j, b) in self.cycles.iter().enumerate() {
                    if !b.alive {
                        continue;
                    }
                    let n = b.pts.len();
                    let skip = if i == j { 2 } else { 0 };
                    if n < 2 + skip {
                        continue;
                    }
                    for sgm in 0..(n - 1 - skip) {
                        if seg_dist(probe, b.pts[sgm], b.pts[sgm + 1]) < 1.4 {
                            danger = true;
                            break 'look;
                        }
                    }
                }
            }
            if danger && !self.cycles[i].danger_prev {
                self.cycles[i].react = self.rng.chance(0.82);
            }
            self.cycles[i].danger_prev = danger;
            if danger && self.cycles[i].react {
                self.cycles[i].force_turn = true;
                // pick the side with more free road beyond the next line
                let mut best = (self.cycles[i].dir + 1) % 4;
                let mut best_score = -1i32;
                for c in [(dir + 1) % 4, (dir + 3) % 4] {
                    let mut score = 0;
                    for k in [8.0f32, 14.0, 20.0, 28.0] {
                        let q = pos + DIRS[dir] * 4.0 + DIRS[c] * k;
                        let mut free = in_bounds(q, 2.0);
                        if free {
                            'chk: for b in self.cycles.iter().filter(|b| b.alive) {
                                for w in b.pts.windows(2) {
                                    if seg_dist(q, w[0], w[1]) < 1.8 {
                                        free = false;
                                        break 'chk;
                                    }
                                }
                            }
                        }
                        if free {
                            score += 1;
                        } else {
                            break;
                        }
                    }
                    if score > best_score {
                        best_score = score;
                        best = c;
                    }
                }
                self.cycles[i].prefer = Some(best);
            }
        }
        for i in 0..self.cycles.len() {
            if self.cycles[i].alive {
                let mut rng = std::mem::replace(&mut self.rng, Rng(1));
                self.cycles[i].step(dt, &mut rng);
                self.rng = rng;
            } else {
                self.cycles[i].respawn -= dt;
                if self.cycles[i].respawn <= 0.0 {
                    let team = self.cycles[i].team;
                    self.cycles[i] = Cycle::spawn(team, i, &mut self.rng);
                    let p = self.cycles[i].pos;
                    self.pulses.push(Pulse { o: p, r: 0.0, strength: 0.45, color: team.color() });
                }
            }
        }

        // Collisions: head against every visible trail wall; the wall's owner scores the derez.
        let mut dead: Vec<(usize, Option<usize>)> = Vec::new();
        for i in 0..self.cycles.len() {
            let a = &self.cycles[i];
            if !a.alive || (a.seeded && a.age < 1.6) {
                continue; // freshly born from the logo: safe while it clears the other lines
            }
            'outer: for (j, b) in self.cycles.iter().enumerate() {
                if !b.alive || (b.seeded && b.age < 1.6 && i != j) {
                    continue;
                }
                let n = b.pts.len();
                let skip = if i == j { 3 } else { 0 };
                if n < 2 + skip {
                    continue;
                }
                for s in 0..(n - 1 - skip) {
                    if seg_dist(a.pos, b.pts[s], b.pts[s + 1]) < 0.6 {
                        dead.push((i, if i == j { None } else { Some(j) }));
                        break 'outer;
                    }
                }
            }
        }
        // Keep the match lively if nothing has collided for a while.
        if dead.is_empty() && self.since_derez > 10.0 {
            let cand: Vec<usize> =
                (0..self.cycles.len()).filter(|&i| self.cycles[i].alive && self.cycles[i].age > 6.0).collect();
            if !cand.is_empty() {
                dead.push((cand[(self.rng.f() * cand.len() as f32) as usize % cand.len()], None));
            }
        }
        for (i, k) in dead {
            if self.cycles[i].alive {
                self.derez(i, k);
            }
        }
    }

    /// Camera pose of the calm Arena overview (also the end pose of the emergence).
    fn arena_pose(t: f32) -> (Vec3, Vec3) {
        let zc = 0.5 * (ARENA_Z0 + ARENA_Z1);
        let a = 0.45 * (t * 0.04).sin();
        (
            Vec3::new(62.0 * a.sin(), 25.0 + 2.0 * (t * 0.07).sin(), zc - 62.0 * a.cos()),
            Vec3::new(0.0, 0.0, zc + 6.0),
        )
    }

    /// 0..1 progress through the emergence (1 outside it).
    pub fn emerge_prog(&self) -> f32 {
        if self.scenario == Scenario::Emerge {
            (self.scen_t / self.scenario.duration()).clamp(0.0, 1.0)
        } else {
            1.0
        }
    }

    fn update_feed(&mut self, dt: f32) {
        if let Some(rx) = &self.net {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Events(mut evs) => {
                        evs.sort_by_key(|e| e.t); // oldest first
                        for e in evs {
                            if self.seen.insert(e.id.clone()) {
                                self.history.push(e.clone());
                                if self.history.len() > 60 {
                                    self.history.remove(0);
                                }
                                if self.queue.len() < 60 {
                                    self.queue.push_back(e);
                                }
                            }
                        }
                    }
                    Msg::Stars(n) => self.stars = n,
                    Msg::Online(o) => self.online = Some(o),
                }
            }
        }
        for l in &mut self.ev_log {
            l.life += dt;
        }
        // advance falling lines; each lands as a floor ripple
        let mut landed: Vec<(f32, f32, Kind, f32, String, String, i64)> = Vec::new();
        for d in &mut self.drops {
            d.y -= d.vy * dt;
        }
        self.drops.retain(|d| {
            if d.y <= 0.0 {
                landed.push((d.x, d.z, d.kind, d.weight, d.label.clone(), d.who.clone(), d.age_s));
                false
            } else {
                true
            }
        });
        self.streaks.retain_mut(|s| s.step(dt));
        for (x, z, kind, w, label, who, age_s) in landed {
            let col = kind.color();
            self.pulses.push(Pulse { o: Vec2::new(x, z), r: 0.0, strength: 0.18 + 0.5 * w, color: col });
            self.spawn_streaks(Vec2::new(x, z), kind, w, &who);
            if matches!(kind, Kind::Push | Kind::Merge) {
                for _ in 0..(6.0 + 10.0 * w) as usize {
                    let a = self.rng.range(0.0, std::f32::consts::TAU);
                    let sp = self.rng.range(2.0, 6.0);
                    let up = self.rng.range(3.0, 7.0);
                    self.push_shard(Vec3::new(x, 0.3, z), Vec3::new(a.cos() * sp, up, a.sin() * sp), col, 0.22);
                }
            }
            let age = if age_s < 0 {
                "SIMULATED".to_string()
            } else if age_s < 90 {
                "JUST NOW".to_string()
            } else if age_s < 5400 {
                format!("{} MIN AGO", age_s / 60)
            } else {
                format!("{} H AGO", age_s / 3600)
            };
            self.ev_log.push_front(LogLine { who, what: label, kind, age, life: 0.0 });
            self.ev_log.truncate(6);
        }
        if !matches!(self.scenario, Scenario::Arena | Scenario::Twin) {
            return;
        }
        // release the next event: queued new ones first (quickly if a burst arrived), then a slow replay of recent history
        self.drop_timer -= dt;
        if self.drop_timer <= 0.0 && self.drops.len() < 18 {
            let (ev, replay) = match self.queue.pop_front() {
                Some(e) => (Some(e), false),
                None if !self.history.is_empty() => {
                    let i = (self.rng.f() * self.history.len() as f32) as usize % self.history.len();
                    (Some(self.history[i].clone()), true)
                }
                None => (None, true),
            };
            self.drop_timer = if self.queue.len() > 8 { 0.5 } else if replay { self.rng.range(2.2, 4.5) } else { self.rng.range(1.0, 2.2) };
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            let ev = ev.unwrap_or_else(|| {
                // no data at all yet: a quiet simulated rain
                let k = [Kind::Comment, Kind::Comment, Kind::Push, Kind::Star][(self.rng.f() * 4.0) as usize % 4];
                Ev { id: String::new(), kind: k, weight: 0.4, label: "SIGNAL".into(), who: "grid".into(), t: -1 }
            });
            let len = 3.5 + 11.0 * ev.weight;
            self.drops.push(Drop {
                // land inside the arena so the event's trail plays out among the cycles
                x: self.rng.range(-56.0, 56.0),
                z: self.rng.range(14.0, 128.0),
                y: 70.0 + len,
                vy: 26.0 + 22.0 * ev.weight + self.rng.range(0.0, 8.0),
                len,
                kind: ev.kind,
                weight: ev.weight,
                label: ev.label,
                who: ev.who,
                age_s: if ev.t < 0 { -1 } else { (now - ev.t).max(0) },
            });
        }
    }

    /// Light trails for a landed event, one pattern per type: a star bursts four ways and brakes, a fork splits,
    /// a push sprints (one lane per few commits), a PR climbs a staircase, a merge brings two lanes into one,
    /// an issue spirals in on itself. Comments stay ripples.
    fn spawn_streaks(&mut self, at: Vec2, kind: Kind, w: f32, who: &str) {
        if self.streaks.len() > 48 {
            return;
        }
        let snap = |v: f32| (v / CELL).round() * CELL;
        let o = Vec2::new(snap(at.x), snap(at.y));
        let d = (self.rng.f() * 4.0) as usize % 4;
        let (f, side) = (DIRS[d], DIRS[(d + 1) % 4]);
        let mut out: Vec<Streak> = Vec::new();
        match kind {
            Kind::Star => {
                for k in 0..4 {
                    let len = snap(self.rng.range(20.0, 32.0) + 12.0 * w);
                    out.push(Streak::new(vec![o, o + DIRS[k] * len], kind, 60.0, -52.0, 60.0, len, 0.75));
                }
            }
            Kind::Fork => {
                let (trunk, arm, run) = (snap(self.rng.range(12.0, 20.0)), 8.0, snap(self.rng.range(24.0, 36.0) + 10.0 * w));
                let fp = o + f * trunk;
                let v = 22.0;
                out.push(Streak::new(vec![o, fp, fp + side * arm, fp + side * arm + f * run], kind, v, 0.0, v, 36.0, 0.9));
                let mut b = Streak::new(vec![fp, fp - side * arm, fp - side * arm + f * run], kind, v, 0.0, v, 36.0, 0.9);
                b.delay = trunk / v;
                out.push(b);
            }
            Kind::Push => {
                let lanes = 1 + ((w * 3.0) as usize).min(3);
                let len = snap(self.rng.range(60.0, 90.0));
                for i in 0..lanes {
                    let s0 = o + side * (i as f32 - (lanes - 1) as f32 * 0.5) * CELL * 0.5;
                    let mut s = Streak::new(vec![s0, s0 + f * len], kind, 16.0, 70.0, 100.0, 22.0, 0.75);
                    s.delay = i as f32 * 0.12;
                    out.push(s);
                }
            }
            Kind::Pr => {
                let steps = 6 + (w * 4.0) as usize;
                let mut path = vec![o];
                let mut p = o;
                for i in 0..steps {
                    p += if i % 2 == 0 { f } else { side } * CELL * 2.0;
                    path.push(p);
                }
                out.push(Streak::new(path, kind, 16.0, 0.0, 16.0, 40.0, 0.95));
            }
            Kind::Merge => {
                let (gap, lead_in, run) = (CELL * 2.0, snap(self.rng.range(10.0, 16.0)), snap(self.rng.range(26.0, 40.0) + 10.0 * w));
                for sg in [-1.0f32, 1.0] {
                    let lane = o + side * gap * sg;
                    out.push(Streak::new(vec![lane - f * lead_in, lane, o, o + f * run], kind, 18.0, 0.0, 18.0, 40.0, 0.9));
                }
            }
            Kind::Issue => {
                // square spiral that ends on the landing point
                let lens = [6.0f32, 6.0, 5.0, 5.0, 4.0, 4.0, 3.0, 3.0, 2.0, 2.0, 1.0].map(|l| l * CELL);
                let mut path = vec![Vec2::ZERO];
                let mut p = Vec2::ZERO;
                for (i, l) in lens.iter().enumerate() {
                    p += DIRS[(d + i) % 4] * *l;
                    path.push(p);
                }
                let end = p;
                out.push(Streak::new(path.into_iter().map(|q| o + q - end).collect(), kind, 14.0, 0.0, 14.0, 56.0, 0.85));
            }
            Kind::Comment | Kind::Other => {}
        }
        if let Some(first) = out.first_mut() {
            first.who = Some(who.to_string());
        }
        self.streaks.extend(out);
    }

    /// Arena camera director: overview → slow orbit → lateral sweep → high view, then a dive toward the logo.
    fn arena_shot(scen_t: f32, t: f32, hold: bool) -> (Vec3, Vec3) {
        let dur = Scenario::Arena.duration();
        let zc = 0.5 * (ARENA_Z0 + ARENA_Z1);
        let ss = |x: f32| {
            let x = x.clamp(0.0, 1.0);
            x * x * (3.0 - 2.0 * x)
        };
        if !hold && scen_t > dur - 3.2 {
            return (Vec3::new(0.0, 9.0, EMERGE_ZB - 22.0), Vec3::new(0.0, 15.0, EMERGE_ZB));
        }
        // recording mode: every shot is reached, in turn, for as long as the arena is held
        let seg = if hold { SHOT_HOLD } else { 13.0 };
        let (shot, u) = (((scen_t / seg) as usize) % 4, (scen_t % seg) / seg);
        match shot {
            0 => Self::arena_pose(t),
            1 => {
                let a = -0.75 + 1.5 * ss(u);
                (Vec3::new(82.0 * a.sin(), 13.0, zc - 82.0 * a.cos()), Vec3::new(0.0, 4.0, zc + 4.0))
            }
            2 => (Vec3::new(-62.0 + 124.0 * ss(u), 9.0, 20.0), Vec3::new(30.0 - 60.0 * ss(u), 3.0, 72.0)),
            _ => (Vec3::new(18.0 * (0.25 * t).sin(), 92.0, zc - 34.0), Vec3::new(0.0, 0.0, zc + 6.0)),
        }
    }

    /// Twin Sync camera shots.
    fn twin_shot(scen_t: f32, t: f32) -> (Vec3, Vec3) {
        match ((scen_t / 14.0) as usize) % 4 {
            0 => (
                Vec3::new(4.5 * (t * 0.07).sin(), 3.6 + 0.4 * (t * 0.11).sin(), -8.0),
                Vec3::new(1.5 * (t * 0.05 + 1.0).sin(), 11.5, 70.0),
            ),
            1 => (Vec3::new(38.0, 4.0, 8.0), Vec3::new(0.0, 13.0, 70.0)),
            2 => (Vec3::new(-42.0, 24.0, 4.0), Vec3::new(0.0, 12.0, 70.0)),
            _ => (Vec3::new(0.0, 11.0, -62.0), Vec3::new(0.0, 12.0, 70.0)),
        }
    }

    /// The opening's first camera pose (eye, target, up): straight down on the flat desktop, screen-up = +z.
    pub fn emerge_start_cam(&self) -> (Vec3, Vec3, Vec3) {
        let ofs = brand::get().mark_offset * EMERGE_H * 0.4831 * self.aspect;
        let zc = FLAT_ZB + EMERGE_MARK_W * brand::get().logo.mid_h() + 1.1;
        (Vec3::new(ofs, EMERGE_H, zc), Vec3::new(ofs, 0.0, zc), Vec3::Z)
    }

    fn update_camera(&mut self, dt: f32) {
        let t = self.time;
        self.cam_up = Vec3::Y;
        let (de, dtg, k) = match self.scenario {
            Scenario::Emerge => {
                // one continuous arc: eye follows a Bezier, pitch/yaw ease from straight-down to the arena view,
                // so the flat background tilts away like a parallax reveal rather than being cut to
                let prog = self.emerge_prog();
                let smoother = |x: f32| {
                    let x = x.clamp(0.0, 1.0);
                    x * x * x * (x * (x * 6.0 - 15.0) + 10.0)
                };
                let s = smoother((prog - 0.10) / 0.90);
                let ofs = brand::get().mark_offset * EMERGE_H * 0.4831 * self.aspect;
                let half = EMERGE_MARK_W * brand::get().logo.mid_h();
                let zc = FLAT_ZB + half + 1.1;
                let e0 = Vec3::new(ofs, EMERGE_H, zc);
                let (e1, _) = Self::arena_pose(t);
                let c = Vec3::new(e0.x * 0.15 - 16.0, 68.0, 0.5 * (e0.z + e1.z) + 8.0);
                let u = 1.0 - s;
                let eye = e0 * (u * u) + c * (2.0 * u * s) + e1 * (s * s);
                // look-at: the flat view's centre at first, then the centre of the logo as it folds up and stands
                let fold = std::f32::consts::FRAC_PI_2 * smoother((prog - 0.30) / 0.685);
                let logo = Vec3::new(0.0, half * fold.sin() + 0.04, rise_zb(fold) + half * fold.cos());
                let t0 = Vec3::new(ofs, 0.0, zc);
                let (_, arena_target) = Self::arena_pose(t);
                let target = t0.lerp(logo, smoother((prog - 0.10) / 0.30)).lerp(arena_target, smoother((prog - 0.68) / 0.32));
                let f = (target - eye).normalize();
                let phi = (-f.y).asin();
                // roll: hold the flat layout's orientation (screen-up = +z) while looking steeply down, where the
                // true heading is ill-defined, and blend to the real heading as the camera pitches away
                let w = ((75.0f32.to_radians() - phi) / 25.0f32.to_radians()).clamp(0.0, 1.0);
                let w = w * w * (3.0 - 2.0 * w);
                let psi = f.x.atan2(f.z) * w;
                self.cam_up = Vec3::new(phi.sin() * psi.sin(), phi.cos(), phi.sin() * psi.cos());
                (eye, target, 80.0)
            }
            Scenario::Twin => {
                let (e, tg) = Self::twin_shot(self.scen_t, t);
                (e, tg, -1.0)
            }
            Scenario::Run => {
                let ramp = (self.scen_t / 3.0).clamp(0.0, 1.0);
                self.run_speed = 28.0 + 62.0 * ramp * ramp * (3.0 - 2.0 * ramp);
                self.run_dist += self.run_speed * dt;
                let e = Vec3::new(1.2 * (self.scen_t * 0.8).sin(), 4.2, -8.0 + self.run_dist);
                (e, Vec3::new(0.0, 7.0, e.z + 60.0), 30.0)
            }
            Scenario::Arena => {
                let (e, tg) = Self::arena_shot(self.scen_t, t, self.hold_arena);
                (e, tg, -1.0)
            }
        };
        if self.snap_cam {
            self.cam_eye = de;
            self.cam_target = dtg;
            self.cam_ve = Vec3::ZERO;
            self.cam_vt = Vec3::ZERO;
            self.snap_cam = false;
        } else if k < 0.0 {
            // critically damped spring: every change of shot is a smooth flight with no velocity jumps
            let w = 0.85f32;
            let dt = dt.min(0.05);
            let ae = (de - self.cam_eye) * (w * w) - self.cam_ve * (2.0 * w);
            let at = (dtg - self.cam_target) * (w * w) - self.cam_vt * (2.0 * w);
            self.cam_ve += ae * dt;
            self.cam_vt += at * dt;
            self.cam_eye += self.cam_ve * dt;
            self.cam_target += self.cam_vt * dt;
        } else {
            self.cam_ve = Vec3::ZERO;
            self.cam_vt = Vec3::ZERO;
            let a = 1.0 - (-k * dt).exp();
            self.cam_eye += (de - self.cam_eye) * a;
            self.cam_target += (dtg - self.cam_target) * a;
        }
    }

    pub fn update(&mut self, dt: f32) {
        let dt = dt.min(0.05);
        self.time += dt;
        self.scen_t += dt;
        self.since_derez += dt;

        let dur = if self.hold_arena && self.scenario == Scenario::Arena { f32::INFINITY } else { self.scenario.duration() };
        let ramp = if self.scenario == Scenario::Emerge { 0.0 } else { ((self.scen_t - (dur - 0.9)) / 0.9).clamp(0.0, 1.0) };
        self.flash_after = (self.flash_after - dt * 1.0).max(0.0);
        self.flash = ramp.max(self.flash_after);
        if self.scen_t >= dur {
            self.advance();
        }
        // palette: the desktop background's graphite + amber until the battle starts, then Tron blue and red
        if self.scenario == Scenario::Emerge {
            let x = ((self.emerge_prog() - 0.50) / 0.50).clamp(0.0, 1.0);
            self.tron = x * x * x * (x * (x * 6.0 - 15.0) + 10.0);
        }
        let tron_target = match self.scenario {
            Scenario::Emerge => 0.0,
            Scenario::Twin => 0.75,
            _ => 1.0,
        };
        self.tron += (tron_target - self.tron) * (1.0 - (-0.7 * dt).exp());
        for f in &mut self.feed {
            f.1 -= dt;
        }
        self.feed.retain(|f| f.1 > 0.0);

        if self.scenario == Scenario::Arena {
            self.update_arena(dt);
        }
        self.update_feed(dt);

        // Shards: gravity, drag, floor bounce, tumble. In the twin scene late debris is drawn into the hub.
        let drag = (1.0 - 0.35 * dt).max(0.0);
        let mark = Vec3::new(0.0, MARK_Y + self.mono_y, MARK_Z);
        let attract = self.scenario == Scenario::Twin;
        let mut absorbed = 0u32;
        self.shards.retain_mut(|s| {
            s.life -= dt;
            if s.life <= 0.0 {
                return false;
            }
            let age = 1.0 - s.life / s.max;
            if attract && age > 0.4 {
                let to = mark - s.p;
                let d = to.length();
                if d < 4.0 {
                    absorbed += 1;
                    return false;
                }
                s.v += to / d * (38.0 * (age - 0.4) * (age - 0.4) * 4.0) * dt;
            }
            s.v.y -= GRAVITY * dt;
            s.v.x *= drag;
            s.v.z *= drag;
            s.p += s.v * dt;
            s.ang += s.w * dt;
            if s.p.y < 0.05 && s.v.y < 0.0 {
                s.p.y = 0.05;
                s.v.y = -s.v.y * 0.5;
                s.v.x *= 0.82;
                s.v.z *= 0.82;
                s.w *= 0.75;
            }
            true
        });
        self.energy = (self.energy + absorbed as f32 * 0.012).min(2.0);

        // Scan front sweeps out and back (physical → digital); in Transit the whole world is already digital.
        if matches!(self.scenario, Scenario::Arena | Scenario::Twin) {
            let prev = self.scan_x;
            self.scan_x += self.scan_dir * SCAN_SPEED * dt;
            if self.scan_x > SCAN_MAX || self.scan_x < SCAN_MIN {
                self.scan_x = self.scan_x.clamp(SCAN_MIN, SCAN_MAX);
                self.scan_dir = -self.scan_dir;
            }
            if self.scenario == Scenario::Twin && prev * self.scan_x <= 0.0 && prev != self.scan_x {
                self.energy = (self.energy + 0.9).min(2.0);
                self.mono_vy += 3.0;
                self.pulses.push(Pulse { o: Vec2::new(0.0, MARK_Z), r: 0.0, strength: 0.9, color: team_col(1) });
            }
        }

        // Data pulses along circuit traces between buildings and the hub (twin scene only).
        if self.scenario == Scenario::Twin {
            self.pkt_timer -= dt;
            while self.pkt_timer <= 0.0 && self.packets.len() < 60 && !self.towers.is_empty() {
                self.pkt_timer += 0.16;
                let i = (self.rng.f() * self.towers.len() as f32) as usize % self.towers.len();
                let up = self.rng.chance(0.78);
                let mut pts = trace(&self.towers[i]);
                if !up {
                    pts.reverse();
                }
                let seg = [(pts[1] - pts[0]).length(), (pts[2] - pts[1]).length(), (pts[3] - pts[2]).length()];
                self.packets.push(Packet { pts, seg, total: seg.iter().sum(), d: 0.0, speed: self.rng.range(28.0, 48.0), up });
            }
        }
        let mut arrived = 0u32;
        self.packets.retain_mut(|p| {
            p.d += p.speed * dt;
            if p.d >= p.total() + Packet::LEN {
                if p.up {
                    arrived += 1;
                }
                return false;
            }
            true
        });
        self.energy = (self.energy + arrived as f32 * 0.03).min(2.0);

        // Shockwaves
        for p in &mut self.pulses {
            p.r += 34.0 * dt;
            p.strength *= (1.0 - 0.9 * dt).max(0.0);
        }
        self.pulses.retain(|p| p.r < 150.0 && p.strength > 0.02);

        // Monolith: spring toward a gentle breathing target, damped.
        let target = 0.55 * (self.time * 0.6).sin();
        let acc = -46.0 * (self.mono_y - target) - 3.4 * self.mono_vy;
        self.mono_vy += acc * dt;
        self.mono_y += self.mono_vy * dt;
        self.energy += (0.35 - self.energy) * (1.0 - (-1.1 * dt).exp());

        self.update_camera(dt);
    }
}
