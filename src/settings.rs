//! Persisted browser settings (`~/.config/chickenwm/chickenbrowser/settings.json`), mainly the memory
//! options. Options marked "restart" become WebView2 browser arguments, which are fixed for the
//! lifetime of the browser process; the rest apply live.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BrowserSettings {
    pub search_engine: String,
    pub memory: MemorySettings,
}

impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            search_engine: crate::SEARCH.to_owned(),
            memory: MemorySettings::preset("balanced").unwrap(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MemorySettings {
    /// `balanced`, `saver`, `maximum` or `custom` (any hand-edited combination).
    pub preset: String,
    /// Live: ask WebView2 to trim memory of tabs as soon as they go into the background.
    pub low_memory_background: bool,
    /// Live: freeze background tabs (timers, scripts) after this many seconds. 0 = never.
    pub suspend_after_secs: u64,
    /// Live: unload background tabs after this many minutes; they reload when revisited and
    /// lose their back/forward history. 0 = never.
    pub discard_after_mins: u64,
    /// Restart: run the toolbar inside the pages' browser process instead of a second one.
    pub share_toolbar_process: bool,
    /// Restart: put all tabs of the same site into one renderer process.
    pub process_per_site: bool,
    /// Restart: cap on renderer processes (0 = Chromium decides). Sites share renderers past it.
    pub renderer_process_limit: u32,
    /// Restart: keep no pages alive for instant back/forward (Chromium's back-forward cache).
    pub disable_back_forward_cache: bool,
    /// Restart: Chromium's low-end-device mode (smaller caches, less GPU memory, simpler raster).
    pub low_end_device_mode: bool,
    /// Restart: no browser-internal background networking (pre-connect, update pings, ...).
    pub disable_background_networking: bool,
    /// Restart: V8 heap cap per renderer in MB (0 = default). Heavy web apps may crash below it.
    pub js_heap_limit_mb: u32,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self::preset("balanced").unwrap()
    }
}

impl MemorySettings {
    pub fn preset(name: &str) -> Option<Self> {
        let base = Self {
            preset: name.to_owned(),
            low_memory_background: true,
            suspend_after_secs: 60,
            discard_after_mins: 0,
            share_toolbar_process: true,
            process_per_site: false,
            renderer_process_limit: 0,
            disable_back_forward_cache: false,
            low_end_device_mode: false,
            disable_background_networking: true,
            js_heap_limit_mb: 0,
        };
        Some(match name {
            "balanced" => base,
            "saver" => Self {
                suspend_after_secs: 20,
                discard_after_mins: 20,
                process_per_site: true,
                renderer_process_limit: 6,
                disable_back_forward_cache: true,
                ..base
            },
            "maximum" => Self {
                suspend_after_secs: 5,
                discard_after_mins: 5,
                process_per_site: true,
                renderer_process_limit: 3,
                disable_back_forward_cache: true,
                low_end_device_mode: true,
                ..base
            },
            _ => return None,
        })
    }

    /// WebView2 `AdditionalBrowserArguments`. Replaces wry's defaults, so those are kept here.
    pub fn browser_args(&self) -> String {
        let mut disabled = vec!["msWebOOUI", "msPdfOOUI", "msSmartScreenProtection"];
        let mut args = Vec::new();
        if self.disable_back_forward_cache {
            disabled.push("BackForwardCache");
        }
        if self.process_per_site {
            args.push("--process-per-site".to_owned());
        }
        if self.renderer_process_limit > 0 {
            args.push(format!("--renderer-process-limit={}", self.renderer_process_limit));
        }
        if self.low_end_device_mode {
            args.push("--enable-low-end-device-mode".to_owned());
        }
        if self.disable_background_networking {
            args.push("--disable-background-networking".to_owned());
        }
        if self.js_heap_limit_mb > 0 {
            args.push(format!("--js-flags=--max-old-space-size={}", self.js_heap_limit_mb));
        }
        args.insert(0, format!("--disable-features={}", disabled.join(",")));
        args.join(" ")
    }

    /// Whether switching from `self` to `other` only takes effect after a browser restart.
    pub fn needs_restart(&self, other: &Self) -> bool {
        self.browser_args() != other.browser_args()
            || self.share_toolbar_process != other.share_toolbar_process
    }
}

pub fn path(root: &Path) -> PathBuf {
    root.join("settings.json")
}

pub fn load(root: &Path) -> BrowserSettings {
    std::fs::read_to_string(path(root))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(root: &Path, settings: &BrowserSettings) {
    match serde_json::to_string_pretty(settings) {
        Ok(text) => {
            if let Err(e) = std::fs::write(path(root), text) {
                tracing::warn!("failed to save browser settings: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to serialize browser settings: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_and_args() {
        let balanced = MemorySettings::preset("balanced").unwrap();
        assert_eq!(
            balanced.browser_args(),
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --disable-background-networking"
        );
        let max = MemorySettings::preset("maximum").unwrap();
        let args = max.browser_args();
        assert!(args.starts_with("--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,BackForwardCache "));
        assert!(args.contains("--renderer-process-limit=3"));
        assert!(args.contains("--enable-low-end-device-mode"));
        assert!(balanced.needs_restart(&max));
        let mut live_only = balanced.clone();
        live_only.suspend_after_secs = 5;
        assert!(!balanced.needs_restart(&live_only));
        assert!(MemorySettings::preset("nope").is_none());
    }

    #[test]
    fn partial_settings_file_falls_back_to_defaults() {
        let s: BrowserSettings =
            serde_json::from_str(r#"{"memory":{"suspend_after_secs":7}}"#).unwrap();
        assert_eq!(s.memory.suspend_after_secs, 7);
        assert!(s.memory.share_toolbar_process);
        assert_eq!(s.search_engine, crate::SEARCH);
    }
}
