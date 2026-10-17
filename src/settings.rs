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
    pub appearance: AppearanceSettings,
    pub privacy: PrivacySettings,
}

impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            search_engine: crate::SEARCH.to_owned(),
            memory: MemorySettings::preset("balanced").unwrap(),
            appearance: AppearanceSettings::default(),
            privacy: PrivacySettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppearanceSettings {
    /// `system` (ChickenWM's theme mode), `light` or `dark` — same accent palette either way.
    pub theme: String,
    /// Switch the browser's chrome between light and dark to match each page's background.
    pub match_website: bool,
    /// Tabs in a sidebar on the left instead of a strip under the toolbar.
    pub vertical_tabs: bool,
    /// Vertical tab sidebar shows icons only.
    pub sidebar_collapsed: bool,
    /// Vertical tabs hide to a thin edge strip and slide out over the page on hover.
    pub sidebar_autohide: bool,
    /// UI motion (tab open/close, sliding highlight, theme cross-fades).
    pub animations: bool,
    /// Soft, slowly drifting gradient behind the chrome, built from the ChickenWM theme colors.
    pub gradient: bool,
    /// 0–100.
    pub gradient_strength: u8,
    /// Film-grain texture over the chrome (like Zen).
    pub noise: bool,
    /// 0–100.
    pub noise_strength: u8,
    /// The page's colors bleed into the chrome as slow, blurred glows.
    pub ambient: bool,
    /// 0–100.
    pub ambient_strength: u8,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            match_website: false,
            vertical_tabs: false,
            sidebar_collapsed: false,
            sidebar_autohide: false,
            animations: true,
            gradient: true,
            gradient_strength: 55,
            noise: true,
            noise_strength: 35,
            ambient: true,
            ambient_strength: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PrivacySettings {
    /// Edge's built-in tracking prevention: `none`, `basic`, `balanced` or `strict`. The ad
    /// blocker already removes trackers, so `basic` avoids breaking sign-in flows by default.
    pub tracking_prevention: String,
    /// Sites (host names, `www.` stripped) where the ad blocker is turned off.
    pub adblock_allowlist: Vec<String>,
}

impl Default for PrivacySettings {
    fn default() -> Self {
        Self { tracking_prevention: "basic".into(), adblock_allowlist: Vec::new() }
    }
}

/// `www.example.com` → `example.com`; `None` for non-web URLs.
pub fn site_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").filter(|(s, _)| *s == "http" || *s == "https")?.1;
    let host = rest.split(['/', '?', '#']).next()?.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.trim_start_matches("www.").to_ascii_lowercase())
}

/// Whether `url`'s host is `site` or one of its subdomains.
pub fn url_on_site(url: &str, site: &str) -> bool {
    site_of(url).is_some_and(|h| h == site || h.ends_with(&format!(".{site}")))
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
    fn site_matching() {
        assert_eq!(site_of("https://www.Example.com:8080/a?b#c").as_deref(), Some("example.com"));
        assert_eq!(site_of("about:blank"), None);
        assert!(url_on_site("https://aos.istanbul.edu.tr/x", "istanbul.edu.tr"));
        assert!(!url_on_site("https://notistanbul.edu.tr/", "istanbul.edu.tr"));
        let s: BrowserSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.appearance.theme, "system");
        assert_eq!(s.privacy.tracking_prevention, "basic");
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
