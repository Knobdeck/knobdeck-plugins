//! USB Camera plugin for KnobDeck: the controls a UVC webcam advertises (image, white balance,
//! exposure, focus, zoom, pan / tilt, …) as sliders, switches and buttons. Linux talks V4L2,
//! Windows DirectShow; each camera shows only the controls it really has.

// macOS has no backend yet (the stub reports no cameras), so the shared model is unused there.
#![cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]

mod model;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;
#[cfg(not(any(target_os = "linux", windows)))]
#[path = "unsupported.rs"]
mod os;
#[cfg(windows)]
#[path = "windows.rs"]
mod os;

use knobdeck_sdk::input::{self, InputExt};
use knobdeck_sdk::{json, Action, Ctx, Error, Plugin, Result, Value, Visual};
use model::{find, label_of, master_of, Camera, Control, Kind, SLIDERS, TOGGLES};
use std::time::Duration;

const FIRST: &str = "__first__";
const POLL: Duration = Duration::from_secs(2);

/// The camera an action targets: the one picked, or the first USB camera found.
fn pick(inputs: &Value) -> Result<Camera> {
    let want = inputs["camera"].as_str().unwrap_or(FIRST);
    let list = os::cameras();
    let found = if want.is_empty() || want == FIRST { list.into_iter().next() } else { list.into_iter().find(|c| c.id == want) };
    found.ok_or_else(|| Error::msg(if want.is_empty() || want == FIRST { "No USB camera found" } else { "That camera isn't connected" }))
}

fn controls_of(inputs: &Value) -> Result<(Camera, Vec<Control>)> {
    let cam = pick(inputs)?;
    let list = os::controls(&cam.id).map_err(Error::msg)?;
    Ok((cam, list))
}

fn need<'a>(list: &'a [Control], key: &str) -> Result<&'a Control> {
    find(list, key).ok_or_else(|| Error::msg(format!("This camera has no {}", label_of(key))))
}

/// A manual control does nothing while its auto mode is on; say so rather than switch it off.
fn ensure_active(list: &[Control], c: &Control) -> Result<()> {
    if !c.inactive {
        return Ok(());
    }
    match master_of(&c.key).and_then(|m| find(list, m)) {
        Some(m) => Err(Error::msg(format!("Turn off {} first", m.label))),
        None => Err(Error::msg(format!("{} can't be changed right now", c.label))),
    }
}

fn camera_input() -> Value {
    input::query_select("camera", "Camera", "get-cameras", &[]).with("default", FIRST)
}

fn step_input() -> Value {
    input::text("step", "Step (%)").placeholder("5").default_value("5")
}

fn step_of(ctx: &Ctx) -> f64 {
    ctx.input_f64("step").filter(|s| *s > 0.0).unwrap_or(5.0)
}

fn on_label(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// The key a generic action works on (picked from the camera's own controls).
fn picked(inputs: &Value) -> Option<String> {
    inputs["control"].as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// What a key of `action` shows for a control: a slider's position and value, a switch's state,
/// or an option's name.
fn visual_for(action: &str, c: &Control) -> Visual {
    let is_switch = action == "switch" || TOGGLES.iter().any(|t| t.0 == action);
    if action == "option" {
        Visual::new().value(c.show())
    } else if is_switch {
        Visual::new().state(on_label(c.value != 0))
    } else {
        Visual::new().state(c.percent()).value(c.show())
    }
}

/// After a write, bring every placed key that shows this camera in line with the camera, whichever
/// action it belongs to: two zoom sliders, a Camera Control slider on Zoom and a Set button all
/// change the same property, so they all follow. Matches on the camera (an unset camera means the
/// first one found) and, for the generic actions, on the control they were set to.
fn publish(ctx: &Ctx, cam: &Camera) {
    let Ok(list) = os::controls(&cam.id) else { return };
    let first = os::cameras().into_iter().next().map(|c| c.id);
    let rt = ctx.runtime();
    let actions = SLIDERS.iter().map(|s| s.0).chain(TOGGLES.iter().map(|t| t.0)).chain(["control", "switch", "option"]);
    for action in actions {
        for id in rt.instances_of(action) {
            let inputs = rt.inputs_of(&id);
            let target = match inputs["camera"].as_str() {
                Some(c) if !c.is_empty() && c != FIRST => Some(c.to_string()),
                _ => first.clone(),
            };
            if target.as_deref() != Some(cam.id.as_str()) {
                continue;
            }
            let key = match action {
                "control" | "switch" | "option" => picked(&inputs),
                std => Some(std.to_string()),
            };
            if let Some(c) = key.and_then(|k| find(&list, &k)) {
                rt.set_visual(&id, visual_for(action, c));
            }
        }
    }
}

fn live_range(inputs: &Value, key: Option<String>) -> Result<Visual> {
    let Some(key) = key else { return Ok(Visual::new().label("Pick a control")) };
    let (_, list) = controls_of(inputs)?;
    Ok(match find(&list, &key) {
        Some(c) => Visual::new().state(c.percent()).value(c.show()),
        None => Visual::new().label("Not supported"),
    })
}

fn adjust(ctx: &Ctx, key: &str) -> Result {
    let (cam, list) = controls_of(&ctx.inputs)?;
    let c = need(&list, key)?;
    ensure_active(&list, c)?;
    let Some(target) = c.target(ctx.value, ctx.delta, step_of(ctx)) else {
        return ctx.fail(format!("{} isn't a slider on this camera", c.label));
    };
    os::set(&cam.id, key, target).map_err(Error::msg)?;
    publish(ctx, &cam);
    Ok(())
}

fn live_switch(inputs: &Value, key: Option<String>) -> Result<Visual> {
    let Some(key) = key else { return Ok(Visual::new().label("Pick a control")) };
    let (_, list) = controls_of(inputs)?;
    Ok(match find(&list, &key) {
        Some(c) => Visual::new().state(on_label(c.value != 0)),
        None => Visual::new().label("Not supported"),
    })
}

fn flip(ctx: &Ctx, key: &str) -> Result {
    let (cam, list) = controls_of(&ctx.inputs)?;
    let c = need(&list, key)?;
    if !matches!(c.kind, Kind::Bool { .. }) {
        return ctx.fail(format!("{} isn't an on/off control", c.label));
    }
    os::set(&cam.id, key, (c.value == 0) as i64).map_err(Error::msg)?;
    publish(ctx, &cam);
    Ok(())
}

/// Options for a picker of one kind of control on the picked camera.
fn control_options(q_inputs: &Value, want: fn(&Kind) -> bool) -> Value {
    let list = pick(q_inputs).and_then(|c| os::controls(&c.id).map_err(Error::msg)).unwrap_or_default();
    Value::Array(
        list.iter()
            .filter(|c| want(&c.kind))
            .map(|c| {
                let label = match &c.kind {
                    Kind::Range { min, max, .. } => format!("{} ({min} to {max})", c.label),
                    _ => c.label.clone(),
                };
                json!({"value": c.key, "label": label})
            })
            .collect(),
    )
}

fn control_input(query: &str) -> Value {
    input::query_select("control", "Control", query, &["camera"])
}

/// Write the camera's factory value of one control.
fn reset_one(cam: &Camera, c: &Control) -> Result {
    match c.default_value() {
        Some(v) => os::set(&cam.id, &c.key, v).map(|_| ()).map_err(Error::msg),
        None => Ok(()), // no usable default to go back to
    }
}

fn main() {
    let mut plugin = Plugin::new(&format!("usb-camera-{}", std::env::consts::OS), "USB Camera")
        .version(env!("CARGO_PKG_VERSION"))
        .platforms(&[std::env::consts::OS]) // one package per OS
        .author("KnobDeck")
        .description("Webcam controls: brightness, contrast, white balance, exposure, focus, zoom, pan and tilt, for every control the camera supports.")
        .icon("app-icon.svg")
        .query("get-cameras", |_| {
            let mut opts = vec![json!({"value": FIRST, "label": "First camera found"})];
            opts.extend(os::cameras().into_iter().map(|c| json!({"value": c.id, "label": c.name})));
            Ok(Value::Array(opts))
        })
        .query("controls-range", |q| Ok(control_options(&q.inputs, |k| matches!(k, Kind::Range { .. }))))
        .query("controls-switch", |q| Ok(control_options(&q.inputs, |k| matches!(k, Kind::Bool { .. }))))
        .query("controls-menu", |q| Ok(control_options(&q.inputs, |k| matches!(k, Kind::Menu { .. }))))
        .query("controls-all", |q| Ok(control_options(&q.inputs, |_| true)));

    for &(key, label, icon, _) in SLIDERS {
        plugin = plugin.action(
            Action::slider(key, label, 0.0, 100.0)
                .icon(icon)
                .unit("%")
                .inputs(vec![camera_input(), step_input()])
                .live(POLL, move |ic| live_range(&ic.inputs, Some(key.to_string())))
                .on_adjust(move |ctx| adjust(ctx, key)),
        );
    }
    for &(key, label) in TOGGLES {
        plugin = plugin.action(
            Action::toggle(key, label)
                .icon("video")
                .inputs(vec![camera_input()])
                .state("off", "Off", "video-off")
                .state("on", "On", "video")
                .live(POLL, move |ic| live_switch(&ic.inputs, Some(key.to_string())))
                .on_press(move |ctx| flip(ctx, key)),
        );
    }

    plugin
        .action(
            Action::slider("control", "Camera Control", 0.0, 100.0)
                .icon("sliders-horizontal")
                .unit("%")
                .inputs(vec![camera_input(), control_input("controls-range"), step_input()])
                .live(POLL, |ic| live_range(&ic.inputs, picked(&ic.inputs)))
                .on_adjust(|ctx| match picked(&ctx.inputs) {
                    Some(key) => adjust(ctx, &key),
                    None => ctx.fail("Pick a control first"),
                }),
        )
        .action(
            Action::toggle("switch", "Camera Switch")
                .icon("toggle-right")
                .inputs(vec![camera_input(), control_input("controls-switch")])
                .state("off", "Off", "toggle-left")
                .state("on", "On", "toggle-right")
                .live(POLL, |ic| live_switch(&ic.inputs, picked(&ic.inputs)))
                .on_press(|ctx| match picked(&ctx.inputs) {
                    Some(key) => flip(ctx, &key),
                    None => ctx.fail("Pick a control first"),
                }),
        )
        .action(
            Action::button("option", "Camera Option")
                .icon("list")
                .inputs(vec![camera_input(), control_input("controls-menu"), input::select("direction", "Each press", &[("next", "Next option"), ("previous", "Previous option")]).default_value("next")])
                .live(POLL, |ic| {
                    let Some(key) = picked(&ic.inputs) else { return Ok(Visual::new().label("Pick a control")) };
                    let (_, list) = controls_of(&ic.inputs)?;
                    Ok(match find(&list, &key) {
                        Some(c) => Visual::new().value(c.show()),
                        None => Visual::new().label("Not supported"),
                    })
                })
                .on_press(|ctx| {
                    let Some(key) = picked(&ctx.inputs) else { return ctx.fail("Pick a control first") };
                    let (cam, list) = controls_of(&ctx.inputs)?;
                    let c = need(&list, &key)?;
                    ensure_active(&list, c)?;
                    let Kind::Menu { items, .. } = &c.kind else { return ctx.fail(format!("{} has no options to cycle", c.label)) };
                    let at = items.iter().position(|i| i.0 == c.value).unwrap_or(0);
                    let n = items.len();
                    let to = if ctx.input_str("direction") == "previous" { (at + n - 1) % n } else { (at + 1) % n };
                    os::set(&cam.id, &key, items[to].0).map_err(Error::msg)?;
                    publish(ctx, &cam);
                    Ok(())
                }),
        )
        .action(
            Action::button("set", "Set Camera Control")
                .icon("sliders-horizontal")
                .inputs(vec![camera_input(), control_input("controls-range"), input::text("value", "Position (%)").placeholder("50").default_value("50")])
                .on_press(|ctx| {
                    let Some(key) = picked(&ctx.inputs) else { return ctx.fail("Pick a control first") };
                    let Some(p) = ctx.input_f64("value").filter(|p| (0.0..=100.0).contains(p)) else { return ctx.fail("Position must be 0–100") };
                    let (cam, list) = controls_of(&ctx.inputs)?;
                    let c = need(&list, &key)?;
                    ensure_active(&list, c)?;
                    let Some(target) = c.target(Some(p), None, 5.0) else { return ctx.fail(format!("{} isn't a slider on this camera", c.label)) };
                    os::set(&cam.id, &key, target).map_err(Error::msg)?;
                    publish(ctx, &cam);
                    Ok(())
                }),
        )
        .action(
            Action::button("reset", "Reset Camera Control")
                .icon("rotate-ccw")
                .inputs(vec![camera_input(), control_input("controls-all").with("default", ""), input::checkbox("all", "Reset every control of the camera")])
                .on_press(|ctx| {
                    let (cam, list) = controls_of(&ctx.inputs)?;
                    if !ctx.input_bool("all") {
                        let Some(key) = picked(&ctx.inputs) else { return ctx.fail("Pick a control or tick \"Reset every control\"") };
                        let c = need(&list, &key)?;
                        ensure_active(&list, c)?;
                        reset_one(&cam, c)?;
                        publish(ctx, &cam);
                        return Ok(());
                    }
                    // Auto modes first: they decide which manual controls accept a write.
                    let (switches, rest): (Vec<&Control>, Vec<&Control>) = list.iter().partition(|c| !matches!(c.kind, Kind::Range { .. }));
                    let mut failed = 0;
                    for c in &switches {
                        failed += reset_one(&cam, c).is_err() as usize;
                    }
                    let now = os::controls(&cam.id).map_err(Error::msg)?;
                    for c in rest {
                        if find(&now, &c.key).is_some_and(|n| !n.inactive) {
                            failed += reset_one(&cam, c).is_err() as usize;
                        }
                    }
                    publish(ctx, &cam);
                    if failed > 0 {
                        return ctx.fail(format!("{failed} control(s) wouldn't reset"));
                    }
                    Ok(())
                }),
        )
        .run()
}
