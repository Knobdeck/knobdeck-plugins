//! Windows backend: DirectShow's camera interfaces, `IAMVideoProcAmp` (image: brightness, contrast,
//! hue, saturation, sharpness, gamma, white balance, backlight, gain) and `IAMCameraControl` (pan,
//! tilt, roll, zoom, exposure, iris, focus). Each property reports a range and whether it has an
//! auto mode; a camera answers only for what its UVC descriptor advertises. These are the
//! interfaces every webcam app (OBS, Teams, the Camera app's pro mode) reaches the driver through.

use crate::model::{label_of, Camera, Control, Kind};
use windows::core::{w, Interface, BSTR, GUID, VARIANT};
use windows::Win32::Media::DirectShow::{IAMCameraControl, IAMVideoProcAmp, IBaseFilter, ICreateDevEnum};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, IBindCtx, IEnumMoniker, IErrorLog, IMoniker, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

const CLSID_SYSTEM_DEVICE_ENUM: GUID = GUID::from_u128(0x62be5d10_60eb_11d0_bd3b_00a0c911ce86);
const CLSID_VIDEO_INPUT_DEVICE_CATEGORY: GUID = GUID::from_u128(0x860bb310_5d01_11d0_bd3b_00a0c911ce86);

const AUTO: i32 = 1;
const MANUAL: i32 = 2;

/// One DirectShow property: which interface, its number, the plugin's key, the key of its auto
/// switch (a flag on the same property), and a unit.
struct Prop {
    camera_control: bool,
    id: i32,
    key: &'static str,
    auto_key: Option<&'static str>,
    unit: &'static str,
}

const fn vpa(id: i32, key: &'static str, auto_key: Option<&'static str>, unit: &'static str) -> Prop {
    Prop { camera_control: false, id, key, auto_key, unit }
}
const fn cc(id: i32, key: &'static str, auto_key: Option<&'static str>, unit: &'static str) -> Prop {
    Prop { camera_control: true, id, key, auto_key, unit }
}

const PROPS: &[Prop] = &[
    vpa(0, "brightness", None, ""),
    vpa(1, "contrast", None, ""),
    vpa(2, "hue", Some("auto-hue"), ""),
    vpa(3, "saturation", None, ""),
    vpa(4, "sharpness", None, ""),
    vpa(5, "gamma", None, ""),
    vpa(7, "white-balance", Some("auto-white-balance"), "K"),
    vpa(8, "backlight", None, ""),
    vpa(9, "gain", Some("auto-gain"), ""),
    cc(0, "pan", None, "°"),
    cc(1, "tilt", None, "°"),
    cc(2, "roll", None, "°"),
    cc(3, "zoom", None, ""),
    cc(4, "exposure", Some("auto-exposure"), ""),
    cc(5, "iris", None, ""),
    cc(6, "focus", Some("autofocus"), ""),
];

/// COM for this thread while a call runs.
struct Com(bool);

impl Com {
    fn new() -> Com {
        // SAFETY: balanced with `CoUninitialize` in `Drop` when this call initialised the thread.
        Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: matches the successful `CoInitializeEx` above.
            unsafe { CoUninitialize() }
        }
    }
}

fn read_text(bag: &IPropertyBag, name: windows::core::PCWSTR) -> Option<String> {
    let mut v = VARIANT::default();
    // SAFETY: `v` is a valid VARIANT the bag fills in; it is dropped (cleared) with this scope.
    unsafe { bag.Read(name, &mut v, None::<&IErrorLog>) }.ok()?;
    BSTR::try_from(&v).ok().map(|b| b.to_string())
}

/// Every USB video input: the camera and the moniker that opens it.
fn devices() -> Vec<(Camera, IMoniker)> {
    let mut out = Vec::new();
    // SAFETY: plain COM calls on interfaces this function owns.
    unsafe {
        let Ok(en) = CoCreateInstance::<_, ICreateDevEnum>(&CLSID_SYSTEM_DEVICE_ENUM, None, CLSCTX_INPROC_SERVER) else { return out };
        let mut e: Option<IEnumMoniker> = None;
        if en.CreateClassEnumerator(&CLSID_VIDEO_INPUT_DEVICE_CATEGORY, &mut e, 0).is_err() {
            return out;
        }
        let Some(e) = e else { return out }; // no cameras: S_FALSE and no enumerator
        loop {
            let mut m: [Option<IMoniker>; 1] = [None];
            let mut got = 0u32;
            if e.Next(&mut m, Some(&mut got)).is_err() || got == 0 {
                break;
            }
            let Some(mk) = m[0].take() else { break };
            let Ok(bag) = mk.BindToStorage::<_, _, IPropertyBag>(None::<&IBindCtx>, None::<&IMoniker>) else { continue };
            let (Some(path), Some(name)) = (read_text(&bag, w!("DevicePath")), read_text(&bag, w!("FriendlyName"))) else { continue };
            // Only USB cameras (the device path of a UVC camera is `\\?\usb#vid_…`); this hides
            // virtual cameras such as OBS's.
            if path.to_ascii_lowercase().contains("usb#") {
                out.push((Camera { id: path.to_ascii_lowercase(), name }, mk));
            }
        }
    }
    // Two cameras of one model: tell them apart by a short tail of their path.
    let names: Vec<String> = out.iter().map(|c| c.0.name.clone()).collect();
    for c in &mut out {
        if names.iter().filter(|n| **n == c.0.name).count() > 1 {
            let tail: String = c.0.id.chars().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
            c.0.name = format!("{} ({tail})", c.0.name);
        }
    }
    out
}

pub fn cameras() -> Vec<Camera> {
    let _com = Com::new();
    devices().into_iter().map(|d| d.0).collect()
}

struct Filters {
    cc: Option<IAMCameraControl>,
    vpa: Option<IAMVideoProcAmp>,
}

impl Filters {
    fn open(cam: &str) -> Result<Filters, String> {
        let mk = devices().into_iter().find(|d| d.0.id == cam).ok_or("That camera isn't connected")?.1;
        // SAFETY: binds the camera's capture filter; the interfaces keep it alive.
        let filter: IBaseFilter = unsafe { mk.BindToObject::<_, _, IBaseFilter>(None::<&IBindCtx>, None::<&IMoniker>) }.map_err(|_| "The camera is busy or was unplugged")?;
        Ok(Filters { cc: filter.cast().ok(), vpa: filter.cast().ok() })
    }

    /// (min, max, step, default, capability flags), or None when the camera lacks the property.
    fn range(&self, p: &Prop) -> Option<(i32, i32, i32, i32, i32)> {
        let (mut min, mut max, mut step, mut def, mut caps) = (0, 0, 0, 0, 0);
        // SAFETY: out-pointers to locals.
        let r = unsafe {
            if p.camera_control {
                self.cc.as_ref()?.GetRange(p.id, &mut min, &mut max, &mut step, &mut def, &mut caps)
            } else {
                self.vpa.as_ref()?.GetRange(p.id, &mut min, &mut max, &mut step, &mut def, &mut caps)
            }
        };
        r.ok().map(|_| (min, max, step, def, caps))
    }

    /// (value, flags)
    fn get(&self, p: &Prop) -> Option<(i32, i32)> {
        let (mut v, mut f) = (0, 0);
        // SAFETY: out-pointers to locals.
        let r = unsafe {
            if p.camera_control {
                self.cc.as_ref()?.Get(p.id, &mut v, &mut f)
            } else {
                self.vpa.as_ref()?.Get(p.id, &mut v, &mut f)
            }
        };
        r.ok().map(|_| (v, f))
    }

    fn put(&self, p: &Prop, value: i32, flags: i32) -> Result<(), String> {
        // SAFETY: plain value arguments.
        let r = unsafe {
            if p.camera_control {
                self.cc.as_ref().ok_or("This camera has no camera controls")?.Set(p.id, value, flags)
            } else {
                self.vpa.as_ref().ok_or("This camera has no image controls")?.Set(p.id, value, flags)
            }
        };
        r.map_err(|e| format!("The camera refused the change: {}", e.message()))
    }
}

/// The controls one property gives: the range, and its auto switch when it has an auto mode.
fn build(f: &Filters, p: &Prop) -> Vec<Control> {
    let (Some((min, max, step, def, caps)), Some((value, flags))) = (f.range(p), f.get(p)) else { return Vec::new() };
    let auto_on = caps & AUTO != 0 && flags & AUTO != 0;
    let mut out = vec![Control { key: p.key.into(), label: label_of(p.key), kind: Kind::Range { min: min as i64, max: max as i64, step: (step as i64).max(1), default: def as i64 }, value: value as i64, inactive: auto_on, unit: p.unit }];
    if let (Some(key), true) = (p.auto_key, caps & AUTO != 0) {
        out.push(Control { key: key.into(), label: label_of(key), kind: Kind::Bool { default: true }, value: auto_on as i64, inactive: false, unit: "" });
    }
    out
}

pub fn controls(cam: &str) -> Result<Vec<Control>, String> {
    let _com = Com::new();
    let f = Filters::open(cam)?;
    Ok(PROPS.iter().flat_map(|p| build(&f, p)).collect())
}

/// Write `value` to the control `key`; returns the value the camera now reports.
pub fn set(cam: &str, key: &str, value: i64) -> Result<i64, String> {
    let _com = Com::new();
    let f = Filters::open(cam)?;
    let (p, is_auto) = PROPS.iter().find_map(|p| if p.key == key { Some((p, false)) } else { (p.auto_key == Some(key)).then_some((p, true)) }).ok_or("This camera doesn't have that control")?;
    let (current, _) = f.get(p).ok_or("This camera doesn't have that control")?;
    if is_auto {
        f.put(p, current, if value != 0 { AUTO } else { MANUAL })?;
        Ok(f.get(p).map_or(value, |(_, flags)| (flags & AUTO != 0) as i64))
    } else {
        f.put(p, value as i32, MANUAL)?;
        Ok(f.get(p).map_or(value, |(v, _)| v as i64))
    }
}
