//! What every backend reports: cameras and their controls, in one OS-independent shape, plus the
//! maths that turns a slider position (0–100 %) into a value the camera accepts.

/// A connected USB camera. `id` is stable across replugs of the same port (Linux: the USB bus
/// path; Windows: the device path), `name` is what the user knows it by.
#[derive(Clone, Debug, PartialEq)]
pub struct Camera {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// An integer between `min` and `max` in steps of `step`.
    Range { min: i64, max: i64, step: i64, default: i64 },
    /// On / off (`value` is 0 or 1).
    Bool { default: bool },
    /// One of a list of named options (`value` is the option's number). DirectShow has none.
    #[cfg_attr(windows, allow(dead_code))]
    Menu { items: Vec<(i64, String)>, default: i64 },
}

/// One control a camera really has, with its current value.
#[derive(Clone, Debug)]
pub struct Control {
    /// `brightness`, `auto-exposure`, … for the standard controls (the same on every OS);
    /// anything else a camera offers gets a key of its own (`v4l2-009a0918`).
    pub key: String,
    pub label: String,
    pub kind: Kind,
    pub value: i64,
    /// The camera ignores writes right now (a manual control while its auto mode is on).
    pub inactive: bool,
    /// Shown after the value ("K", "°").
    pub unit: &'static str,
}

/// The standard controls that get an action of their own: (key, label, icon, unit).
pub const SLIDERS: &[(&str, &str, &str, &str)] = &[
    ("brightness", "Brightness", "sun", ""),
    ("contrast", "Contrast", "contrast", ""),
    ("saturation", "Saturation", "droplet", ""),
    ("hue", "Hue", "palette", ""),
    ("sharpness", "Sharpness", "triangle", ""),
    ("gamma", "Gamma", "sun-dim", ""),
    ("gain", "Gain", "gauge", ""),
    ("backlight", "Backlight Compensation", "sun-medium", ""),
    ("white-balance", "White Balance", "thermometer", "K"),
    ("exposure", "Exposure", "aperture", ""),
    ("focus", "Focus", "focus", ""),
    ("zoom", "Zoom", "zoom-in", ""),
    ("pan", "Pan", "move-horizontal", "°"),
    ("tilt", "Tilt", "move-vertical", "°"),
    ("roll", "Roll", "rotate-cw", "°"),
    ("iris", "Iris", "circle-dot", ""),
];

/// The on/off controls that get an action of their own: (key, label).
pub const TOGGLES: &[(&str, &str)] = &[("auto-exposure", "Auto Exposure"), ("auto-white-balance", "Auto White Balance"), ("autofocus", "Autofocus"), ("auto-hue", "Auto Hue")];

/// The auto mode that makes a manual control inactive.
pub fn master_of(key: &str) -> Option<&'static str> {
    Some(match key {
        "exposure" => "auto-exposure",
        "white-balance" | "v4l2-0098090e" | "v4l2-0098090f" => "auto-white-balance", // temperature, red / blue balance
        "focus" => "autofocus",
        "hue" => "auto-hue",
        "gain" => "auto-gain",
        _ => return None,
    })
}

/// Name of a standard control for messages ("This camera has no Zoom").
pub fn label_of(key: &str) -> String {
    if key == "auto-gain" {
        return "Auto Gain".into();
    }
    SLIDERS.iter().find(|s| s.0 == key).map(|s| s.1).or_else(|| TOGGLES.iter().find(|t| t.0 == key).map(|t| t.1)).unwrap_or(key).to_string()
}

pub fn find<'a>(list: &'a [Control], key: &str) -> Option<&'a Control> {
    list.iter().find(|c| c.key == key)
}

impl Control {
    /// Where a range control sits on a 0–100 slider.
    pub fn percent(&self) -> i64 {
        match self.kind {
            Kind::Range { min, max, .. } if max > min => (((self.value - min) as f64 / (max - min) as f64) * 100.0).round().clamp(0.0, 100.0) as i64,
            Kind::Bool { .. } => self.value.clamp(0, 1) * 100,
            _ => 0,
        }
    }

    /// The value as the user reads it: "4600 K", "On", "50 Hz".
    pub fn show(&self) -> String {
        match &self.kind {
            Kind::Range { .. } if self.unit.is_empty() => self.value.to_string(),
            Kind::Range { .. } if self.unit == "°" => format!("{}°", self.value),
            Kind::Range { .. } => format!("{} {}", self.value, self.unit),
            Kind::Bool { .. } => (if self.value != 0 { "On" } else { "Off" }).into(),
            Kind::Menu { items, .. } => items.iter().find(|i| i.0 == self.value).map_or_else(|| self.value.to_string(), |i| i.1.clone()),
        }
    }

    /// The value a "reset" writes; `None` when the camera's default isn't one of its own options
    /// (a driver reporting a default its menu doesn't list).
    pub fn default_value(&self) -> Option<i64> {
        match &self.kind {
            Kind::Range { default, .. } => Some(*default),
            Kind::Menu { default, items } => items.iter().any(|i| i.0 == *default).then_some(*default),
            Kind::Bool { default } => Some(*default as i64),
        }
    }

    /// The value a slider move asks for: an absolute position (0–100 %) or `delta` steps of
    /// `step_pct` % of the range, never less than one step of the camera's own. Snapped to the
    /// camera's step grid and kept inside its range.
    pub fn target(&self, absolute: Option<f64>, delta: Option<f64>, step_pct: f64) -> Option<i64> {
        let Kind::Range { min, max, step, .. } = self.kind else { return None };
        let raw = match absolute {
            Some(p) => min as f64 + p.clamp(0.0, 100.0) / 100.0 * (max - min) as f64,
            None => {
                let unit = ((max - min) as f64 * step_pct / 100.0).max(step as f64);
                self.value as f64 + unit * delta.unwrap_or(0.0)
            }
        };
        Some(snap(min, max, step, raw))
    }
}

/// `raw` moved onto the camera's grid (`min + n * step`) and into `min..=max`.
pub fn snap(min: i64, max: i64, step: i64, raw: f64) -> i64 {
    let step = step.max(1);
    let n = ((raw - min as f64) / step as f64).round() as i64;
    (min + n * step).clamp(min, max.max(min))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(min: i64, max: i64, step: i64, value: i64) -> Control {
        Control { key: "zoom".into(), label: "Zoom".into(), kind: Kind::Range { min, max, step, default: min }, value, inactive: false, unit: "" }
    }

    #[test]
    fn percent_maps_the_range() {
        assert_eq!(range(0, 12, 1, 6).percent(), 50);
        assert_eq!(range(2000, 10000, 100, 4600).percent(), 33);
        assert_eq!(range(5, 5, 1, 5).percent(), 0);
    }

    #[test]
    fn absolute_snaps_to_the_grid() {
        let c = range(2000, 10000, 100, 4600);
        assert_eq!(c.target(Some(0.0), None, 5.0), Some(2000));
        assert_eq!(c.target(Some(100.0), None, 5.0), Some(10000));
        assert_eq!(c.target(Some(33.0), None, 5.0), Some(4600));
    }

    #[test]
    fn a_turn_is_at_least_one_native_step() {
        // zoom 0–12: 5 % of the range is 0.6, a turn still moves one step.
        let c = range(0, 12, 1, 3);
        assert_eq!(c.target(None, Some(1.0), 5.0), Some(4));
        assert_eq!(c.target(None, Some(-1.0), 5.0), Some(2));
        assert_eq!(c.target(None, Some(-10.0), 5.0), Some(0));
        assert_eq!(c.target(None, Some(100.0), 5.0), Some(12));
    }

    #[test]
    fn show_reads_naturally() {
        let mut c = range(0, 12, 1, 4);
        c.unit = "K";
        assert_eq!(c.show(), "4 K");
        let m = Control { key: "p".into(), label: "P".into(), kind: Kind::Menu { items: vec![(1, "50 Hz".into()), (2, "60 Hz".into())], default: 1 }, value: 2, inactive: false, unit: "" };
        assert_eq!(m.show(), "60 Hz");
    }
}
