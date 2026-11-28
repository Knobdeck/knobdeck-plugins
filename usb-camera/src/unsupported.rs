//! Platforms without a backend yet (macOS needs raw UVC requests through IOKit).

use crate::model::{Camera, Control};

pub fn cameras() -> Vec<Camera> {
    Vec::new()
}

pub fn controls(_cam: &str) -> Result<Vec<Control>, String> {
    Err("USB Camera isn't available on this system yet".into())
}

pub fn set(_cam: &str, _key: &str, _value: i64) -> Result<i64, String> {
    Err("USB Camera isn't available on this system yet".into())
}
