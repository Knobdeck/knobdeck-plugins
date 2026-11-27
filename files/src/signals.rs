//! What the file manager has selected, for profile rules: the dial and the phone's deck can switch
//! to a profile of image actions while images are selected, PDF actions while a PDF is.
//!
//! - `files.extension`: the extensions of the selected files (`png`, `pdf`…), a set.
//! - `files.kind`: the kinds of the selected files (`image`, `video`, `audio`, `pdf`, `archive`,
//!   `text`, `folder`), a set.
//!
//! Both are values only while the window in front is a file manager KnobDeck can read; elsewhere
//! they have no value (a rule never holds on them). They are also not reported for a selection
//! with a file of a type that isn't listed: the options are what the plugin supports. The host
//! reads them as the dial opens (`on_signal_read`); while a phone's deck follows them, they are
//! watched (once a second, only then) and reported when they change.

use crate::paths;
use knobdeck_sdk::os;
use knobdeck_sdk::{json, Error, Result, Runtime, SignalDecl, SignalMode, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const EXTENSION: &str = "files.extension";
pub const KIND: &str = "files.kind";

/// The kinds and their extensions.
const KINDS: &[(&str, &str, &[&str])] = &[
    ("image", "Image", &["png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "bmp", "tif", "tiff", "svg", "avif", "ico", "raw", "cr2", "nef", "arw", "dng"]),
    ("video", "Video", &["mp4", "mov", "mkv", "avi", "webm", "m4v", "wmv", "flv", "mpg", "mpeg"]),
    ("audio", "Audio", &["mp3", "wav", "flac", "aac", "m4a", "ogg", "opus", "aiff", "wma"]),
    ("pdf", "PDF", &["pdf"]),
    ("archive", "Archive", &["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst", "dmg", "iso"]),
    ("text", "Text", &["txt", "md", "markdown", "rtf", "csv", "json", "yaml", "yml", "toml", "xml", "log", "ini"]),
];

pub fn declarations() -> Vec<SignalDecl> {
    let exts: Vec<(String, String)> = KINDS.iter().flat_map(|(_, _, e)| e.iter().map(|e| (e.to_string(), e.to_uppercase()))).collect();
    let ext_opts: Vec<(&str, &str)> = exts.iter().map(|(v, l)| (v.as_str(), l.as_str())).collect();
    let mut kind_opts: Vec<(&str, &str)> = KINDS.iter().map(|(k, l, _)| (*k, *l)).collect();
    kind_opts.push(("folder", "Folder"));
    vec![
        SignalDecl::choice(EXTENSION, "Selected file types", &ext_opts)
            .multi()
            .mode(SignalMode::Both)
            .description("The extensions of the files selected in the file manager"),
        SignalDecl::choice(KIND, "Selected file kinds", &kind_opts)
            .multi()
            .mode(SignalMode::Both)
            .description("Image, video, audio, PDF, archive, text or folder, for the files selected in the file manager"),
    ]
}

fn kind_of(p: &Path) -> Option<&'static str> {
    if p.is_dir() {
        return Some("folder");
    }
    let e = paths::ext(p);
    KINDS.iter().find(|(_, _, exts)| exts.contains(&e.as_str())).map(|(k, _, _)| *k)
}

fn ext_of(p: &Path) -> Option<String> {
    let e = paths::ext(p);
    KINDS.iter().any(|(_, _, exts)| exts.contains(&e.as_str())).then_some(e)
}

/// The distinct values for `signal` over the selected files, in first-seen order. `Err` when the
/// window in front is no file manager we can read, or a file's type isn't one we list.
pub fn value_of(signal: &str, files: &[std::path::PathBuf]) -> Result<Value> {
    let mut out: Vec<String> = vec![];
    for f in files {
        let v = match signal {
            EXTENSION if f.is_dir() => continue,
            EXTENSION => ext_of(f),
            KIND => kind_of(f).map(String::from),
            _ => return Err(Error::msg(format!("no signal \"{signal}\""))),
        };
        let v = v.ok_or_else(|| Error::msg("a selected file's type isn't supported"))?;
        if !out.contains(&v) {
            out.push(v);
        }
    }
    Ok(json!(out))
}

/// The selection now, for a read or the watcher.
pub fn read(signal: &str) -> Result<Value> {
    let files = os::files::selected().map_err(|e| Error::msg(e.to_string()))?;
    value_of(signal, &files)
}

static WATCHING: AtomicBool = AtomicBool::new(false);
const POLL: Duration = Duration::from_secs(1);

/// The host's list of wanted signals changed: watch while one of ours is on it.
pub fn demand(wanted: &[String], rt: &Runtime) {
    let ours = |w: &[String]| w.iter().any(|s| s == EXTENSION || s == KIND);
    if !ours(wanted) || WATCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    let rt = rt.clone();
    std::thread::spawn(move || {
        let mut last: [Option<Option<Value>>; 2] = [None, None];
        while ours(&rt.signal_demand()) {
            // One read of the selection serves both signals.
            let files = os::files::selected();
            for (i, id) in [EXTENSION, KIND].into_iter().enumerate() {
                if !rt.signal_wanted(id) {
                    continue;
                }
                let now = files.as_ref().ok().and_then(|f| value_of(id, f).ok());
                if last[i].as_ref() != Some(&now) {
                    match &now {
                        Some(v) => rt.signal(id, v.clone()),
                        None => rt.clear_signal(id),
                    }
                    last[i] = Some(now);
                }
            }
            std::thread::sleep(POLL);
        }
        for id in [EXTENSION, KIND] {
            rt.clear_signal(id);
        }
        WATCHING.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn extensions_and_kinds_are_distinct_sets() {
        let files = [p("/a/x.PNG"), p("/a/y.png"), p("/a/z.pdf")];
        assert_eq!(value_of(EXTENSION, &files).unwrap(), json!(["png", "pdf"]));
        assert_eq!(value_of(KIND, &files).unwrap(), json!(["image", "pdf"]));
        assert_eq!(value_of(EXTENSION, &[]).unwrap(), json!([]), "nothing selected is a value");
    }

    #[test]
    fn a_type_we_do_not_list_is_not_reported() {
        assert!(value_of(EXTENSION, &[p("/a/x.png"), p("/a/setup.exe")]).is_err());
        assert!(value_of(KIND, &[p("/a/noext")]).is_err());
    }

    #[test]
    fn every_value_we_send_is_a_declared_option() {
        let decls = declarations();
        for d in &decls {
            let all: Vec<String> = d.options.iter().map(|o| o.value.clone()).collect();
            let sample: Vec<PathBuf> = KINDS.iter().flat_map(|(_, _, e)| e.iter().map(|e| p(&format!("/a/f.{e}")))).collect();
            let v = value_of(&d.id, &sample).unwrap();
            assert!(d.accept(&v).is_some(), "{}: {v} not accepted; options {all:?}", d.id);
        }
    }
}
