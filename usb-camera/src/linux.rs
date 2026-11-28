//! Linux backend: V4L2 (the kernel's video API; the `uvcvideo` driver turns each UVC control the
//! camera advertises into a V4L2 control). Controls are enumerated from the driver, so a camera
//! shows exactly what it supports. Opening a node for control ioctls never touches a running
//! stream, so OBS or a meeting app can keep using the camera.

use crate::model::{Camera, Control, Kind};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

// ---- kernel ABI (linux/videodev2.h) ----

const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr
}
const IOC_RW: u32 = 3;
const IOC_R: u32 = 2;

#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct Ctrl {
    id: u32,
    value: i32,
}

#[repr(C)]
struct QueryExtCtrl {
    id: u32,
    kind: u32,
    name: [u8; 32],
    minimum: i64,
    maximum: i64,
    step: u64,
    default_value: i64,
    flags: u32,
    elem_size: u32,
    elems: u32,
    nr_of_dims: u32,
    dims: [u32; 4],
    reserved: [u32; 32],
}

#[repr(C, packed)]
struct QueryMenu {
    id: u32,
    index: u32,
    name: [u8; 32], // a NUL-terminated name, or an i64 for integer menus
    reserved: u32,
}

const VIDIOC_QUERYCAP: u32 = ioc(IOC_R, 0, std::mem::size_of::<Capability>());
const VIDIOC_G_CTRL: u32 = ioc(IOC_RW, 27, std::mem::size_of::<Ctrl>());
const VIDIOC_S_CTRL: u32 = ioc(IOC_RW, 28, std::mem::size_of::<Ctrl>());
const VIDIOC_QUERYMENU: u32 = ioc(IOC_RW, 37, std::mem::size_of::<QueryMenu>());
const VIDIOC_QUERY_EXT_CTRL: u32 = ioc(IOC_RW, 103, std::mem::size_of::<QueryExtCtrl>());

const CAP_VIDEO_CAPTURE: u32 = 0x1;
const CAP_VIDEO_CAPTURE_MPLANE: u32 = 0x1000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;

const NEXT_CTRL: u32 = 0x8000_0000;
const TYPE_INTEGER: u32 = 1;
const TYPE_BOOLEAN: u32 = 2;
const TYPE_MENU: u32 = 3;
const TYPE_INTEGER_MENU: u32 = 9;
const FLAG_DISABLED: u32 = 0x1;
const FLAG_READ_ONLY: u32 = 0x4;
const FLAG_INACTIVE: u32 = 0x10;
const FLAG_WRITE_ONLY: u32 = 0x40;

/// V4L2 pan / tilt are arc seconds; the plugin speaks degrees.
const ARCSEC: i64 = 3600;

/// The standard controls: (V4L2 id, key, label, unit). Everything else the driver lists is shown
/// under its own name.
const KNOWN: &[(u32, &str, &str, &str)] = &[
    (0x0098_0900, "brightness", "Brightness", ""),
    (0x0098_0901, "contrast", "Contrast", ""),
    (0x0098_0902, "saturation", "Saturation", ""),
    (0x0098_0903, "hue", "Hue", ""),
    (0x0098_090c, "auto-white-balance", "Auto White Balance", ""),
    (0x0098_0910, "gamma", "Gamma", ""),
    (0x0098_0912, "auto-gain", "Auto Gain", ""),
    (0x0098_0913, "gain", "Gain", ""),
    (0x0098_0918, "power-line", "Power Line Frequency", ""),
    (0x0098_0919, "auto-hue", "Auto Hue", ""),
    (0x0098_091a, "white-balance", "White Balance", "K"),
    (0x0098_091b, "sharpness", "Sharpness", ""),
    (0x0098_091c, "backlight", "Backlight Compensation", ""),
    (0x009a_0901, "auto-exposure", "Auto Exposure", ""),
    (0x009a_0902, "exposure", "Exposure", ""),
    (0x009a_0908, "pan", "Pan", "°"),
    (0x009a_0909, "tilt", "Tilt", "°"),
    (0x009a_090a, "focus", "Focus", ""),
    (0x009a_090c, "autofocus", "Autofocus", ""),
    (0x009a_090d, "zoom", "Zoom", ""),
    (0x009a_0910, "iris", "Iris", ""),
];

/// UVC "relative" and "speed" controls start a movement that stays on until told to stop (a
/// reading is meaningless), so a slider or a key can't represent them. The absolute controls cover
/// pan, tilt, zoom, focus and iris.
const MOVEMENT: &[u32] = &[0x009a_0904, 0x009a_0905, 0x009a_090b, 0x009a_090e, 0x009a_090f, 0x009a_0911, 0x009a_0920, 0x009a_0921];

/// V4L2's exposure mode: 1 = manual, anything else is some form of automatic.
const EXPOSURE_MANUAL: i64 = 1;

// ---- helpers ----

fn ctl<T>(f: &File, req: u32, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: `arg` points to a live, correctly sized struct for `req`.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, arg as *mut T) };
        if r >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

fn zeroed<T>() -> T {
    // SAFETY: only used for plain-old-data kernel structs, where all-zero is a valid value.
    unsafe { std::mem::zeroed() }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(&b[..b.iter().position(|c| *c == 0).unwrap_or(b.len())]).trim().to_string()
}

fn open(path: &str) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(path)
}

fn explain(e: io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => "No permission to use the camera: add your user to the \"video\" group and log in again".into(),
        Some(libc::ENODEV | libc::ENOENT | libc::EIO) => "The camera was unplugged".into(),
        Some(libc::EBUSY) => "The camera is busy right now".into(),
        _ => format!("Camera error: {e}"),
    }
}

struct Node {
    path: String,
    card: String,
    bus: String,
}

/// Every video node that captures video, in numeric order. (A camera also has metadata nodes;
/// they don't advertise capture.)
fn nodes() -> Vec<Node> {
    let mut found: Vec<(u32, String)> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n = name.strip_prefix("video")?.parse::<u32>().ok()?;
            Some((n, format!("/dev/{name}")))
        })
        .collect();
    found.sort();
    found
        .into_iter()
        .filter_map(|(_, path)| {
            let f = open(&path).ok()?;
            let mut cap: Capability = zeroed();
            ctl(&f, VIDIOC_QUERYCAP, &mut cap).ok()?;
            let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 { cap.device_caps } else { cap.capabilities };
            (caps & (CAP_VIDEO_CAPTURE | CAP_VIDEO_CAPTURE_MPLANE) != 0).then(|| Node { path, card: text(&cap.card), bus: text(&cap.bus_info) })
        })
        .collect()
}

/// "OBSBOT Meet SE: OBSBOT Meet SE" (the driver repeats the name) becomes "OBSBOT Meet SE".
fn tidy(card: &str) -> String {
    match card.split_once(": ") {
        Some((a, b)) if a == b => a.to_string(),
        _ => card.to_string(),
    }
}

pub fn cameras() -> Vec<Camera> {
    let mut out: Vec<Camera> = Vec::new();
    for n in nodes() {
        if n.bus.starts_with("usb-") && !out.iter().any(|c| c.id == n.bus) {
            out.push(Camera { id: n.bus, name: tidy(&n.card) });
        }
    }
    // Two cameras of one model: tell them apart by port.
    let names: Vec<String> = out.iter().map(|c| c.name.clone()).collect();
    for c in &mut out {
        if names.iter().filter(|n| **n == c.name).count() > 1 {
            c.name = format!("{} ({})", c.name, c.id.trim_start_matches("usb-"));
        }
    }
    out
}

fn open_camera(id: &str) -> Result<File, String> {
    let node = nodes().into_iter().find(|n| n.bus == id).ok_or("That camera isn't connected")?;
    open(&node.path).map_err(explain)
}

// ---- controls ----

struct Raw {
    id: u32,
    kind: u32,
    name: String,
    min: i64,
    max: i64,
    step: i64,
    default: i64,
    flags: u32,
    items: Vec<(i64, String)>,
}

fn enumerate(f: &File) -> Vec<Raw> {
    let mut out = Vec::new();
    let mut next = NEXT_CTRL;
    loop {
        let mut q: QueryExtCtrl = zeroed();
        q.id = next;
        if ctl(f, VIDIOC_QUERY_EXT_CTRL, &mut q).is_err() {
            break;
        }
        next = q.id | NEXT_CTRL;
        let (kind, flags) = (q.kind, q.flags);
        if flags & (FLAG_DISABLED | FLAG_READ_ONLY) != 0 || !matches!(kind, TYPE_INTEGER | TYPE_BOOLEAN | TYPE_MENU | TYPE_INTEGER_MENU) || MOVEMENT.contains(&q.id) {
            continue;
        }
        let mut items = Vec::new();
        if matches!(kind, TYPE_MENU | TYPE_INTEGER_MENU) {
            for i in q.minimum..=q.maximum {
                let mut m = QueryMenu { id: q.id, index: i as u32, name: [0; 32], reserved: 0 };
                if ctl(f, VIDIOC_QUERYMENU, &mut m).is_ok() {
                    let name = m.name;
                    let label = if kind == TYPE_MENU { text(&name) } else { i64::from_ne_bytes(name[..8].try_into().unwrap()).to_string() };
                    items.push((i, label));
                }
            }
        }
        out.push(Raw { id: q.id, kind, name: text(&q.name), min: q.minimum, max: q.maximum, step: q.step as i64, default: q.default_value, flags, items });
    }
    out
}

fn read(f: &File, id: u32) -> Option<i64> {
    let mut c = Ctrl { id, value: 0 };
    ctl(f, VIDIOC_G_CTRL, &mut c).ok().map(|_| c.value as i64)
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// How a raw control looks to the plugin: its control, and the divisor between the plugin's value
/// and the kernel's (3600 for pan / tilt).
fn build(f: &File, r: &Raw) -> Option<(Control, i64)> {
    let known = KNOWN.iter().find(|k| k.0 == r.id);
    let (key, label, unit) = match known {
        Some(k) => (k.1.to_string(), k.2.to_string(), k.3),
        None => (format!("v4l2-{:08x}", r.id), capitalise(&r.name), ""),
    };
    let div = if matches!(key.as_str(), "pan" | "tilt") { ARCSEC } else { 1 };
    let raw = read(f, r.id).or_else(|| (r.flags & FLAG_WRITE_ONLY != 0).then_some(r.default))?;
    let kind;
    let value;
    if key == "auto-exposure" {
        // The kernel's menu (auto / manual / shutter or aperture priority) is an on/off switch here.
        kind = Kind::Bool { default: r.default != EXPOSURE_MANUAL };
        value = (raw != EXPOSURE_MANUAL) as i64;
    } else {
        match r.kind {
            TYPE_BOOLEAN => {
                kind = Kind::Bool { default: r.default != 0 };
                value = raw.clamp(0, 1);
            }
            TYPE_MENU | TYPE_INTEGER_MENU => {
                kind = Kind::Menu { items: r.items.clone(), default: r.default };
                value = raw;
            }
            _ => {
                kind = Kind::Range { min: r.min / div, max: r.max / div, step: (r.step / div).max(1), default: r.default / div };
                value = raw / div;
            }
        }
    }
    Some((Control { key, label, kind, value, inactive: r.flags & FLAG_INACTIVE != 0, unit }, div))
}

pub fn controls(cam: &str) -> Result<Vec<Control>, String> {
    let f = open_camera(cam)?;
    Ok(enumerate(&f).iter().filter_map(|r| build(&f, r)).map(|c| c.0).collect())
}

/// Write `value` to the control `key`; returns the value the camera now reports.
pub fn set(cam: &str, key: &str, value: i64) -> Result<i64, String> {
    let f = open_camera(cam)?;
    let raws = enumerate(&f);
    let (raw, (control, div)) = raws.iter().find_map(|r| build(&f, r).filter(|(c, _)| c.key == key).map(|c| (r, c))).ok_or("This camera doesn't have that control")?;
    let kernel = if key == "auto-exposure" {
        if value == 0 {
            EXPOSURE_MANUAL
        } else {
            // Back to automatic: the camera's own default mode, or the first automatic one.
            if raw.default != EXPOSURE_MANUAL { raw.default } else { raw.items.iter().map(|i| i.0).find(|i| *i != EXPOSURE_MANUAL).ok_or("This camera has no automatic exposure")? }
        }
    } else {
        match control.kind {
            Kind::Bool { .. } => value.clamp(0, 1),
            _ => value * div,
        }
    };
    let mut c = Ctrl { id: raw.id, value: kernel.clamp(i32::MIN as i64, i32::MAX as i64) as i32 };
    ctl(&f, VIDIOC_S_CTRL, &mut c).map_err(explain)?;
    Ok(build(&f, raw).map_or(value, |(c, _)| c.value))
}

#[cfg(test)]
mod abi {
    use super::*;
    #[test]
    fn kernel_struct_sizes() {
        assert_eq!(std::mem::size_of::<Capability>(), 104);
        assert_eq!(std::mem::size_of::<Ctrl>(), 8);
        assert_eq!(std::mem::size_of::<QueryExtCtrl>(), 232);
        assert_eq!(std::mem::size_of::<QueryMenu>(), 44);
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_G_CTRL, 0xC008_561B);
        assert_eq!(VIDIOC_QUERY_EXT_CTRL, 0xC0E8_5667);
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Read-only: lists the connected cameras and their controls. `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn list_real_controls() {
        for cam in cameras() {
            println!("{} [{}]", cam.name, cam.id);
            for c in controls(&cam.id).unwrap() {
                println!("  {:<22} {:<24} {:?} = {}{}", c.key, c.label, c.kind, c.show(), if c.inactive { "  (inactive)" } else { "" });
            }
        }
    }
}

#[cfg(test)]
mod live_write {
    use super::*;

    /// Changes Sharpness by one step and puts it back, checking each read-back. Harmless to a
    /// running stream. `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn sharpness_round_trip() {
        let cam = &cameras()[0];
        let before = controls(&cam.id).unwrap().into_iter().find(|c| c.key == "sharpness").unwrap().value;
        let other = if before > 0 { before - 1 } else { before + 1 };
        let changed = set(&cam.id, "sharpness", other);
        let back = set(&cam.id, "sharpness", before); // always restore, then judge
        assert_eq!(changed, Ok(other));
        assert_eq!(back, Ok(before));
        println!("sharpness {before} -> {other} -> {before}");
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    /// Read-only timing of the operations a poll tick runs. `cargo test --release -- --ignored --nocapture bench`.
    #[test]
    #[ignore]
    fn bench_reads() {
        let cam = cameras().remove(0);
        let n = 300;
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(cameras());
        }
        let cams = t.elapsed() / n;
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(controls(&cam.id).unwrap());
        }
        let ctrls = t.elapsed() / n;
        println!("cameras():  {cams:?} each");
        println!("controls(): {ctrls:?} each (includes finding the node)");
        println!("one key's poll tick (cameras + controls): {:?}", cams + ctrls);
    }
}
