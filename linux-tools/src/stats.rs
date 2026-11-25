//! CPU, GPU and memory use from the kernel (`/proc`, DRM sysfs), plus NVIDIA's own tool.

use base64::Engine;
use knobdeck_sdk::{Error, Result};
use std::sync::Mutex;

/// The last CPU sample: (busy, total) jiffies.
static LAST_CPU: Mutex<Option<(u64, u64)>> = Mutex::new(None);

fn cpu_sample() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let nums: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|n| n.parse().ok()).collect();
    // user nice system idle iowait irq softirq steal (guest time is already in user/nice).
    let total: u64 = nums.iter().take(8).sum();
    let idle = nums.get(3)? + nums.get(4).unwrap_or(&0);
    Some((total - idle, total))
}

/// CPU use since the previous call (the first call only primes the sample).
fn cpu() -> Option<f64> {
    let now = cpu_sample()?;
    let prev = LAST_CPU.lock().unwrap().replace(now)?;
    let (busy, total) = (now.0.saturating_sub(prev.0), now.1.saturating_sub(prev.1));
    (total > 0).then(|| busy as f64 * 100.0 / total as f64)
}

fn memory() -> Option<f64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |k: &str| info.lines().find_map(|l| l.strip_prefix(k)?.split_whitespace().next()?.parse::<f64>().ok());
    let (total, avail) = (field("MemTotal:")?, field("MemAvailable:")?);
    (total > 0.0).then(|| (total - avail) * 100.0 / total)
}

/// One GPU: a stable id (`drm:card1`, `nvidia:0`), a display name and its use.
pub struct Gpu {
    pub id: String,
    pub name: String,
    pub busy: f64,
}

fn vendor_name(vendor: &str) -> &'static str {
    match vendor {
        "0x1002" => "AMD GPU",
        "0x8086" => "Intel GPU",
        _ => "GPU",
    }
}

/// Every GPU: AMD and Intel (xe) report `gpu_busy_percent` in sysfs; NVIDIA through nvidia-smi.
pub fn gpus() -> Vec<Gpu> {
    let mut out = Vec::new();
    let mut cards: Vec<_> = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("card") && !e.file_name().to_string_lossy().contains('-'))
        .collect();
    cards.sort_by_key(|e| e.file_name());
    for e in cards {
        let dev = e.path().join("device");
        let Some(busy) = std::fs::read_to_string(dev.join("gpu_busy_percent")).ok().and_then(|v| v.trim().parse::<f64>().ok()) else { continue };
        let card = e.file_name().to_string_lossy().into_owned();
        let name = std::fs::read_to_string(dev.join("product_name"))
            .ok()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| vendor_name(std::fs::read_to_string(dev.join("vendor")).unwrap_or_default().trim()).to_string());
        out.push(Gpu { id: format!("drm:{card}"), name: format!("{name} ({card})"), busy });
    }
    let smi = std::process::Command::new("nvidia-smi").args(["--query-gpu=index,name,utilization.gpu", "--format=csv,noheader,nounits"]).output();
    if let Ok(smi) = smi {
        for line in String::from_utf8_lossy(&smi.stdout).lines() {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            if let [index, name, busy] = f[..] {
                if let Ok(busy) = busy.parse::<f64>() {
                    out.push(Gpu { id: format!("nvidia:{index}"), name: name.to_string(), busy });
                }
            }
        }
    }
    out
}

/// Use of the chosen GPU (`gpu` = an id from [`gpus`]); empty or unknown means the busiest one.
fn gpu(choice: &str) -> Option<f64> {
    let all = gpus();
    all.iter().find(|g| g.id == choice).map(|g| g.busy).or_else(|| all.iter().map(|g| g.busy).reduce(f64::max))
}

/// (display name, percent) for `cpu`, `gpu` or `memory`; `None` while unavailable.
pub fn read(stat: &str, gpu_choice: &str) -> Result<(&'static str, Option<f64>)> {
    Ok(match stat {
        "gpu" => ("GPU", gpu(gpu_choice)),
        "memory" => ("Memory", memory()),
        "cpu" | "" => ("CPU", cpu()),
        other => return Err(Error::msg(format!("Unknown stat \"{other}\""))),
    })
}

/// A ring gauge as an SVG data URI (same look as Win Tools / Mac Tools).
pub fn gauge(pct: f64) -> String {
    let p = pct.clamp(0.0, 100.0).round();
    let color = if p >= 85.0 { "#ef4444" } else if p >= 60.0 { "#f59e0b" } else { "#22c55e" };
    let circ = 2.0 * std::f64::consts::PI * 40.0;
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><circle cx="50" cy="50" r="40" fill="none" stroke="#888888" stroke-opacity="0.3" stroke-width="10"/><circle cx="50" cy="50" r="40" fill="none" stroke="{color}" stroke-width="10" stroke-linecap="round" stroke-dasharray="{:.1} {circ:.1}" transform="rotate(-90 50 50)"/><text x="50" y="61" text-anchor="middle" font-family="sans-serif" font-size="30" font-weight="600" fill="#888888">{p:.0}</text></svg>"##,
        circ * p / 100.0
    );
    format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_stats() {
        let _ = read("cpu", "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let cpu = read("cpu", "").unwrap().1.unwrap();
        assert!((0.0..=100.0).contains(&cpu));
        let mem = read("memory", "").unwrap().1.unwrap();
        assert!((0.0..=100.0).contains(&mem));
        eprintln!("cpu={cpu:.1} mem={mem:.1} gpu={:?}", read("gpu", "").unwrap().1);
        assert!(read("disk", "").is_err());
        assert!(gauge(150.0).starts_with("data:image/svg+xml;base64,"));
    }
}
