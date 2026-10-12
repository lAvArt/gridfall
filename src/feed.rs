//! Live Omarchy activity: public GitHub repo events, polled in the background through `curl`.
//! Keeps who did what (public GitHub login + a short action line, no comment text). The last result is cached on disk so the
//! screensaver replays real recent activity when offline.

use glam::Vec3;
use serde_json::Value;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

/// The watched GitHub repo ("owner/name"), from the config.
fn repo() -> &'static str {
    &crate::brand::get().repo
}

/// The repo's short name for the HUD rows ("STARRED OMARCHY").
fn repo_short() -> String {
    repo().rsplit('/').next().unwrap_or("").to_uppercase()
}
const POLL_SECS: u64 = 180;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Star,
    Fork,
    Push,
    Merge,
    Pr,
    Comment,
    Issue,
    Other,
}

impl Kind {
    /// One colour per event type, shared by the falling line, its floor ripple, the HUD row and the legend.
    pub fn color(self) -> Vec3 {
        match self {
            Kind::Star => Vec3::new(1.0, 0.80, 0.25),     // gold
            Kind::Fork => Vec3::new(0.25, 0.85, 1.0),     // cyan
            Kind::Push => Vec3::new(1.0, 0.50, 0.12),     // orange
            Kind::Pr => Vec3::new(0.72, 0.48, 1.0),       // violet
            Kind::Merge => Vec3::new(0.35, 1.0, 0.55),    // green: merged / released
            Kind::Comment => Vec3::new(0.70, 0.80, 0.98), // pale blue
            Kind::Issue => Vec3::new(1.0, 0.36, 0.46),    // pink-red
            Kind::Other => Vec3::new(0.62, 0.64, 0.70),   // grey
        }
    }
    /// Legend order and names.
    pub const LEGEND: [(Kind, &'static str); 7] =
        [(Kind::Star, "STAR"), (Kind::Fork, "FORK"), (Kind::Push, "PUSH"), (Kind::Pr, "PR"), (Kind::Merge, "MERGE"), (Kind::Comment, "COMMENT"), (Kind::Issue, "ISSUE")];
}

#[derive(Clone, Debug)]
pub struct Ev {
    pub id: String,
    pub kind: Kind,
    /// 0..1: how tall / bright the falling line is
    pub weight: f32,
    pub label: String,
    /// GitHub login of whoever did it
    pub who: String,
    /// unix seconds
    pub t: i64,
}

pub enum Msg {
    Events(Vec<Ev>),
    Stars(u32),
    Online(bool),
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// "2026-10-09T18:00:41Z" → unix seconds
fn epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    Some(days_from_civil(n(0, 4)?, n(5, 7)?, n(8, 10)?) * 86400 + n(11, 13)? * 3600 + n(14, 16)? * 60 + n(17, 19)?)
}

pub fn parse_events(body: &str) -> Vec<Ev> {
    let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(body) else { return Vec::new() };
    arr.iter()
        .filter_map(|e| {
            let id = e.get("id")?.as_str()?.to_string();
            let t = epoch(e.get("created_at")?.as_str()?)?;
            let ty = e.get("type")?.as_str()?;
            let pl = e.get("payload");
            let who: String = e.get("actor").and_then(|a| a.get("display_login").or_else(|| a.get("login"))).and_then(|v| v.as_str()).unwrap_or("someone").to_string();
            // people, not automation: bot accounts would otherwise drown the feed in comments
            let login = e.get("actor").and_then(|a| a.get("login")).and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            if login.ends_with("[bot]") || login.ends_with("bot") || who.to_lowercase().ends_with("bot") {
                return None;
            }
            let num = |k: &str| pl.and_then(|p| p.get(k)).and_then(|v| v.get("number")).and_then(|v| v.as_u64()).map_or(String::new(), |n| format!(" #{n}"));
            let s_of = |k: &str| pl.and_then(|p| p.get(k)).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let (kind, weight, label) = match ty {
                "WatchEvent" => (Kind::Star, 0.55, format!("STARRED {}", repo_short())),
                "ForkEvent" => (Kind::Fork, 0.65, format!("FORKED {}", repo_short())),
                "PushEvent" => {
                    let n = pl.and_then(|p| p.get("size")).and_then(|v| v.as_u64()).unwrap_or(1).max(1);
                    (Kind::Push, (0.55 + 0.12 * n as f32).min(1.0), format!("PUSHED {} COMMIT{}", n, if n == 1 { "" } else { "S" }))
                }
                "PullRequestEvent" => {
                    let merged = pl.and_then(|p| p.get("pull_request")).and_then(|p| p.get("merged")).and_then(|v| v.as_bool()).unwrap_or(false);
                    let action = s_of("action");
                    if merged {
                        (Kind::Merge, 1.0, format!("MERGED PR{}", num("pull_request")))
                    } else {
                        (Kind::Pr, 0.7, format!("{} PR{}", action.to_uppercase(), num("pull_request")))
                    }
                }
                "IssueCommentEvent" => (Kind::Comment, 0.30, format!("COMMENTED ON{}", num("issue"))),
                "PullRequestReviewCommentEvent" | "PullRequestReviewEvent" => (Kind::Comment, 0.34, format!("REVIEWED PR{}", num("pull_request"))),
                "IssuesEvent" => (Kind::Issue, 0.45, format!("{} ISSUE{}", s_of("action").to_uppercase(), num("issue"))),
                "CreateEvent" => (Kind::Other, 0.45, format!("CREATED {} {}", s_of("ref_type").to_uppercase(), s_of("ref")).trim_end().to_string()),
                "ReleaseEvent" => {
                    let tag = pl.and_then(|p| p.get("release")).and_then(|r| r.get("tag_name")).and_then(|v| v.as_str()).unwrap_or("");
                    (Kind::Merge, 1.0, format!("RELEASED {tag}").trim_end().to_string())
                }
                _ => return None,
            };
            Some(Ev { id, kind, weight, label, who, t })
        })
        .collect()
}

fn curl(url: &str) -> Option<String> {
    let out = std::process::Command::new("curl")
        .args(["-sL", "-m", "8", "-H", "Accept: application/vnd.github+json", "-H", "User-Agent: gridfall-screensaver", url])
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn cache_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(format!(".cache/gridfall/events-{}.json", repo().replace('/', "_"))))
}

pub fn spawn() -> Receiver<Msg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        if let Some(body) = cache_path().and_then(|p| std::fs::read_to_string(p).ok()) {
            let evs = parse_events(&body);
            if !evs.is_empty() && tx.send(Msg::Events(evs)).is_err() {
                return;
            }
        }
        let mut round = 0u32;
        loop {
            match curl(&format!("https://api.github.com/repos/{}/events?per_page=100", repo())) {
                Some(body) => {
                    let evs = parse_events(&body);
                    if !evs.is_empty() {
                        if let Some(p) = cache_path() {
                            if let Some(d) = p.parent() {
                                let _ = std::fs::create_dir_all(d);
                            }
                            let _ = std::fs::write(p, &body);
                        }
                        if tx.send(Msg::Online(true)).is_err() || tx.send(Msg::Events(evs)).is_err() {
                            return;
                        }
                    } else if tx.send(Msg::Online(false)).is_err() {
                        return;
                    }
                }
                None => {
                    if tx.send(Msg::Online(false)).is_err() {
                        return;
                    }
                }
            }
            if round % 3 == 0 {
                if let Some(v) = curl(&format!("https://api.github.com/repos/{}", repo())).and_then(|b| serde_json::from_str::<Value>(&b).ok()) {
                    if let Some(n) = v.get("stargazers_count").and_then(|x| x.as_u64()) {
                        if tx.send(Msg::Stars(n as u32)).is_err() {
                            return;
                        }
                    }
                }
            }
            round += 1;
            std::thread::sleep(Duration::from_secs(POLL_SECS));
        }
    });
    rx
}
