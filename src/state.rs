//! Persistent pin state (`$XDG_STATE_HOME/impin/pins.toml` on Linux,
//! `%LOCALAPPDATA%\impin\pins.toml` on Windows).
//!
//! The daemon is the single writer; saves are atomic (tmp + rename) so a
//! crash mid-write can never corrupt the file.

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// One pinned image, as persisted. Geometry is in output-local pixels.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PinRecord {
    pub source: PathBuf,
    /// Hyprland output name the geometry is relative to ("" = fallback).
    pub output: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Sibling order among pins (creation order in v0.1).
    pub z: u32,
    /// View state. Defaults keep M1-era files (geometry only) loadable.
    /// zoom 1 = 100% raster; pan = image top-left in window px;
    /// opacity 0.2..1.0; radius in px.
    #[serde(default = "default_zoom")]
    pub zoom: f32,
    #[serde(default)]
    pub pan_x: f64,
    #[serde(default)]
    pub pan_y: f64,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    #[serde(default)]
    pub radius: f64,
}

fn default_zoom() -> f32 {
    1.0
}

fn default_opacity() -> f32 {
    1.0
}

/// The on-disk document: TOML roots must be tables, so records live under
/// `[[pins]]` (array of tables).
#[derive(Debug, Serialize, Deserialize, Default)]
struct PinFile {
    pins: Vec<PinRecord>,
}

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// `XDG_STATE_HOME/impin/pins.toml` (default `~/.local/state`).
    pub fn new() -> Self {
        Self {
            path: default_dir().join("pins.toml"),
        }
    }

    pub fn load(&self) -> anyhow::Result<Vec<PinRecord>> {
        match std::fs::read_to_string(&self.path) {
            // A corrupt file is an ERROR, not an empty store: the daemon
            // moves it aside so no save can clobber it.
            Ok(text) => toml::from_str::<PinFile>(&text)
                .map(|file| file.pins)
                .context(format!("parsing {}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(anyhow::anyhow!("reading {}: {e}", self.path.display())),
        }
    }

    /// The state file path (for moving a corrupt file aside).
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn save(&self, pins: &[PinRecord]) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(&PinFile {
            pins: pins.to_vec(),
        })?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

/// Content-addressed store for clipboard images (`images/<blake3>.png`).
pub fn images_dir() -> PathBuf {
    default_dir().join("images")
}

fn default_dir() -> PathBuf {
    #[cfg(windows)]
    {
        // Spec 01: %LOCALAPPDATA%\impin (roaming-like per-user state).
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("AppData/Local")
            });
        base.join("impin")
    }
    #[cfg(not(windows))]
    {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
            });
        base.join("impin")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("impin-test-{}", std::process::id()))
    }

    #[test]
    fn missing_file_loads_empty() {
        let store = Store {
            path: Path::new("/nonexistent/impin/pins.toml").to_path_buf(),
        };
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn round_trips_records_atomically() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store {
            path: dir.join("pins.toml"),
        };
        let pins = vec![PinRecord {
            source: "/tmp/a.png".into(),
            output: "DP-1".into(),
            x: 12.0,
            y: 34.0,
            w: 100.0,
            h: 50.0,
            z: 0,
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
            opacity: 1.0,
            radius: 0.0,
        }];
        store.save(&pins).unwrap();
        assert!(!dir.join("pins.toml.tmp").exists(), "tmp must be renamed");
        assert_eq!(store.load().unwrap(), pins);
        std::fs::remove_dir_all(&dir).ok();
    }
}
