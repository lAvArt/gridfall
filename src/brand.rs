//! Everything that makes the screensaver someone's own: the logo (any SVG), the wallpaper it starts from, the
//! accent colours, the names in the HUD and the GitHub repo feeding the arena. Read once at start-up from
//! `$XDG_CONFIG_HOME/gridfall/config.json` (or `--config FILE`), with the current Omarchy theme as the default.

use glam::Vec3;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// What a logo shape turns into: armour plates with lit edges (the default), a dimmer base plate, a recessed echo
/// layer, or an emissive light channel. Chosen per SVG path by its id (or an ancestor group's): `base…`, `echo…`, `glow…`.
#[derive(Clone, Copy, PartialEq)]
pub enum Role {
    Base,
    Armour,
    Echo,
    Glow,
}

impl Role {
    /// edge brightness
    pub fn gain(self) -> f32 {
        match self {
            Role::Echo => 0.38,
            Role::Glow => 1.6,
            _ => 1.0,
        }
    }
    /// depth parallax while the mark rises
    pub fn depth(self) -> f32 {
        match self {
            Role::Base => 0.0,
            Role::Armour => 1.0,
            Role::Echo => 1.9,
            Role::Glow => 0.5,
        }
    }
}

/// Logo points live in a 2048-unit square, y down: centred horizontally, standing on the square's bottom edge.
pub type P2 = (f32, f32);

pub struct Facet {
    pub role: Role,
    /// closed outlines (holes included)
    pub loops: Vec<Vec<P2>>,
    pub tris: Vec<[P2; 3]>,
}

pub struct Edge {
    pub a: P2,
    pub b: P2,
    /// long enough, and on a solid facet: may become one of the first light-cycle trails
    pub seedable: bool,
}

pub struct Logo {
    pub facets: Vec<Facet>,
    pub edges: Vec<Edge>,
    /// distinct corners, for the drafting probe
    pub verts: Vec<P2>,
    /// construction lines: (point, direction), drawn extended across the drafting sheet
    pub guides: Vec<(P2, P2)>,
    /// bounding box in logo space: (u0, v0, u1, v1)
    pub bbox: (f32, f32, f32, f32),
    /// size in the source's own units, for the figure caption
    pub src_size: (f32, f32),
}

impl Logo {
    /// vertical centre (logo space)
    pub fn vc(&self) -> f32 {
        0.5 * (self.bbox.1 + self.bbox.3)
    }
    /// height of the logo's centre above its base, as a fraction of the 2048 square
    pub fn mid_h(&self) -> f32 {
        (2048.0 - self.vc()) / 2048.0
    }

    fn finish(facets: Vec<Facet>, src_size: (f32, f32), guides: Option<Vec<(P2, P2)>>) -> Logo {
        let mut edges = Vec::new();
        let mut verts: Vec<P2> = Vec::new();
        let mut bbox = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for f in &facets {
            for l in &f.loops {
                for i in 0..l.len() {
                    let (a, b) = (l[i], l[(i + 1) % l.len()]);
                    let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
                    if len < 1e-3 {
                        continue;
                    }
                    // at the emergence's 32-unit width, 96 logo units ≈ 1.5 world units
                    edges.push(Edge { a, b, seedable: f.role != Role::Glow && len > 96.0 });
                    if !verts.iter().any(|v| (v.0 - a.0).abs() < 0.5 && (v.1 - a.1).abs() < 0.5) {
                        verts.push(a);
                    }
                    bbox = (bbox.0.min(a.0), bbox.1.min(a.1), bbox.2.max(a.0), bbox.3.max(a.1));
                }
            }
        }
        if verts.len() > 48 {
            let step = verts.len() as f32 / 48.0;
            verts = (0..48).map(|i| verts[(i as f32 * step) as usize]).collect();
        }
        let guides = guides.unwrap_or_else(|| {
            // the lines through the longest edges, each line once
            let mut by_len: Vec<&Edge> = edges.iter().collect();
            by_len.sort_by(|x, y| elen(y).total_cmp(&elen(x)));
            let mut out: Vec<(P2, P2)> = Vec::new();
            for e in by_len {
                let l = elen(e);
                let d = ((e.b.0 - e.a.0) / l, (e.b.1 - e.a.1) / l);
                let dup = out.iter().any(|(p, q)| {
                    let cross = (q.0 * d.1 - q.1 * d.0).abs();
                    let off = ((e.a.0 - p.0) * q.1 - (e.a.1 - p.1) * q.0).abs();
                    cross < 0.02 && off < 12.0
                });
                if !dup {
                    out.push((e.a, d));
                }
                if out.len() >= 10 {
                    break;
                }
            }
            out
        });
        Logo { facets, edges, verts, guides, bbox, src_size }
    }

    /// The Twinise mark, built in: four facets in its native 2048² space.
    pub fn twinise() -> Logo {
        const BAR: [P2; 4] = [(0.0, 483.65), (1564.35, 483.65), (2048.0, 0.0), (483.64, 0.0)];
        const STEM: [P2; 4] = [(1549.67, 597.76), (1550.06, 1394.23), (896.32, 2048.0), (896.32, 1251.11)];
        const ECHO: [P2; 4] = [(1450.24, 597.76), (1450.63, 1379.48), (782.14, 2048.0), (782.14, 1265.86)];
        const SLIVER: [P2; 4] = [(2048.0, 30.90), (2048.0, 68.57), (896.16, 1220.43), (896.16, 1182.74)];
        let quad = |q: [P2; 4], role| Facet { role, loops: vec![q.to_vec()], tris: vec![[q[0], q[1], q[2]], [q[0], q[2], q[3]]] };
        let facets = vec![quad(BAR, Role::Base), quad(STEM, Role::Armour), quad(ECHO, Role::Echo), quad(SLIVER, Role::Glow)];
        // the drafting sheet's construction lines, as drawn on the lAvArt desktop
        let mut guides: Vec<(P2, P2)> = Vec::new();
        let diag = (std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2);
        for p in [(2048.0, 30.9), (1564.35, 483.65), (782.14, 2048.0), (896.32, 2048.0)] {
            guides.push((p, diag));
        }
        for v in [483.65, 597.76] {
            guides.push(((0.0, v), (1.0, 0.0)));
        }
        for u in [782.14, 896.32, 1450.24, 1550.06] {
            guides.push(((u, 0.0), (0.0, 1.0)));
        }
        Logo::finish(facets, (2048.0, 2048.0), Some(guides))
    }

    /// Any SVG: every filled (or stroked) shape becomes a facet, flattened and triangulated.
    pub fn from_svg(path: &Path) -> Result<Logo, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut opt = usvg::Options::default();
        if data.windows(5).any(|w| w == b"<text") {
            opt.fontdb_mut().load_system_fonts();
        }
        let tree = usvg::Tree::from_data(&data, &opt).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut shapes: Vec<(Role, Vec<Vec<P2>>, bool)> = Vec::new();
        collect(tree.root(), Role::Armour, &mut shapes);
        if shapes.is_empty() {
            return Err(format!("{}: no filled shapes", path.display()));
        }
        // fit into the 2048 square: centred horizontally, standing on the bottom edge
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (_, loops, _) in &shapes {
            for p in loops.iter().flatten() {
                (x0, y0, x1, y1) = (x0.min(p.0), y0.min(p.1), x1.max(p.0), y1.max(p.1));
            }
        }
        let s = 2048.0 / (x1 - x0).max(y1 - y0).max(1e-3);
        let cx = 0.5 * (x0 + x1);
        let map = |p: P2| ((p.0 - cx) * s + 1024.0, (p.1 - y1) * s + 2048.0);
        let mut facets = Vec::new();
        for (role, loops, even_odd) in shapes {
            let loops: Vec<Vec<P2>> = loops.into_iter().map(|l| simplify(l.into_iter().map(map).collect())).filter(|l| l.len() >= 3).collect();
            if loops.is_empty() {
                continue;
            }
            let tris = tessellate(&loops, even_odd);
            facets.push(Facet { role, loops, tris });
        }
        Ok(Logo::finish(facets, (x1 - x0, y1 - y0), None))
    }
}

fn elen(e: &Edge) -> f32 {
    ((e.b.0 - e.a.0).powi(2) + (e.b.1 - e.a.1).powi(2)).sqrt()
}

fn role_of(id: &str, inherited: Role) -> Role {
    let id = id.to_ascii_lowercase();
    if id.starts_with("glow") {
        Role::Glow
    } else if id.starts_with("echo") {
        Role::Echo
    } else if id.starts_with("base") {
        Role::Base
    } else {
        inherited
    }
}

fn collect(g: &usvg::Group, role: Role, out: &mut Vec<(Role, Vec<Vec<P2>>, bool)>) {
    let role = role_of(g.id(), role);
    for node in g.children() {
        match node {
            usvg::Node::Group(c) => collect(c, role, out),
            usvg::Node::Text(t) => collect(t.flattened(), role, out),
            usvg::Node::Path(p) if p.is_visible() => {
                let role = role_of(p.id(), role);
                let ts = p.abs_transform();
                if let Some(fill) = p.fill() {
                    if let Some(d) = p.data().clone().transform(ts) {
                        out.push((role, flatten(&d), fill.rule() == usvg::FillRule::EvenOdd));
                    }
                } else if let Some(st) = p.stroke() {
                    // line-art logos: the stroke's outline becomes the shape
                    let scale = (ts.sx * ts.sx + ts.ky * ts.ky).sqrt().max(1e-3);
                    if let Some(d) = p.data().stroke(&st.to_tiny_skia(), scale).and_then(|d| d.transform(ts)) {
                        out.push((role, flatten(&d), false));
                    }
                }
            }
            _ => {}
        }
    }
}

/// Curves to polylines, one closed loop per subpath.
fn flatten(path: &usvg::tiny_skia_path::Path) -> Vec<Vec<P2>> {
    use usvg::tiny_skia_path::PathSegment;
    let b = path.bounds();
    let tol = (b.width().max(b.height()) / 300.0).max(1e-4);
    let mut loops: Vec<Vec<P2>> = Vec::new();
    let mut cur: Vec<P2> = Vec::new();
    let mut last = (0.0, 0.0);
    let steps = |pts: &[P2]| -> usize {
        let l: f32 = pts.windows(2).map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt()).sum();
        ((l / tol).sqrt().ceil() as usize).clamp(2, 32)
    };
    for seg in path.segments() {
        match seg {
            PathSegment::MoveTo(p) => {
                if cur.len() >= 3 {
                    loops.push(std::mem::take(&mut cur));
                }
                cur.clear();
                last = (p.x, p.y);
                cur.push(last);
            }
            PathSegment::LineTo(p) => {
                last = (p.x, p.y);
                cur.push(last);
            }
            PathSegment::QuadTo(c, p) => {
                let (c, p) = ((c.x, c.y), (p.x, p.y));
                let n = steps(&[last, c, p]);
                for i in 1..=n {
                    let t = i as f32 / n as f32;
                    let u = 1.0 - t;
                    cur.push((u * u * last.0 + 2.0 * u * t * c.0 + t * t * p.0, u * u * last.1 + 2.0 * u * t * c.1 + t * t * p.1));
                }
                last = p;
            }
            PathSegment::CubicTo(c1, c2, p) => {
                let (c1, c2, p) = ((c1.x, c1.y), (c2.x, c2.y), (p.x, p.y));
                let n = steps(&[last, c1, c2, p]);
                for i in 1..=n {
                    let t = i as f32 / n as f32;
                    let u = 1.0 - t;
                    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                    cur.push((a * last.0 + b * c1.0 + c * c2.0 + d * p.0, a * last.1 + b * c1.1 + c * c2.1 + d * p.1));
                }
                last = p;
            }
            PathSegment::Close => {
                if cur.len() >= 3 {
                    loops.push(std::mem::take(&mut cur));
                }
                cur.clear();
            }
        }
    }
    if cur.len() >= 3 {
        loops.push(cur);
    }
    loops
}

/// Drop repeated points and corners that are (nearly) straight.
fn simplify(l: Vec<P2>) -> Vec<P2> {
    let mut v: Vec<P2> = Vec::with_capacity(l.len());
    for p in l {
        if v.last().is_none_or(|q: &P2| (q.0 - p.0).abs() > 0.05 || (q.1 - p.1).abs() > 0.05) {
            v.push(p);
        }
    }
    while v.len() > 3 && (v[0].0 - v[v.len() - 1].0).abs() < 0.05 && (v[0].1 - v[v.len() - 1].1).abs() < 0.05 {
        v.pop();
    }
    let mut changed = true;
    while changed && v.len() > 3 {
        changed = false;
        let n = v.len();
        for i in 0..n {
            let (a, b, c) = (v[(i + n - 1) % n], v[i], v[(i + 1) % n]);
            let (ab, bc) = ((b.0 - a.0, b.1 - a.1), (c.0 - b.0, c.1 - b.1));
            let cross = ab.0 * bc.1 - ab.1 * bc.0;
            let (la, lb) = ((ab.0 * ab.0 + ab.1 * ab.1).sqrt(), (bc.0 * bc.0 + bc.1 * bc.1).sqrt());
            if la * lb < 1e-6 || (cross.abs() / (la * lb) < 0.004 && ab.0 * bc.0 + ab.1 * bc.1 > 0.0) {
                v.remove(i);
                changed = true;
                break;
            }
        }
    }
    v
}

fn tessellate(loops: &[Vec<P2>], even_odd: bool) -> Vec<[P2; 3]> {
    use lyon_tessellation::math::point;
    use lyon_tessellation::path::Path as LPath;
    use lyon_tessellation::{BuffersBuilder, FillOptions, FillRule, FillTessellator, FillVertex, VertexBuffers};
    let mut b = LPath::builder();
    for l in loops {
        b.begin(point(l[0].0, l[0].1));
        for p in &l[1..] {
            b.line_to(point(p.0, p.1));
        }
        b.end(true);
    }
    let path = b.build();
    let mut geo: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let opts = FillOptions::tolerance(0.5).with_fill_rule(if even_odd { FillRule::EvenOdd } else { FillRule::NonZero });
    if FillTessellator::new().tessellate_path(&path, &opts, &mut BuffersBuilder::new(&mut geo, |v: FillVertex| v.position().to_array())).is_err() {
        return Vec::new();
    }
    geo.indices
        .chunks_exact(3)
        .map(|t| {
            let p = |i: u32| (geo.vertices[i as usize][0], geo.vertices[i as usize][1]);
            [p(t[0]), p(t[1]), p(t[2])]
        })
        .collect()
}

pub struct Wallpaper {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

pub struct Brand {
    pub logo: Logo,
    /// the desktop the screensaver starts from; None = the procedural graphite sheet
    pub wallpaper: Option<Wallpaper>,
    /// HUD system name ("SYS.ID // NAME-01") and figure caption
    pub name: String,
    /// display-face lockup shown over the opening (one or two lines; empty = none)
    pub wordmark: Vec<String>,
    pub tagline: String,
    /// bottom-right HUD credit
    pub footer: String,
    /// GitHub "owner/repo" whose live activity rains into the arena
    pub repo: String,
    /// theme accent, display value (sRGB 0..1)
    pub accent: Vec3,
    /// theme foreground grey, display value
    pub fg: Vec3,
    /// theme background, display value (what the procedural sheet matches)
    pub bg: f32,
    /// light-cycle team colours: [programs, users]
    pub teams: [Vec3; 2],
    /// the lAvArt desktop's hairline grid, signal sweeps, telemetry nodes and registration crosses
    pub desktop_grid: bool,
    /// construction lines, figure caption and vertex probe around the flat logo
    pub drafting: bool,
    /// the logo is already on the desktop (so it is there from the first frame instead of being drawn in)
    pub logo_on_wallpaper: bool,
    /// where the flat logo sits across the opening frame (fraction of its height; 0 = centred)
    pub mark_offset: f32,
    /// "auto"/"low": the integrated, power-saving GPU (the discrete one stays asleep); "high": the fast one
    pub gpu: String,
}

static BRAND: OnceLock<Brand> = OnceLock::new();

pub fn get() -> &'static Brand {
    BRAND.get_or_init(|| load(&std::env::args().collect::<Vec<_>>()))
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn hex(s: &str) -> Option<Vec3> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(Vec3::new(((v >> 16) & 255) as f32 / 255.0, ((v >> 8) & 255) as f32 / 255.0, (v & 255) as f32 / 255.0))
}

/// The active Omarchy theme's colours (Omarchy 4: ~/.local/state/omarchy/current/theme/colors.toml).
fn theme_color(key: &str) -> Option<Vec3> {
    let dirs = [home().join(".local/state/omarchy/current/theme"), home().join(".config/omarchy/current/theme")];
    let text = dirs.iter().find_map(|d| std::fs::read_to_string(d.join("colors.toml")).ok())?;
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| hex(v.trim().trim_matches('"')))?
    })
}

fn current_wallpaper() -> Option<PathBuf> {
    [home().join(".local/state/omarchy/current/background"), home().join(".config/omarchy/current/background")]
        .into_iter()
        .find_map(|p| std::fs::canonicalize(p).ok())
}

fn load_wallpaper(p: &Path) -> Option<Wallpaper> {
    let img = match image::open(p) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("gridfall: wallpaper {}: {e}", p.display());
            return None;
        }
    };
    // plenty for any screen; keeps the upload and the GPU texture limit in check
    let img = if img.width() > 4096 || img.height() > 4096 { img.resize(4096, 4096, image::imageops::FilterType::Triangle) } else { img };
    let rgba = img.to_rgba8();
    Some(Wallpaper { w: rgba.width(), h: rgba.height(), rgba: rgba.into_raw() })
}

fn expand(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(r) => home().join(r),
        None => PathBuf::from(s),
    }
}

fn load(args: &[String]) -> Brand {
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let cfg_path = arg("--config").map(|s| expand(&s)).unwrap_or_else(|| {
        let base = std::env::var("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".config"));
        base.join("gridfall/config.json")
    });
    let cfg: Value = std::fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).map_err(|e| eprintln!("gridfall: {}: {e}", cfg_path.display())).ok())
        .unwrap_or(Value::Null);
    let s = |k: &str| arg(&format!("--{k}")).or_else(|| cfg.get(k).and_then(|v| v.as_str()).map(String::from));
    let b = |k: &str, d: bool| cfg.get(k).and_then(|v| v.as_bool()).unwrap_or(d);
    let lavart = s("preset").as_deref() == Some("lavart");

    let accent = s("accent").and_then(|v| hex(&v)).or_else(|| theme_color("accent")).unwrap_or(Vec3::new(0.902, 0.557, 0.051));
    let fg = theme_color("foreground").unwrap_or(Vec3::splat(0.745));
    let bg = theme_color("background").map(|c| (c.x + c.y + c.z) / 3.0).unwrap_or(0.0706);
    let teams = {
        let t: Vec<Vec3> = cfg.get("teams").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().and_then(hex)).collect()).unwrap_or_default();
        if t.len() == 2 { [t[0], t[1]] } else { [Vec3::new(1.0, 0.16, 0.10), Vec3::new(0.05, 0.50, 1.0)] }
    };

    let logo_spec = s("logo").unwrap_or_else(|| if lavart { "twinise".into() } else { "omarchy".into() });
    let logo = match logo_spec.as_str() {
        "twinise" => Ok(Logo::twinise()),
        "omarchy" => Logo::from_svg(Path::new("/usr/share/omarchy/logo.svg")),
        p => Logo::from_svg(&expand(p)),
    }
    .unwrap_or_else(|e| {
        eprintln!("gridfall: logo: {e}; using the built-in mark");
        Logo::twinise()
    });

    let wall_spec = s("wallpaper").unwrap_or_else(|| if lavart { "none".into() } else { "auto".into() });
    let wallpaper = match wall_spec.as_str() {
        "none" => None,
        "auto" => current_wallpaper().and_then(|p| load_wallpaper(&p)),
        p => load_wallpaper(&expand(p)),
    };

    let lines = |k: &str, d: &[&str]| -> Vec<String> {
        match cfg.get(k) {
            Some(Value::String(v)) => v.split('\n').filter(|l| !l.is_empty()).map(String::from).collect(),
            Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            _ => d.iter().map(|x| x.to_string()).collect(),
        }
    };
    let (name_d, foot_d, word_d, tag_d): (&str, &str, &[&str], &str) = if lavart {
        ("TWINISE", "LAVART STUDIO © 2026", &["lAvArt", "Studio"], "TWINISE  /  DIGITAL TWIN SYSTEMS")
    } else {
        ("OMARCHY", "", &[], "")
    };
    Brand {
        logo,
        wallpaper,
        name: s("name").unwrap_or_else(|| name_d.into()).to_uppercase(),
        wordmark: lines("wordmark", word_d),
        tagline: s("tagline").unwrap_or_else(|| tag_d.into()),
        footer: s("footer").unwrap_or_else(|| foot_d.into()),
        repo: s("repo").unwrap_or_else(|| "omacom/omarchy".into()),
        accent,
        fg,
        bg,
        teams,
        desktop_grid: b("desktop_grid", lavart),
        drafting: b("drafting", true),
        logo_on_wallpaper: b("logo_on_wallpaper", lavart),
        gpu: s("gpu").unwrap_or_else(|| "auto".into()),
        mark_offset: cfg.get("mark_offset").and_then(|v| v.as_f64()).map(|v| v as f32).unwrap_or(if lavart { 0.32 } else { 0.0 }),
    }
}
