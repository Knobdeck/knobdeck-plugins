//! Plan limits (used % of the 5 h / weekly windows) for Claude Code and Codex, without hammering
//! the providers' usage endpoints (they rate-limit hard, and a 429 used to be retried every two
//! minutes).
//!
//! Sources, best first:
//! 1. **Claude Code's status line**: it hands `rate_limits` to the status line command after each
//!    reply. Setup points the status line at this plugin's hook program (`hook statusline`), which
//!    records the numbers and then runs the user's own status line, if they had one. No network.
//! 2. **Codex's session logs**: every `token_count` event carries `rate_limits`. No network.
//! 3. **The usage endpoints**, only when the local numbers are old (no session ran for a while)
//!    and a tile is placed: at most one call per 10 minutes per provider, `Retry-After` and an
//!    exponential back-off (5 min … 2 h) obeyed and kept in a file so a restart doesn't start
//!    over, and the last good numbers stay on the key while the endpoint says no.
//!
//! Tokens are only read, never refreshed or written back (that would rotate the CLI's refresh
//! token and log it out).

use crate::store::{now_ms, write_atomic};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Local numbers newer than this are trusted; the endpoint isn't called.
const FRESH_MS: u64 = 15 * 60_000;
/// Minimum gap between two endpoint calls.
const API_GAP_MS: u64 = 10 * 60_000;
/// Minimum gap for a call the user asked for with a key press.
const FORCED_GAP_MS: u64 = 60_000;
const BACKOFF_FIRST_MS: u64 = 5 * 60_000;
const BACKOFF_MAX_MS: u64 = 2 * 3600_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn key(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Win {
    /// Percent used, 0–100.
    pub used: f64,
    /// Unix ms.
    pub resets_at: Option<u64>,
    /// Window length in seconds (0 = unknown).
    #[serde(default)]
    pub secs: u64,
}

impl Win {
    /// "5h", "7d", "30d".
    pub fn name(&self) -> String {
        match self.secs {
            0 => String::new(),
            s if s >= 86400 => format!("{}d", s / 86400),
            s => format!("{}h", (s / 3600).max(1)),
        }
    }

    /// A window that has reset since it was recorded starts again from zero.
    pub fn at(&self, now: u64) -> Win {
        match self.resets_at {
            Some(r) if r <= now => Win {
                used: 0.0,
                resets_at: None,
                secs: self.secs,
            },
            _ => self.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snap {
    /// When these numbers were recorded (unix ms).
    pub at: u64,
    pub src: String,
    /// Claude: "5h", "7d", "fable". Codex: "primary", "secondary".
    pub wins: BTreeMap<String, Win>,
}

/// What the endpoint last said, kept between runs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Api {
    pub last: u64,
    pub until: u64,
    pub fails: u32,
    pub error: String,
}

fn dir(data: &Path) -> PathBuf {
    data.join("limits")
}

fn load<T: for<'a> Deserialize<'a> + Default>(path: &Path) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save<T: Serialize>(path: &Path, v: &T) {
    if let Some(p) = path.parent() {
        let _ = fs::create_dir_all(p);
    }
    if let Ok(t) = serde_json::to_vec(v) {
        let _ = write_atomic(path, &t);
    }
}

pub fn read(data: &Path, p: Provider) -> Snap {
    load(&dir(data).join(format!("{}.json", p.key())))
}

pub fn api_state(data: &Path, p: Provider) -> Api {
    load(&dir(data).join(format!("{}-api.json", p.key())))
}

fn write_snap(data: &Path, p: Provider, s: &Snap) {
    save(&dir(data).join(format!("{}.json", p.key())), s);
}

fn write_api(data: &Path, p: Provider, a: &Api) {
    save(&dir(data).join(format!("{}-api.json", p.key())), a);
}

// ── Parsing ───────────────────────────────────────────────────────────────────

/// Unix seconds, milliseconds or an ISO date → unix ms.
fn time_ms(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => {
            let n = n.as_f64()?;
            Some(if n > 1e11 {
                n as u64
            } else {
                (n * 1000.0) as u64
            })
        }
        Value::String(s) => parse_iso_ms(s).or_else(|| {
            s.parse::<f64>().ok().map(|n| {
                if n > 1e11 {
                    n as u64
                } else {
                    (n * 1000.0) as u64
                }
            })
        }),
        _ => None,
    }
}

pub fn parse_iso_ms(s: &str) -> Option<u64> {
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let frac = s
        .get(19..)
        .and_then(|r| r.strip_prefix('.'))
        .map_or(0, |r| {
            let digits: String = r.chars().take_while(char::is_ascii_digit).take(3).collect();
            format!("{digits:0<3}").parse().unwrap_or(0)
        });
    // days_from_civil (Howard Hinnant)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(((days * 24 + h) * 60 + mi) * 60 + sec)
        .ok()
        .map(|t| t * 1000 + frac)
}

/// Claude Code's status line input: `"rate_limits": {"five_hour": {"used_percentage": 23.5,
/// "resets_at": 1738425600}, "seven_day": {…}}`. Absent until the first reply of a session
/// (and for API-key logins).
pub fn claude_from_status_line(ev: &Value) -> BTreeMap<String, Win> {
    let mut out = BTreeMap::new();
    for (key, name, secs) in [
        ("five_hour", "5h", 5 * 3600),
        ("seven_day", "7d", 7 * 86400),
    ] {
        let w = &ev["rate_limits"][key];
        if let Some(used) = w["used_percentage"]
            .as_f64()
            .or_else(|| w["utilization"].as_f64())
        {
            out.insert(
                name.to_string(),
                Win {
                    used,
                    resets_at: time_ms(&w["resets_at"]),
                    secs,
                },
            );
        }
    }
    out
}

/// The `/api/oauth/usage` response.
pub fn claude_from_api(v: &Value) -> BTreeMap<String, Win> {
    let mut out = claude_from_status_line(&serde_json::json!({"rate_limits": v}));
    // The model-scoped weekly limit, listed in `limits` (kind weekly_scoped).
    let scoped = v["limits"].as_array().and_then(|ls| {
        ls.iter().find(|l| {
            let m = &l["scope"]["model"];
            l["kind"] == "weekly_scoped"
                && [&m["display_name"], &m["id"]].iter().any(|s| {
                    s.as_str()
                        .is_some_and(|s| s.to_lowercase().contains("fable"))
                })
        })
    });
    if let Some(l) = scoped {
        if let Some(used) = l["percent"].as_f64() {
            out.insert(
                "fable".into(),
                Win {
                    used,
                    resets_at: time_ms(&l["resets_at"]),
                    secs: 7 * 86400,
                },
            );
        }
    }
    out
}

/// Codex's `rate_limits`: `{"primary": {"used_percent": 2.0, "window_minutes": 300,
/// "resets_at": 1738425600}, "secondary": {…}}`.
pub fn codex_from_log(rl: &Value) -> BTreeMap<String, Win> {
    let mut out = BTreeMap::new();
    for key in ["primary", "secondary"] {
        let w = &rl[key];
        let Some(used) = w["used_percent"].as_f64() else {
            continue;
        };
        let resets_at = time_ms(&w["resets_at"])
            .or_else(|| w["resets_in_seconds"].as_u64().map(|s| now_ms() + s * 1000));
        out.insert(
            key.to_string(),
            Win {
                used,
                resets_at,
                secs: w["window_minutes"].as_u64().unwrap_or(0) * 60,
            },
        );
    }
    out
}

/// The usage endpoint's response for Codex.
pub fn codex_from_api(v: &Value) -> BTreeMap<String, Win> {
    let mut out = BTreeMap::new();
    for (key, api_key) in [
        ("primary", "primary_window"),
        ("secondary", "secondary_window"),
    ] {
        let w = &v["rate_limit"][api_key];
        if let Some(used) = w["used_percent"].as_f64() {
            out.insert(
                key.to_string(),
                Win {
                    used,
                    resets_at: w["reset_at"].as_u64().map(|s| s * 1000),
                    secs: w["limit_window_seconds"].as_u64().unwrap_or(0),
                },
            );
        }
    }
    out
}

// ── Recording local numbers ───────────────────────────────────────────────────

/// Keep `new` as the provider's numbers when they are newer than what is stored. Returns whether
/// the file was written.
fn record(data: &Path, p: Provider, new: BTreeMap<String, Win>, at: u64, src: &str) -> bool {
    if new.is_empty() {
        return false;
    }
    let mut snap = read(data, p);
    if at < snap.at {
        return false;
    }
    // Same numbers a moment ago: leave the file alone (the status line fires on every update).
    if at.saturating_sub(snap.at) < 30_000 && new.iter().all(|(k, w)| snap.wins.get(k) == Some(w)) {
        return false;
    }
    snap.wins.extend(new);
    snap.at = at;
    snap.src = src.into();
    write_snap(data, p, &snap);
    true
}

/// From the status line hook.
pub fn record_claude_status_line(data: &Path, ev: &Value) {
    record(
        data,
        Provider::Claude,
        claude_from_status_line(ev),
        now_ms(),
        "status line",
    );
}

/// The newest `rate_limits` in the tail of Codex's most recently written session logs.
pub fn scan_codex_logs(data: &Path) {
    let Some(root) = crate::codex::sessions_root() else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = vec![];
    // The newest day folders; a session that runs past midnight is in the previous one.
    let days: Vec<PathBuf> = crate::codex::subdirs_desc(&root)
        .into_iter()
        .take(1)
        .flat_map(|y| crate::codex::subdirs_desc(&y).into_iter().take(2))
        .flat_map(|m| crate::codex::subdirs_desc(&m).into_iter().take(3))
        .take(3)
        .collect();
    for day in days {
        for f in fs::read_dir(&day)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
        {
            if f.extension().is_some_and(|x| x == "jsonl") {
                if let Ok(t) = fs::metadata(&f).and_then(|m| m.modified()) {
                    files.push((t, f));
                }
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (mtime, f) in files.into_iter().take(4) {
        let Some(wins) = last_rate_limits(&f) else {
            continue;
        };
        let at = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        record(data, Provider::Codex, wins, at, "session log");
        return;
    }
}

/// The last `rate_limits` object in the final 512 KB of a log.
fn last_rate_limits(path: &Path) -> Option<BTreeMap<String, Win>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(512 * 1024);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(512 * 1024).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev().filter(|l| l.contains("\"rate_limits\"")) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let rl = if v["payload"]["rate_limits"].is_object() {
            &v["payload"]["rate_limits"]
        } else {
            &v["rate_limits"]
        };
        let wins = codex_from_log(rl);
        if !wins.is_empty() {
            return Some(wins);
        }
    }
    None
}

// ── The endpoints, rarely ─────────────────────────────────────────────────────

/// Back-off after the `fails`-th failure in a row, honouring a `Retry-After` (seconds).
pub fn backoff_ms(fails: u32, retry_after: Option<u64>) -> u64 {
    let exp = BACKOFF_FIRST_MS
        .saturating_mul(1u64 << fails.saturating_sub(1).min(8))
        .min(BACKOFF_MAX_MS);
    exp.max(
        retry_after
            .unwrap_or(0)
            .saturating_mul(1000)
            .min(BACKOFF_MAX_MS),
    )
}

/// Whether the endpoint may be called now.
pub fn api_due(snap: &Snap, api: &Api, now: u64, forced: bool) -> bool {
    if now < api.until {
        return false;
    }
    if now.saturating_sub(api.last) < if forced { FORCED_GAP_MS } else { API_GAP_MS } {
        return false;
    }
    forced || now.saturating_sub(snap.at) >= FRESH_MS
}

enum Fail {
    /// 401 / 403: the CLI's token expired; it refreshes it itself on its next run.
    SignIn,
    Limited(Option<u64>),
    Other(String),
}

fn http() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build()
}

fn get(req: ureq::Request) -> Result<Value, Fail> {
    match req.call() {
        Ok(r) => r
            .into_json()
            .map_err(|_| Fail::Other("bad response".into())),
        Err(ureq::Error::Status(401 | 403, _)) => Err(Fail::SignIn),
        Err(ureq::Error::Status(429, r)) => Err(Fail::Limited(
            r.header("retry-after").and_then(|h| h.trim().parse().ok()),
        )),
        Err(ureq::Error::Status(c, _)) => Err(Fail::Other(format!("HTTP {c}"))),
        Err(_) => Err(Fail::Other("offline".into())),
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn claude_token() -> Option<String> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".claude")))?;
    let v = read_json(&dir.join(".credentials.json"));
    // macOS Claude Code keeps the login in the Keychain, not in a file.
    #[cfg(target_os = "macos")]
    let v = v.or_else(|| {
        std::process::Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-w",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok()?.trim().parse().ok())
    });
    v?["claudeAiOauth"]["accessToken"]
        .as_str()
        .map(str::to_string)
}

fn fetch(p: Provider) -> Result<BTreeMap<String, Win>, Fail> {
    match p {
        Provider::Claude => {
            let token = claude_token().ok_or_else(|| Fail::Other("no login found".into()))?;
            get(http()
                .get("https://api.anthropic.com/api/oauth/usage")
                .set("Authorization", &format!("Bearer {token}"))
                .set("anthropic-version", "2023-06-01")
                .set("anthropic-beta", "oauth-2025-04-20"))
            .map(|v| claude_from_api(&v))
        }
        Provider::Codex => {
            let path = crate::install::codex_config_path()
                .map(|c| c.with_file_name("auth.json"))
                .ok_or_else(|| Fail::Other("no login found".into()))?;
            let v = read_json(&path).ok_or_else(|| Fail::Other("no login found".into()))?;
            let t = &v["tokens"];
            let token = t["access_token"]
                .as_str()
                .ok_or_else(|| Fail::Other("no login found".into()))?;
            let mut req = http()
                .get("https://chatgpt.com/backend-api/codex/usage")
                .set("Authorization", &format!("Bearer {token}"))
                .set("User-Agent", "codex_cli_rs")
                .set("Accept", "application/json");
            if let Some(id) = t["account_id"].as_str() {
                req = req.set("chatgpt-account-id", id);
            }
            get(req).map(|v| codex_from_api(&v))
        }
    }
}

static REFRESH_CLAUDE: AtomicBool = AtomicBool::new(false);
static REFRESH_CODEX: AtomicBool = AtomicBool::new(false);

/// A key press: look again soon (still bound by the minimum gap and any back-off).
pub fn request_refresh(p: Provider) {
    match p {
        Provider::Claude => &REFRESH_CLAUDE,
        Provider::Codex => &REFRESH_CODEX,
    }
    .store(true, Ordering::Relaxed);
}

/// One pass for a provider that has a key placed; cheap unless the endpoint is due.
pub fn poll(data: &Path, p: Provider) {
    if p == Provider::Codex {
        scan_codex_logs(data);
    }
    let forced = match p {
        Provider::Claude => &REFRESH_CLAUDE,
        Provider::Codex => &REFRESH_CODEX,
    }
    .swap(false, Ordering::Relaxed);
    let (snap, mut api, now) = (read(data, p), api_state(data, p), now_ms());
    if !api_due(&snap, &api, now, forced) {
        return;
    }
    api.last = now;
    match fetch(p) {
        Ok(wins) if !wins.is_empty() => {
            record(data, p, wins, now, "usage endpoint");
            api = Api {
                last: now,
                ..Default::default()
            };
        }
        Ok(_) => api.error = "not on this plan".into(),
        Err(fail) => {
            api.fails += 1;
            let (error, wait) = match fail {
                Fail::SignIn => ("sign in".to_string(), 30 * 60_000),
                Fail::Limited(after) => ("rate limited".to_string(), backoff_ms(api.fails, after)),
                Fail::Other(e) => (e, backoff_ms(api.fails.min(2), None)),
            };
            api.error = error;
            api.until = now + wait;
        }
    }
    write_api(data, p, &api);
}

/// Why a key has nothing to show.
pub fn why_empty(data: &Path, p: Provider) -> String {
    let api = api_state(data, p);
    let now = now_ms();
    if now < api.until {
        return format!("{} · retry in {}", api.error, left(api.until));
    }
    if !api.error.is_empty() {
        return api.error;
    }
    match p {
        Provider::Claude => "waiting for Claude Code".into(),
        Provider::Codex => "waiting for Codex".into(),
    }
}

/// Time until `at` (unix ms): "2d 3h", "1h 48m", "12m".
pub fn left(at: u64) -> String {
    let m = at.saturating_sub(now_ms()) / 60_000;
    let (d, h, m) = (m / 1440, m % 1440 / 60, m % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_line() {
        let ev = json!({"rate_limits": {"five_hour": {"used_percentage": 23.5, "resets_at": 1738425600}, "seven_day": {"used_percentage": 41.2, "resets_at": 1738857600}}});
        let w = claude_from_status_line(&ev);
        assert_eq!(
            w["5h"],
            Win {
                used: 23.5,
                resets_at: Some(1738425600_000),
                secs: 18000
            }
        );
        assert_eq!(w["7d"].used, 41.2);
        assert!(claude_from_status_line(&json!({"model": "x"})).is_empty());
        // Only one window present.
        assert_eq!(
            claude_from_status_line(&json!({"rate_limits": {"five_hour": {"used_percentage": 1}}}))
                .len(),
            1
        );
    }

    #[test]
    fn claude_endpoint() {
        let v: Value = r#"{"five_hour":{"utilization":18.0,"resets_at":"1970-01-01T00:00:01.5+00:00"},"seven_day":{"utilization":31.0,"resets_at":null},
            "limits":[{"kind":"weekly_all","percent":31},{"kind":"weekly_scoped","percent":7,"resets_at":"1970-01-01T00:00:02Z","scope":{"model":{"display_name":"Fable"}}}]}"#
            .parse()
            .unwrap();
        let w = claude_from_api(&v);
        assert_eq!(w["5h"].resets_at, Some(1500));
        assert_eq!(w["7d"].used, 31.0);
        assert_eq!(
            w["fable"],
            Win {
                used: 7.0,
                resets_at: Some(2000),
                secs: 604800
            }
        );
    }

    #[test]
    fn codex() {
        let rl = json!({"primary": {"used_percent": 2.0, "window_minutes": 300, "resets_at": 1738425600}, "secondary": {"used_percent": 9.0, "window_minutes": 10080, "resets_at": 1738857600}});
        let w = codex_from_log(&rl);
        assert_eq!(w["primary"].name(), "5h");
        assert_eq!(w["secondary"].name(), "7d");
        assert_eq!(w["secondary"].resets_at, Some(1738857600_000));
        let api = json!({"rate_limit": {"primary_window": {"used_percent": 2, "limit_window_seconds": 2592000, "reset_at": 10}, "secondary_window": null}});
        let w = codex_from_api(&api);
        assert_eq!(
            (w["primary"].name(), w["primary"].resets_at),
            ("30d".to_string(), Some(10_000))
        );
        assert!(!w.contains_key("secondary"));
    }

    #[test]
    fn log_tail() {
        let dir = std::env::temp_dir().join(format!("kd-limits-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("rollout.jsonl");
        let ev = |used: u32| {
            format!("{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"rate_limits\":{{\"primary\":{{\"used_percent\":{used}.0,\"window_minutes\":300,\"resets_at\":99999999999}}}}}}}}\n")
        };
        fs::write(
            &f,
            format!(
                "{{\"type\":\"session_meta\"}}\n{}{}{{\"type\":\"other\"}}\n",
                ev(3),
                ev(5)
            ),
        )
        .unwrap();
        assert_eq!(last_rate_limits(&f).unwrap()["primary"].used, 5.0);
        fs::write(&f, "{\"type\":\"x\"}\n").unwrap();
        assert!(last_rate_limits(&f).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_window_starts_over() {
        let w = Win {
            used: 80.0,
            resets_at: Some(1000),
            secs: 18000,
        };
        assert_eq!(w.at(2000).used, 0.0);
        assert_eq!(w.at(500).used, 80.0);
    }

    #[test]
    fn backoff_grows_and_honours_retry_after() {
        assert_eq!(backoff_ms(1, None), 5 * 60_000);
        assert_eq!(backoff_ms(2, None), 10 * 60_000);
        assert_eq!(backoff_ms(20, None), BACKOFF_MAX_MS);
        assert_eq!(backoff_ms(1, Some(1800)), 30 * 60_000);
        assert_eq!(backoff_ms(1, Some(999_999)), BACKOFF_MAX_MS);
    }

    #[test]
    fn endpoint_is_rarely_due() {
        let now = 10_000_000;
        let snap = |age: u64| Snap {
            at: now - age,
            ..Default::default()
        };
        let api = Api::default();
        assert!(
            !api_due(&snap(60_000), &api, now, false),
            "fresh local numbers: no call"
        );
        assert!(
            api_due(&snap(FRESH_MS + 1), &api, now, false),
            "old numbers: one call"
        );
        assert!(
            !api_due(
                &snap(FRESH_MS + 1),
                &Api {
                    last: now - 60_000,
                    ..Default::default()
                },
                now,
                false
            ),
            "not twice in 10 minutes"
        );
        assert!(
            !api_due(
                &snap(FRESH_MS + 1),
                &Api {
                    until: now + 1,
                    ..Default::default()
                },
                now,
                true
            ),
            "never during a back-off"
        );
        assert!(
            api_due(&snap(60_000), &api, now, true),
            "a press may ask early"
        );
        assert!(
            !api_due(
                &snap(60_000),
                &Api {
                    last: now - 5_000,
                    ..Default::default()
                },
                now,
                true
            ),
            "but not in a loop"
        );
    }

    #[test]
    fn record_keeps_newer() {
        let dir = std::env::temp_dir().join(format!("kd-limits-rec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let w = |u: f64| {
            BTreeMap::from([(
                "5h".to_string(),
                Win {
                    used: u,
                    resets_at: None,
                    secs: 18000,
                },
            )])
        };
        assert!(record(&dir, Provider::Claude, w(10.0), 100_000, "a"));
        assert!(
            !record(&dir, Provider::Claude, w(50.0), 50_000, "b"),
            "older numbers lose"
        );
        assert!(
            !record(&dir, Provider::Claude, w(10.0), 110_000, "c"),
            "same numbers: no rewrite"
        );
        assert!(record(&dir, Provider::Claude, w(12.0), 120_000, "d"));
        assert_eq!(read(&dir, Provider::Claude).wins["5h"].used, 12.0);
        let _ = fs::remove_dir_all(&dir);
    }
}
