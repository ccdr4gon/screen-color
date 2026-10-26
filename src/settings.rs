//! Saved temperatures in %APPDATA%\screen_color\settings.json (shared with screen_color.py).

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::color::NEUTRAL_K;
use crate::display::Display;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    /// Temperature per monitor, keyed by `Display::device_path`.
    pub temperatures: BTreeMap<String, u32>,
    /// The display last picked in the window.
    pub selected: Option<String>,
}

fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA").or_else(|| std::env::var_os("USERPROFILE"));
    PathBuf::from(base.unwrap_or_default()).join("screen_color").join("settings.json")
}

impl Settings {
    /// The saved settings, or the defaults if there are none or they can't be read.
    pub fn load() -> Self {
        Self::read().unwrap_or_default()
    }

    /// The saved settings; `None` if the file exists but can't be read right now.
    pub fn read() -> Option<Self> {
        let text = match std::fs::read_to_string(path()) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Some(Self::default()),
            Err(_) => return None,
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Some(Self::default());
        };
        let temperatures = value["temperatures"]
            .as_object()
            .map(|map| {
                map.iter()
                    .filter_map(|(path, k)| Some((path.clone(), u32::try_from(k.as_u64()?).ok()?)))
                    .collect()
            })
            .unwrap_or_default();
        let selected = value["selected"].as_str().map(str::to_owned);
        Some(Self { temperatures, selected })
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut value = json!({ "temperatures": self.temperatures });
        if let Some(selected) = &self.selected {
            value["selected"] = json!(selected);
        }
        let text = serde_json::to_string_pretty(&value).expect("settings are always valid JSON");
        // Write a temporary file and swap it in, so other programs never read a half-written file.
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, &path)
    }

    /// Three-way merge for when this program and another (say the window and
    /// `screen-color set`) both changed the settings since they were read as `base`.
    /// The result has both sets of changes; where both changed a display, `ours` wins.
    pub fn merge(base: &Settings, ours: &Settings, theirs: &Settings) -> Settings {
        let mut merged = theirs.clone();
        for path in base.temperatures.keys().chain(ours.temperatures.keys()) {
            let mine = ours.temperatures.get(path);
            if mine != base.temperatures.get(path) {
                match mine {
                    Some(&kelvin) => merged.temperatures.insert(path.clone(), kelvin),
                    None => merged.temperatures.remove(path),
                };
            }
        }
        if ours.selected != base.selected {
            merged.selected = ours.selected.clone();
        }
        merged
    }

    pub fn temperature(&self, display: &Display) -> Option<u32> {
        self.temperatures.get(&display.device_path).copied()
    }

    /// Neutral displays aren't stored, so they're left alone for other apps to manage.
    pub fn remember(&mut self, display: &Display, kelvin: u32) {
        if kelvin == NEUTRAL_K {
            self.temperatures.remove(&display.device_path);
        } else {
            self.temperatures.insert(display.device_path.clone(), kelvin);
        }
    }
}
