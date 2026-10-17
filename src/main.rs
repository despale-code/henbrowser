//! chickenbrowser — a lightweight browser for HyperWin on the system WebView2 (Chromium) engine.
//!
//! - Tabs are WebView2 child views; only the active one is visible and they resize synchronously
//!   with the window, so content keeps up with live resizing.
//! - Chromium extensions (e.g. Proton Pass) load from `~/.config/chickenwm/chickenbrowser/extensions/`;
//!   their toolbar buttons and popups are provided by chickenbrowser (WebView2 has no extension UI).
//! - Built-in network ad/tracker blocking (EasyList + EasyPrivacy) — see `adblock_filter`.

#![windows_subsystem = "windows"]

mod adblock_filter;
mod extensions;
mod history;
mod settings;

use adblock_filter::SharedEngine;
use extensions::ExtensionInfo;
use settings::{BrowserSettings, MemorySettings};
use chickenwm_core::config::config_dir;
use serde::Deserialize;
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::{ResizeDirection, Window, WindowBuilder};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{AcceleratorKeyPressedEventHandler, CapturePreviewCompletedHandler, ClearBrowsingDataCompletedHandler, FocusChangedEventHandler, TrySuspendCompletedHandler};
use windows::core::{BOOL, Interface};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM};
use windows::Win32::System::Com::{IStream, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET};
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_F5, VK_LEFT, VK_MENU, VK_RIGHT, VK_SHIFT,
    VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, SetForegroundWindow, SetWindowPos, ShowWindow, HWND_BOTTOM, HWND_TOP,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_RESTORE,
};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{
    MemoryUsageLevel, NewWindowResponse, PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder,
    WebViewBuilderExtWindows, WebViewExtWindows,
};

/// Height of the toolbar + tab strip (logical px); must match `toolbar.html`.
const TOOLBAR_H: f64 = 82.0;
/// Address bar suggestion rows (logical px); must match suggest.html.
const SUGGEST_ROW_H: f64 = 36.0;
const SUGGEST_PAD: f64 = 18.0;
const SUGGEST_MAX: usize = 6;
/// Height of the toolbar row alone; extension popovers open just below it, Safari-style.
const TOOLBAR_ROW_H: f64 = 48.0;
/// Vertical tab sidebar widths (logical px); must match toolbar.html.
const SIDEBAR_W: f64 = 240.0;
const SIDEBAR_COLLAPSED_W: f64 = 56.0;
/// Hover strip left visible when the vertical tab sidebar auto-hides.
const SIDEBAR_STRIP_W: f64 = 6.0;
const HOME: &str = "https://duckduckgo.com/";
const SEARCH: &str = "https://duckduckgo.com/?q=";
const POPUP_W: f64 = 600.0;
const POPUP_H: f64 = 430.0;
/// Toolbar background used until `browser.css` has been read (its `--bg-color`).
const CHROME_BG: (u8, u8, u8, u8) = (240, 239, 237, 255);

/// `--bg-color` from browser.css, so the window never flashes a different color while loading.
fn chrome_bg(css: &str) -> (u8, u8, u8, u8) {
    let fallback = chickenwm_core::config::RgbaColor { r: CHROME_BG.0, g: CHROME_BG.1, b: CHROME_BG.2, a: 255 };
    let c = chickenwm_core::css::CssStyleSheet::parse(css).var_color("--bg-color", fallback);
    (c.r, c.g, c.b, 255)
}

/// JS that puts browser.css on the page as a `<style id="user-theme">`.
fn theme_apply_js(css: &str) -> String {
    format!(
        "(function(css){{let el=document.getElementById('user-theme');if(!el){{el=document.createElement('style');el.id='user-theme';(document.head||document.documentElement).appendChild(el);}}el.textContent=css;}})({});",
        serde_json::to_string(css).unwrap_or_default()
    )
}

/// Initialization script for internal pages (menu, about:settings — both `about:blank`
/// documents): applies the system theme as soon as the document exists. Web pages are skipped.
fn internal_mode_init(mode_css: &str) -> String {
    if mode_css.is_empty() {
        return String::new();
    }
    format!(
        "if (location.protocol === 'about:' || location.protocol === 'data:') {{ const go = () => {{ {} }}; if (document.head) go(); else document.addEventListener('DOMContentLoaded', go); }}",
        mode_apply_js(mode_css)
    )
}

fn theme_init_script(css: &str) -> String {
    format!(
        "if (location.protocol === 'about:' || location.protocol === 'data:') {{ const go = () => {{ {} }}; if (document.head) go(); else document.addEventListener('DOMContentLoaded', go); }}",
        theme_apply_js(css)
    )
}

#[derive(Debug, Clone, Copy)]
pub enum Shortcut {
    NewTab,
    CloseTab,
    FocusAddress,
    NextTab,
    PrevTab,
    Reload,
    Back,
    Forward,
    SelectTab(usize),
    Escape,
}

pub enum UserEvent {
    Ipc(String),
    Title(u32, String),
    PageLoad { tab: u32, url: String, started: bool },
    OpenTab(String),
    Shortcut(Shortcut),
    AdblockReady(Box<adblock::Engine>),
    Blocked(u32),
    Extensions(Vec<ExtensionInfo>),
    RefreshExtensions,
    PopupLostFocus,
    CssReload(String),
    /// Periodic check that suspends/unloads background tabs.
    MemoryTick,
    /// A tab's favicon URL changed.
    Favicon(u32, String),
    /// A tab's page background was measured: `true` = dark.
    PageTone(u32, bool),
    /// A tab's page colors were sampled for the ambient glow.
    PageAmbient(u32, Vec<[u8; 3]>),
    /// Capturing a tab for the ambient glow failed: sample its CSS colors instead.
    AmbientFallback(u32),
    /// Re-sample the active page, so playing video / animated images keep the glow in step.
    AmbientTick,
    /// chickenwm.conf changed (theme / per-app modes may have).
    ThemeConfigChanged,
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum ToolbarCmd {
    Ready,
    Navigate { input: String },
    /// Address bar text changed: show history matches under the field at `x`/`w` (logical px).
    Suggest { text: String, x: f64, w: f64 },
    SuggestSel { sel: i32 },
    SuggestHide,
    NewTab,
    NewWindow,
    CloseTab { id: u32 },
    SwitchTab { id: u32 },
    Back,
    Forward,
    Reload,
    ExtPopup { id: String },
    FocusPage,
    ToggleMenu,
    OpenSettings,
    EditCss,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    Print,
    Devtools,
    ClearData,
    ProtonPass,
    SetSearchEngine { url: String },
    /// The settings page loaded and wants the current settings + memory usage.
    SettingsReady,
    SetMemoryPreset { preset: String },
    SetMemory { memory: MemorySettings },
    /// Relaunch so browser-argument (restart) options take effect.
    RestartBrowser,
    SetAppearance { appearance: settings::AppearanceSettings },
    SetPrivacy { privacy: settings::PrivacySettings },
    /// Shield button: turn the ad blocker off/on for the current site.
    ToggleSiteAdblock,
    /// Collapse / expand the vertical tab sidebar.
    ToggleSidebar,
    ToggleVerticalTabs,
    ToggleSidebarAutohide,
    /// Auto-hide: show / hide the sliding tab panel over the page.
    SidebarPeek { show: bool },
    /// The sliding tab panel finished loading.
    FlyoutReady,
    /// Menu: flip between light and dark (from whatever is showing now).
    ToggleDark,
    /// Settings page: `system` | `light` | `dark` for the browser (master theme provider).
    SetAppMode { mode: String },
    /// Custom titlebar (the window is undecorated): move the window.
    DragWindow,
    /// Resize from the top edge/corners (`n`, `nw`, `ne`); the other edges use the native
    /// invisible resize frame kept by the undecorated shadow.
    ResizeWindow { edge: String },
    /// Double-click on empty toolbar space, like Safari's zoom.
    ToggleMaximize,
    CloseWindow,
}

struct Paths {
    root: PathBuf,
    profile: PathBuf,
    extensions: PathBuf,
    adblock: PathBuf,
    css: PathBuf,
}

struct Tab {
    id: u32,
    webview: WebView,
    title: String,
    url: String,
    loading: bool,
    /// Current page URL, shared with the ad blocker for first/third-party decisions.
    page_url: Rc<RefCell<String>>,
    /// When the tab last went into the background (None while it is the visible tab).
    hidden_since: Option<Instant>,
    /// Frozen via `TrySuspend`; WebView2 resumes it automatically when it becomes visible.
    suspended: bool,
    /// Unloaded to `about:blank` to free its renderer; `url` is reloaded when it is shown again.
    discarded: bool,
    /// Showing the built-in settings page. Only then does the tab's IPC reach the browser, so
    /// ordinary web pages can't send browser commands through `window.ipc`.
    settings_page: Rc<Cell<bool>>,
    /// Favicon URL (empty until the page reports one).
    favicon: String,
    /// Measured page background (`Some(true)` = dark), for "match website colors".
    tone: Option<bool>,
    /// Colors sampled from the page (dominant first), bled into the chrome as ambient glow.
    ambient: Vec<[u8; 3]>,
}

struct Popup {
    extension_id: String,
    webview: WebView,
    opened_at: Instant,
}

struct Browser {
    window: Window,
    toolbar: WebView,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u32,
    popup: Option<Popup>,
    /// When/which popup last closed by losing focus, so the click on its own toolbar button
    /// (which steals focus first) toggles it closed instead of instantly reopening it.
    popup_closed: Option<(String, Instant)>,
    web_context: WebContext,
    /// Separate toolbar profile, only when `share_toolbar_process` is off.
    _toolbar_context: Option<WebContext>,
    proxy: EventLoopProxy<UserEvent>,
    engine: SharedEngine,
    blocked: u32,
    extensions: Vec<ExtensionInfo>,
    extensions_loaded: bool,
    paths: Paths,
    settings: BrowserSettings,
    /// Memory settings the browser process was started with (restart options are fixed).
    launch_memory: MemorySettings,
    /// Current browser.css, injected into internal pages (menu, about:settings).
    user_css: String,
    /// ChickenWM theme (accent + mode) the light/dark override palettes are built from.
    system_theme: chickenwm_core::theme::ThemeConfig,
    /// Light/dark override CSS last sent to the chrome ("" = follow browser.css).
    mode_css: String,
    allowlist: adblock_filter::SharedAllowlist,
    /// Auto-hide tab panel (toolbar.html in flyout mode), created on first hover.
    flyout: Option<WebView>,
    flyout_visible: bool,
    history: history::History,
    /// Address bar suggestion dropdown (suggest.html), built hidden at startup so it can slide
    /// over the page like the tab flyout.
    suggest: Option<WebView>,
    suggest_visible: bool,
    /// Bottom-left total-RAM chip (ram.html), a small overlay over the page.
    ram: Option<WebView>,
    session_path: PathBuf,
    /// Last session JSON written, so ticks only touch the disk when the tabs changed.
    session_saved: String,
    /// When the in-flight ambient capture started (None = idle).
    ambient_busy: Rc<Cell<Option<Instant>>>,
}

/// Every webview on the shared profile must be created with identical environment options
/// (extensions flag, browser arguments), or WebView2 refuses to create it — so all of them are
/// built through here.
fn profile_builder<'a>(context: &'a mut WebContext, memory: &MemorySettings) -> WebViewBuilder<'a> {
    WebViewBuilder::new_with_web_context(context)
        .with_browser_extensions_enabled(true)
        .with_additional_browser_args(memory.browser_args())
}

/// Reports favicon changes as `UserEvent::Favicon`.
fn install_favicon(controller: &ICoreWebView2Controller, tab: u32, proxy: EventLoopProxy<UserEvent>) {
    use webview2_com::FaviconChangedEventHandler;
    let Ok(core) = (unsafe { controller.CoreWebView2() }) else { return };
    let Ok(core15) = core.cast::<ICoreWebView2_15>() else { return };
    let mut token = 0i64;
    unsafe {
        let _ = core15.add_FaviconChanged(
            &FaviconChangedEventHandler::create(Box::new(move |sender, _| {
                if let Some(c15) = sender.and_then(|s| s.cast::<ICoreWebView2_15>().ok()) {
                    let mut uri = windows::core::PWSTR::null();
                    c15.FaviconUri(&mut uri)?;
                    let _ = proxy.send_event(UserEvent::Favicon(tab, webview2_com::take_pwstr(uri)));
                }
                Ok(())
            })),
            &mut token,
        );
    }
}

/// Evaluates to `true` (dark), `false` (light) for the page's background: the first opaque
/// background of `<body>` / `<html>`, else of the element in the middle of the viewport.
const PAGE_TONE_JS: &str = r#"(() => {
  const parse = (c) => {
    const m = c && c.match(/rgba?\(([^)]+)\)/);
    if (!m) return null;
    const p = m[1].split(/[ ,/]+/).filter(Boolean).map(parseFloat);
    return p.length > 3 && p[3] < 0.5 ? null : p;
  };
  let c = null;
  for (const el of [document.body, document.documentElement]) {
    if (el) { c = parse(getComputedStyle(el).backgroundColor); if (c) break; }
  }
  for (let e = document.elementFromPoint(innerWidth / 2, innerHeight / 2); !c && e; e = e.parentElement) {
    c = parse(getComputedStyle(e).backgroundColor);
  }
  if (!c) c = [255, 255, 255];
  return (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]) / 255 < 0.45;
})()"#;

/// Evaluates to up to five `[r, g, b]` colors that represent the page, dominant first: the
/// `theme-color`, the backgrounds along the edges next to the chrome (where a bleed reads as a
/// continuation of the page), the middle, and the link color as an accent. Near-duplicates are
/// dropped. Cross-origin images can't be read, so only CSS colors are used.
const PAGE_AMBIENT_JS: &str = r#"(() => {
  try {
    const parse = (c) => {
      const m = c && c.match(/rgba?\(([^)]+)\)/);
      if (!m) return null;
      const p = m[1].split(/[ ,/]+/).filter(Boolean).map(parseFloat);
      return p.length > 3 && p[3] < 0.4 ? null : p.slice(0, 3).map(Math.round);
    };
    const bgAt = (x, y) => {
      for (let e = document.elementFromPoint(x, y); e; e = e.parentElement) {
        const c = parse(getComputedStyle(e).backgroundColor);
        if (c) return c;
      }
      return null;
    };
    const out = [];
    const add = (c) => {
      if (!c) return;
      for (const o of out) {
        if (Math.abs(o[0] - c[0]) + Math.abs(o[1] - c[1]) + Math.abs(o[2] - c[2]) < 36) return;
      }
      out.push(c);
    };
    const meta = document.querySelector('meta[name="theme-color"]');
    if (meta && meta.content && document.body) {
      const probe = document.createElement("i");
      probe.style.color = meta.content;
      document.body.appendChild(probe);
      add(parse(getComputedStyle(probe).color));
      probe.remove();
    }
    const W = innerWidth, H = innerHeight;
    for (const [x, y] of [[W * 0.5, 3], [W * 0.15, 3], [W * 0.85, 3], [3, H * 0.3], [3, H * 0.7],
                          [W * 0.5, H * 0.45], [W * 0.5, H - 3]]) add(bgAt(x, y));
    const link = document.querySelector("a[href]");
    if (link) add(parse(getComputedStyle(link).color));
    for (const el of [document.body, document.documentElement]) {
      if (el) add(parse(getComputedStyle(el).backgroundColor));
    }
    if (!out.length) out.push([255, 255, 255]);
    return out.slice(0, 5);
  } catch (_) { return []; }
})()"#;

/// Reads a whole in-memory stream (a `CapturePreview` result).
fn read_stream(stream: &IStream) -> Option<Vec<u8>> {
    unsafe {
        let mut stat = STATSTG::default();
        stream.Stat(&mut stat, STATFLAG_NONAME).ok()?;
        let len = stat.cbSize as usize;
        stream.Seek(0, STREAM_SEEK_SET, None).ok()?;
        let mut buf = vec![0u8; len];
        let mut read = 0u32;
        if stream.Read(buf.as_mut_ptr().cast(), len as u32, Some(&mut read)).is_err() {
            return None;
        }
        buf.truncate(read as usize);
        Some(buf)
    }
}

/// Up to five `[r, g, b]` glow colors from a page capture, dominant first. Each region next to
/// the chrome (top middle, top corners, left side, centre) contributes its most prominent
/// color, favouring saturated ones over the grey that averaging video frames gives.
fn ambient_from_jpeg(bytes: &[u8]) -> Option<Vec<[u8; 3]>> {
    const REGIONS: [(f32, f32, f32, f32); 6] = [
        (0.2, 0.0, 0.8, 0.25),
        (0.0, 0.0, 0.4, 0.3),
        (0.6, 0.0, 1.0, 0.3),
        (0.0, 0.2, 0.3, 0.8),
        (0.3, 0.3, 0.7, 0.7),
        (0.0, 0.0, 1.0, 1.0),
    ];
    const W: u32 = 48;
    const H: u32 = 30;
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg).ok()?;
    let small = img.resize_exact(W, H, image::imageops::FilterType::Triangle).to_rgb8();
    let mut out: Vec<[u8; 3]> = Vec::new();
    for (x0, y0, x1, y1) in REGIONS {
        // 8 levels per channel; (count, sum r, sum g, sum b)
        let mut buckets = vec![(0u32, 0u32, 0u32, 0u32); 512];
        let (px0, px1) = ((x0 * W as f32) as u32, ((x1 * W as f32) as u32).max(1).min(W));
        let (py0, py1) = ((y0 * H as f32) as u32, ((y1 * H as f32) as u32).max(1).min(H));
        for y in py0..py1 {
            for x in px0..px1 {
                let p = small.get_pixel(x, y).0;
                let i = ((p[0] >> 5) as usize) << 6 | ((p[1] >> 5) as usize) << 3 | (p[2] >> 5) as usize;
                let b = &mut buckets[i];
                *b = (b.0 + 1, b.1 + p[0] as u32, b.2 + p[1] as u32, b.3 + p[2] as u32);
            }
        }
        let best = buckets.iter().filter(|b| b.0 > 0).max_by(|a, b| {
            let weight = |b: &&(u32, u32, u32, u32)| {
                let m = [b.1 / b.0, b.2 / b.0, b.3 / b.0];
                let sat = (m.iter().max().unwrap() - m.iter().min().unwrap()) as f32 / 255.0;
                b.0 as f32 * (1.0 + 1.5 * sat)
            };
            weight(a).total_cmp(&weight(b))
        })?;
        let c = [(best.1 / best.0) as u8, (best.2 / best.0) as u8, (best.3 / best.0) as u8];
        let distinct = out.iter().all(|o| {
            (o[0] as i32 - c[0] as i32).abs() + (o[1] as i32 - c[1] as i32).abs() + (o[2] as i32 - c[2] as i32).abs() >= 36
        });
        if distinct {
            out.push(c);
        }
    }
    out.truncate(5);
    (!out.is_empty()).then_some(out)
}

/// Profile-wide preferences: the color scheme pages see (`prefers-color-scheme`) and Edge's
/// tracking prevention level.
fn apply_profile_prefs(controller: &ICoreWebView2Controller, dark: Option<bool>, tracking: &str) {
    unsafe {
        let Ok(profile) = controller.CoreWebView2().and_then(|c| c.cast::<ICoreWebView2_13>()).and_then(|c| c.Profile()) else {
            return;
        };
        let scheme = match dark {
            Some(true) => COREWEBVIEW2_PREFERRED_COLOR_SCHEME_DARK,
            Some(false) => COREWEBVIEW2_PREFERRED_COLOR_SCHEME_LIGHT,
            None => COREWEBVIEW2_PREFERRED_COLOR_SCHEME_AUTO,
        };
        let _ = profile.SetPreferredColorScheme(scheme);
        if let Ok(p3) = profile.cast::<ICoreWebView2Profile3>() {
            let level = match tracking {
                "none" => COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_NONE,
                "balanced" => COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_BALANCED,
                "strict" => COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_STRICT,
                _ => COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_BASIC,
            };
            let _ = p3.SetPreferredTrackingPreventionLevel(level);
        }
    }
}

/// `:root { … }` override for the chrome in `mode`, built from the ChickenWM accent.
fn mode_css(theme: &chickenwm_core::theme::ThemeConfig, mode: chickenwm_core::theme::ThemeMode) -> String {
    let palette = theme.palette(mode);
    let mut css = String::from(":root {");
    for (k, v) in palette.css_vars("browser.css") {
        css.push_str(&format!(" {k}: {v};"));
    }
    css.push_str(&format!(" color-scheme: {}; }}", mode.as_str()));
    css
}

/// JS that installs the light/dark override as `<style id="mode-theme">` (after the user theme).
fn mode_apply_js(css: &str) -> String {
    format!(
        "(function(css){{let el=document.getElementById('mode-theme');if(!el){{el=document.createElement('style');el.id='mode-theme';(document.head||document.documentElement).appendChild(el);}}el.textContent=css;}})({});",
        serde_json::to_string(css).unwrap_or_default()
    )
}

/// Logs failed navigations (with WebView2's error status), and with `CHICKENBROWSER_DEBUG` set,
/// every JavaScript exception and console error a page produces.
fn install_diagnostics(controller: &ICoreWebView2Controller, tab: u32) {
    use webview2_com::{
        CallDevToolsProtocolMethodCompletedHandler, DevToolsProtocolEventReceivedEventHandler,
        NavigationCompletedEventHandler,
    };
    let Ok(core) = (unsafe { controller.CoreWebView2() }) else { return };
    let mut token = 0i64;
    unsafe {
        let _ = core.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    let mut ok = BOOL(0);
                    args.IsSuccess(&mut ok)?;
                    if !ok.as_bool() {
                        let mut status = COREWEBVIEW2_WEB_ERROR_STATUS(0);
                        args.WebErrorStatus(&mut status)?;
                        tracing::warn!("tab {tab}: navigation failed (WebErrorStatus {})", status.0);
                    }
                }
                Ok(())
            })),
            &mut token,
        );
    }
    if std::env::var_os("CHICKENBROWSER_DEBUG").is_none() {
        return;
    }
    for (event, enable) in [("Runtime.exceptionThrown", "Runtime.enable"), ("Log.entryAdded", "Log.enable")] {
        unsafe {
            if let Ok(receiver) = core.GetDevToolsProtocolEventReceiver(&windows::core::HSTRING::from(event)) {
                let _ = receiver.add_DevToolsProtocolEventReceived(
                    &DevToolsProtocolEventReceivedEventHandler::create(Box::new(move |_, args| {
                        if let Some(args) = args {
                            let mut json = windows::core::PWSTR::null();
                            args.ParameterObjectAsJson(&mut json)?;
                            let text = webview2_com::take_pwstr(json);
                            tracing::warn!("tab {tab} {event}: {}", text.chars().take(600).collect::<String>());
                        }
                        Ok(())
                    })),
                    &mut token,
                );
            }
            let _ = core.CallDevToolsProtocolMethod(
                &windows::core::HSTRING::from(enable),
                &windows::core::HSTRING::from("{}"),
                &CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(()))),
            );
        }
    }
}

fn show_settings_page(webview: &WebView) {
    if let Ok(core) = unsafe { webview.controller().CoreWebView2() } {
        let html = windows::core::HSTRING::from(include_str!("settings.html"));
        let _ = unsafe { core.NavigateToString(&html) };
    }
}

/// Private bytes of every process in the browser's WebView2 process tree.
fn process_memory(core: &ICoreWebView2) -> Option<(u64, u32)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let infos = core
            .cast::<ICoreWebView2_2>()
            .ok()?
            .Environment()
            .ok()?
            .cast::<ICoreWebView2Environment8>()
            .ok()?
            .GetProcessInfos()
            .ok()?;
        let mut count = 0u32;
        infos.Count(&mut count).ok()?;
        let mut total = 0u64;
        for i in 0..count {
            let Ok(info) = infos.GetValueAtIndex(i) else { continue };
            let mut pid = 0i32;
            if info.ProcessId(&mut pid).is_err() {
                continue;
            }
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) else { continue };
            let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
            let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
            if K32GetProcessMemoryInfo(handle, &mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS, size).as_bool() {
                total += counters.PrivateUsage as u64;
            }
            let _ = CloseHandle(handle);
        }
        Some((total, count))
    }
}

/// Turns address-bar input into a URL: explicit URLs and bare domains load directly,
/// anything else is a search.
fn resolve_input(input: &str, search_base: &str) -> String {
    let input = input.trim();
    if input.is_empty() {
        return HOME.to_owned();
    }
    if input.eq_ignore_ascii_case("about:settings")
        || input.eq_ignore_ascii_case("chrome://settings")
        || input.eq_ignore_ascii_case("edge://settings")
        || input.eq_ignore_ascii_case("hypr://settings")
        || input.eq_ignore_ascii_case("settings")
    {
        return "about:settings".to_owned();
    }
    let has_scheme = ["http://", "https://", "file://", "about:", "chrome-extension://", "edge://"]
        .iter()
        .any(|s| input.starts_with(s));
    if has_scheme {
        return input.to_owned();
    }
    let looks_like_host = !input.contains(' ')
        && (input.contains('.') || input.starts_with("localhost"))
        && !input.ends_with('.');
    if looks_like_host {
        format!("https://{input}")
    } else {
        let query: String = input
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                b' ' => "+".to_owned(),
                _ => format!("%{b:02X}"),
            })
            .collect();
        format!("{search_base}{query}")
    }
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(vk.0 as i32) < 0 }
}

fn shortcut_for(vk: u32, ctrl: bool, shift: bool, alt: bool) -> Option<Shortcut> {
    let key = |v: VIRTUAL_KEY| vk == v.0 as u32;
    Some(match (ctrl, shift, alt) {
        (true, false, false) if vk == b'T' as u32 => Shortcut::NewTab,
        (true, false, false) if vk == b'W' as u32 => Shortcut::CloseTab,
        (true, false, false) if vk == b'L' as u32 => Shortcut::FocusAddress,
        (true, false, false) if vk == b'R' as u32 => Shortcut::Reload,
        (true, false, false) if (b'1' as u32..=b'9' as u32).contains(&vk) => {
            Shortcut::SelectTab((vk - b'1' as u32) as usize)
        }
        (true, false, false) if key(VK_TAB) => Shortcut::NextTab,
        (true, true, false) if key(VK_TAB) => Shortcut::PrevTab,
        (false, false, true) if key(VK_LEFT) => Shortcut::Back,
        (false, false, true) if key(VK_RIGHT) => Shortcut::Forward,
        (false, false, false) if key(VK_F5) => Shortcut::Reload,
        (false, false, false) if key(VK_ESCAPE) => Shortcut::Escape,
        _ => return None,
    })
}

/// Browser shortcuts work even while a page has focus (they're intercepted before the page).
fn install_shortcuts(controller: &ICoreWebView2Controller, proxy: EventLoopProxy<UserEvent>, page: bool) {
    let mut token = 0i64;
    unsafe {
        let _ = controller.add_AcceleratorKeyPressed(
            &AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else {
                    return Ok(());
                };
                let mut kind = COREWEBVIEW2_KEY_EVENT_KIND::default();
                args.KeyEventKind(&mut kind)?;
                if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
                    && kind != COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN
                {
                    return Ok(());
                }
                let mut vk = 0u32;
                args.VirtualKey(&mut vk)?;
                let shortcut = shortcut_for(vk, key_down(VK_CONTROL), key_down(VK_SHIFT), key_down(VK_MENU));
                // Pages keep Escape (closing their own dialogs, fullscreen video, ...).
                if let Some(s) = shortcut.filter(|s| !(page && matches!(s, Shortcut::Escape))) {
                    args.SetHandled(true)?;
                    let _ = proxy.send_event(UserEvent::Shortcut(s));
                }
                Ok(())
            })),
            &mut token,
        );
    }
}

impl Browser {
    fn logical_size(&self) -> (f64, f64) {
        let size = self.window.inner_size();
        let scale = self.window.scale_factor();
        (size.width as f64 / scale, size.height as f64 / scale)
    }

    /// Width of the vertical tab sidebar (0 with horizontal tabs).
    fn sidebar_w(&self) -> f64 {
        let a = &self.settings.appearance;
        match (a.vertical_tabs, a.sidebar_collapsed) {
            (false, _) => 0.0,
            _ if a.sidebar_autohide => SIDEBAR_STRIP_W,
            (true, true) => SIDEBAR_COLLAPSED_W,
            (true, false) => SIDEBAR_W,
        }
    }

    fn content_rect(&self) -> Rect {
        let (w, h) = self.logical_size();
        let (left, top) = if self.settings.appearance.vertical_tabs {
            (self.sidebar_w(), TOOLBAR_ROW_H)
        } else {
            (0.0, TOOLBAR_H)
        };
        Rect {
            position: LogicalPosition::new(left, top).into(),
            size: LogicalSize::new((w - left).max(1.0), (h - top).max(1.0)).into(),
        }
    }

    /// The toolbar covers the whole window, behind the page: it draws the toolbar row and the
    /// tab strip or vertical tab sidebar around the content area.
    fn toolbar_rect(&self) -> Rect {
        let (w, h) = self.logical_size();
        Rect {
            position: LogicalPosition::new(0.0, 0.0).into(),
            size: LogicalSize::new(w, h).into(),
        }
    }

    fn flyout_rect(&self) -> Rect {
        let (_, h) = self.logical_size();
        Rect {
            position: LogicalPosition::new(0.0, TOOLBAR_ROW_H).into(),
            size: LogicalSize::new(SIDEBAR_W, (h - TOOLBAR_ROW_H).max(1.0)).into(),
        }
    }

    /// Shows / hides the auto-hide tab panel. It's a small separate webview so it can slide
    /// *over* the page (the toolbar itself sits behind it); kept alive once created.
    fn sidebar_peek(&mut self, show: bool) {
        if !show {
            if let Some(f) = &self.flyout {
                let _ = f.set_visible(false);
            }
            self.flyout_visible = false;
            return;
        }
        if !(self.settings.appearance.vertical_tabs && self.settings.appearance.sidebar_autohide) {
            return;
        }
        if self.flyout.is_none() {
            let proxy = self.proxy.clone();
            let rect = self.flyout_rect();
            let built = profile_builder(&mut self.web_context, &self.settings.memory)
                .with_initialization_script("window.__CHICKEN_FLYOUT = true;")
                .with_html(include_str!("toolbar.html"))
                .with_transparent(true)
                .with_background_color((0, 0, 0, 0))
                .with_bounds(rect)
                .with_ipc_handler(move |req| {
                    let _ = proxy.send_event(UserEvent::Ipc(req.body().clone()));
                })
                .build_as_child(&self.window);
            match built {
                Ok(w) => self.flyout = Some(w),
                Err(e) => {
                    tracing::warn!("couldn't create the tab panel: {e}");
                    return;
                }
            }
        }
        let rect = self.flyout_rect();
        if let Some(f) = &self.flyout {
            let _ = f.set_bounds(rect);
            let _ = f.set_visible(true);
            let hwnd = f.hwnd();
            unsafe {
                let _ = SetWindowPos(HWND(hwnd.0 as _), Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            let _ = f.evaluate_script("window.hb && hb.peek && hb.peek()");
        }
        self.flyout_visible = true;
        self.sync_toolbar();
    }

    /// Writes the open tabs (and which one is active) when they differ from the last write.
    fn save_session(&mut self) {
        // Closing the last tab empties the list on the way out: keep the previous session then.
        if self.tabs.is_empty() {
            return;
        }
        // Sites like YouTube change the address without a page load, so `t.url` goes stale:
        // ask the webview for its real current address.
        let live = |t: &Tab| -> String {
            if !t.discarded && !t.settings_page.get() {
                if let Ok(core) = unsafe { t.webview.controller().CoreWebView2() } {
                    let mut src = windows::core::PWSTR::null();
                    if unsafe { core.Source(&mut src) }.is_ok() {
                        let s = webview2_com::take_pwstr(src);
                        if s.starts_with("http://") || s.starts_with("https://") {
                            return s;
                        }
                    }
                }
            }
            t.url.clone()
        };
        let all: Vec<String> = self.tabs.iter().map(live).collect();
        let urls: Vec<&str> = all.iter().map(String::as_str).filter(|u| !u.is_empty() && *u != "about:blank").collect();
        let active_url = all.get(self.active).map(String::as_str).unwrap_or("");
        let active = urls.iter().position(|u| *u == active_url).unwrap_or(0);
        let json = json!({ "active": active, "urls": urls }).to_string();
        if json != self.session_saved && std::fs::write(&self.session_path, &json).is_ok() {
            self.session_saved = json;
        }
    }

    /// Reopens the tabs from the previous run.
    fn restore_session(&mut self) {
        #[derive(Deserialize)]
        struct Session {
            active: usize,
            urls: Vec<String>,
        }
        let Some(s) = std::fs::read_to_string(&self.session_path)
            .ok()
            .and_then(|t| serde_json::from_str::<Session>(&t).ok())
        else {
            return;
        };
        let active = s.active.min(s.urls.len().saturating_sub(1));
        for (i, url) in s.urls.iter().enumerate() {
            if i == active || url == "about:settings" {
                self.open_tab(url);
            } else {
                // Only the visible tab loads now; the others are opened unloaded and load when
                // first selected (several pages loading at once, hidden, also stalled video pages).
                self.open_tab_in("about:blank", true);
                if let Some(tab) = self.tabs.last_mut() {
                    tab.url = url.clone();
                    tab.discarded = true;
                }
            }
        }
        if !self.tabs.is_empty() {
            self.switch_to(active);
        }
    }

    fn ram_rect(&self) -> Rect {
        let (_, h) = self.logical_size();
        Rect {
            position: LogicalPosition::new(8.0, (h - 30.0).max(0.0)).into(),
            size: LogicalSize::new(110.0, 22.0).into(),
        }
    }

    fn build_ram(&mut self) {
        let rect = self.ram_rect();
        let built = profile_builder(&mut self.web_context, &self.settings.memory)
            .with_html(include_str!("ram.html"))
            .with_initialization_script(theme_init_script(&self.user_css))
            .with_initialization_script(internal_mode_init(&self.mode_css))
            .with_transparent(true)
            .with_background_color((0, 0, 0, 0))
            .with_bounds(rect)
            .build_as_child(&self.window);
        match built {
            Ok(w) => self.ram = Some(w),
            Err(e) => tracing::warn!("couldn't create the RAM readout: {e}"),
        }
    }

    /// Total private memory of the browser (its own process plus every WebView2 process).
    fn update_ram(&self) {
        let Some(ram) = &self.ram else { return };
        let own = unsafe {
            use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
            use windows::Win32::System::Threading::GetCurrentProcess;
            let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
            let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
            if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, size).as_bool() {
                c.PrivateUsage as u64
            } else {
                0
            }
        };
        let webview = self
            .tabs
            .first()
            .and_then(|t| unsafe { t.webview.controller().CoreWebView2() }.ok())
            .and_then(|core| process_memory(&core))
            .map_or(0, |(bytes, _)| bytes);
        let total = own + webview;
        let text = if total >= 1 << 30 {
            format!("{:.1} GB", total as f64 / (1u64 << 30) as f64)
        } else {
            format!("{} MB", total >> 20)
        };
        let _ = ram.evaluate_script(&format!("ram.set({})", serde_json::to_string(&text).unwrap_or_default()));
        unsafe {
            let _ = SetWindowPos(HWND(ram.hwnd().0 as _), Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    fn build_suggest(&mut self) {
        let proxy = self.proxy.clone();
        let built = profile_builder(&mut self.web_context, &self.settings.memory)
            .with_html(include_str!("suggest.html"))
            .with_initialization_script(theme_init_script(&self.user_css))
            .with_initialization_script(internal_mode_init(&self.mode_css))
            .with_transparent(true)
            .with_background_color((0, 0, 0, 0))
            .with_visible(false)
            .with_bounds(Rect {
                position: LogicalPosition::new(0.0, 0.0).into(),
                size: LogicalSize::new(1.0, 1.0).into(),
            })
            .with_ipc_handler(move |req| {
                let _ = proxy.send_event(UserEvent::Ipc(req.body().clone()));
            })
            .build_as_child(&self.window);
        match built {
            Ok(w) => self.suggest = Some(w),
            Err(e) => tracing::warn!("couldn't create the address bar suggestions: {e}"),
        }
    }

    fn show_suggestions(&mut self, text: &str, x: f64, w: f64) {
        let items = self.history.query(text, SUGGEST_MAX);
        if items.is_empty() || self.suggest.is_none() {
            self.hide_suggestions();
            return;
        }
        let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".into());
        let _ = self.toolbar.evaluate_script(&format!("window.hb && hb.suggestions({json})"));
        let rect = Rect {
            position: LogicalPosition::new(x, TOOLBAR_ROW_H - 6.0).into(),
            size: LogicalSize::new(w.max(200.0), items.len() as f64 * SUGGEST_ROW_H + SUGGEST_PAD).into(),
        };
        if let Some(s) = &self.suggest {
            let _ = s.set_bounds(rect);
            let _ = s.evaluate_script(&format!("sg.set({json}); sg.sel(-1);"));
            let _ = s.set_visible(true);
            unsafe {
                let _ = SetWindowPos(HWND(s.hwnd().0 as _), Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
        self.suggest_visible = true;
    }

    fn hide_suggestions(&mut self) {
        if !self.suggest_visible {
            return;
        }
        if let Some(s) = &self.suggest {
            let _ = s.set_visible(false);
        }
        self.suggest_visible = false;
    }

    /// Keeps the toolbar under the page, popups and menus (it's full-window now).
    fn keep_toolbar_behind(&self) {
        let hwnd = self.toolbar.hwnd();
        unsafe {
            let _ = SetWindowPos(HWND(hwnd.0 as _), Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// A live light/dark override for the chrome: only "match website colors" makes one (the
    /// page's own tone). Otherwise `None`: browser.css already carries the palette the master
    /// theme provider picked for the browser (`app_modes = browser:…`).
    fn effective_mode(&self) -> Option<chickenwm_core::theme::ThemeMode> {
        use chickenwm_core::theme::ThemeMode;
        if self.settings.appearance.match_website {
            if let Some(dark) = self.tabs.get(self.active).and_then(|t| t.tone) {
                let page = if dark { ThemeMode::Dark } else { ThemeMode::Light };
                // No override needed when the page already matches the browser's own mode.
                if page != self.system_theme.mode_for("browser") {
                    return Some(page);
                }
            }
        }
        None
    }

    /// Sets the browser's light/dark mode through the master theme provider.
    fn set_browser_mode(&mut self, mode: chickenwm_core::theme::AppMode) {
        match chickenwm_core::theme::set_app_mode("browser", mode) {
            Ok(theme) => self.system_theme = theme,
            Err(e) => tracing::warn!("couldn't save the browser's theme mode: {e}"),
        }
        self.apply_profile();
        self.refresh_chrome_theme();
        self.push_settings_state();
    }

    /// Sends the light/dark override to the toolbar, menu and settings pages if it changed.
    fn refresh_chrome_theme(&mut self) {
        let css = self.effective_mode().map(|m| mode_css(&self.system_theme, m)).unwrap_or_default();
        if css == self.mode_css {
            return;
        }
        let script = mode_apply_js(&css);
        let _ = self.toolbar.evaluate_script(&script);
        if let Some(f) = &self.flyout {
            let _ = f.evaluate_script(&script);
        }
        if let Some(s) = &self.suggest {
            let _ = s.evaluate_script(&script);
        }
        for tab in self.tabs.iter().filter(|t| t.settings_page.get()) {
            let _ = tab.webview.evaluate_script(&script);
        }
        if let Some(popup) = self.popup.as_ref().filter(|p| p.extension_id == "menu") {
            let _ = popup.webview.evaluate_script(&script);
        }
        let bg = if css.is_empty() { chrome_bg(&self.user_css) } else { chrome_bg(&css) };
        let _ = self.toolbar.set_background_color(bg);
        self.mode_css = css;
    }

    /// Color scheme websites see, from the configured theme (never from page tone, which would
    /// feed back into itself).
    fn apply_profile(&self) {
        let dark = Some(self.system_theme.mode_for("browser") == chickenwm_core::theme::ThemeMode::Dark);
        if let Some(tab) = self.tabs.first() {
            apply_profile_prefs(&tab.webview.controller(), dark, &self.settings.privacy.tracking_prevention);
        }
    }

    /// Measures the active page's background for "match website colors".
    fn measure_tone(&self) {
        // Same moments (load, switch, settings change, periodic tick) feed the ambient glow.
        self.measure_ambient();
        if !self.settings.appearance.match_website {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.settings_page.get() || tab.discarded {
            return;
        }
        let (id, proxy) = (tab.id, self.proxy.clone());
        let _ = tab.webview.evaluate_script_with_callback(PAGE_TONE_JS, move |result| {
            match result.trim() {
                "true" => { let _ = proxy.send_event(UserEvent::PageTone(id, true)); }
                "false" => { let _ = proxy.send_event(UserEvent::PageTone(id, false)); }
                _ => {}
            }
        });
    }

    /// Samples the active page's colors for the ambient glow from a capture of what is actually
    /// on screen (images and video included); falls back to CSS colors if the capture fails.
    fn measure_ambient(&self) {
        if !self.settings.appearance.ambient {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.settings_page.get() || tab.discarded {
            return;
        }
        if self.ambient_busy.get().is_some_and(|at| at.elapsed() < Duration::from_secs(5)) {
            return;
        }
        let Some(stream) = (unsafe { SHCreateMemStream(None) }) else {
            self.measure_ambient_css();
            return;
        };
        self.ambient_busy.set(Some(Instant::now()));
        let (id, proxy, busy, read_from) = (tab.id, self.proxy.clone(), self.ambient_busy.clone(), stream.clone());
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |result| {
            busy.set(None);
            match result.ok().and_then(|_| read_stream(&read_from)) {
                Some(bytes) => {
                    // Decoding and quantizing happens off the UI thread.
                    std::thread::spawn(move || match ambient_from_jpeg(&bytes) {
                        Some(colors) => { let _ = proxy.send_event(UserEvent::PageAmbient(id, colors)); }
                        None => { let _ = proxy.send_event(UserEvent::AmbientFallback(id)); }
                    });
                }
                None => { let _ = proxy.send_event(UserEvent::AmbientFallback(id)); }
            }
            Ok(())
        }));
        let started = match unsafe { tab.webview.controller().CoreWebView2() } {
            Ok(core) => unsafe {
                core.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_JPEG, &stream, &handler).is_ok()
            },
            Err(_) => false,
        };
        if !started {
            self.ambient_busy.set(None);
            self.measure_ambient_css();
        }
    }

    /// CSS-only sampling (backgrounds, theme-color, link color): no pixels needed.
    fn measure_ambient_css(&self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.settings_page.get() || tab.discarded {
            return;
        }
        let (id, proxy) = (tab.id, self.proxy.clone());
        let _ = tab.webview.evaluate_script_with_callback(PAGE_AMBIENT_JS, move |result| {
            if let Ok(colors) = serde_json::from_str::<Vec<[u8; 3]>>(result.trim()) {
                let _ = proxy.send_event(UserEvent::PageAmbient(id, colors));
            }
        });
    }

    fn popup_rect(&self) -> Rect {
        let (w, h) = self.logical_size();
        let width = POPUP_W.min(w - 20.0);
        let height = POPUP_H.min((h - TOOLBAR_ROW_H - 10.0).max(100.0));
        Rect {
            position: LogicalPosition::new((w - width - 12.0).max(0.0), TOOLBAR_ROW_H).into(),
            size: LogicalSize::new(width, height).into(),
        }
    }

    fn menu_rect(&self) -> Rect {
        let (w, h) = self.logical_size();
        let width = 260.0f64.min(w - 20.0);
        let height = 440.0f64.min((h - TOOLBAR_ROW_H - 10.0).max(100.0));
        Rect {
            position: LogicalPosition::new((w - width - 8.0).max(0.0), TOOLBAR_ROW_H).into(),
            size: LogicalSize::new(width, height).into(),
        }
    }

    fn layout(&self) {
        let _ = self.toolbar.set_bounds(self.toolbar_rect());
        self.keep_toolbar_behind();
        if let Some(r) = &self.ram {
            let _ = r.set_bounds(self.ram_rect());
        }
        if let (Some(f), true) = (&self.flyout, self.flyout_visible) {
            let _ = f.set_bounds(self.flyout_rect());
        }
        if let Some(tab) = self.tabs.get(self.active) {
            let _ = tab.webview.set_bounds(self.content_rect());
        }
        if let Some(popup) = &self.popup {
            let bounds = if popup.extension_id == "menu" {
                self.menu_rect()
            } else {
                self.popup_rect()
            };
            let _ = popup.webview.set_bounds(bounds);
        }
    }

    fn sync_toolbar(&self) {
        let active = self.tabs.get(self.active);
        let state = json!({
            "tabs": self.tabs.iter().enumerate().map(|(i, t)| json!({
                "id": t.id, "title": t.title, "url": t.url, "loading": t.loading, "active": i == self.active,
                "icon": t.favicon, "asleep": t.suspended || t.discarded,
            })).collect::<Vec<_>>(),
            "layout": {
                "vertical": self.settings.appearance.vertical_tabs,
                "autohide": self.settings.appearance.sidebar_autohide,
                "collapsed": self.settings.appearance.sidebar_collapsed,
                "sidebar": self.sidebar_w(),
                "animations": self.settings.appearance.animations,
            },
            "fx": {
                "gradient": self.settings.appearance.gradient,
                "gradient_strength": self.settings.appearance.gradient_strength,
                "noise": self.settings.appearance.noise,
                "noise_strength": self.settings.appearance.noise_strength,
                "ambient": self.settings.appearance.ambient,
                "ambient_strength": self.settings.appearance.ambient_strength,
            },
            // The settings page (and unloaded tabs) have no colors of their own: the glow fades out.
            "ambient": active
                .filter(|t| !t.settings_page.get() && !t.discarded)
                .map(|t| t.ambient.as_slice())
                .unwrap_or(&[]),
            "site_adblock": active
                .and_then(|t| settings::site_of(&t.url))
                .map(|site| !self.allowlist.borrow().contains(&site)),
            "url": active.map(|t| t.url.as_str()).unwrap_or(""),
            "blocked": self.blocked,
            "adblock": self.engine.borrow().is_some(),
            "extensions": self.extensions,
            "popup": self.popup.as_ref().map(|p| p.extension_id.as_str()),
        });
        let script = format!("window.hb && hb.render({state})");
        let _ = self.toolbar.evaluate_script(&script);
        if let Some(f) = &self.flyout {
            let _ = f.evaluate_script(&script);
        }
        let title = active
            .map(|t| if t.title.is_empty() { t.url.clone() } else { t.title.clone() })
            .unwrap_or_default();
        self.window.set_title(&format!("{title} — chickenbrowser"));
    }

    fn open_tab(&mut self, url: &str) {
        self.open_tab_in(url, false);
    }

    /// `background`: create the tab hidden and leave the current tab visible and active.
    fn open_tab_in(&mut self, url: &str, background: bool) {
        let id = self.next_id;
        self.next_id += 1;
        let is_settings = url == "about:settings";
        let page_url = Rc::new(RefCell::new(url.to_owned()));
        let settings_page = Rc::new(Cell::new(is_settings));
        let bounds = self.content_rect();
        let (p_title, p_load, p_new, p_ipc) = (
            self.proxy.clone(),
            self.proxy.clone(),
            self.proxy.clone(),
            self.proxy.clone(),
        );
        let ipc_allowed = settings_page.clone();

        let mut builder = profile_builder(&mut self.web_context, &self.settings.memory)
            .with_initialization_script(theme_init_script(&self.user_css))
            .with_initialization_script(internal_mode_init(&self.mode_css))
            .with_bounds(bounds)
            .with_devtools(true)
            .with_ipc_handler(move |req| {
                if ipc_allowed.get() {
                    let _ = p_ipc.send_event(UserEvent::Ipc(req.body().clone()));
                }
            })
            .with_document_title_changed_handler(move |title| {
                let _ = p_title.send_event(UserEvent::Title(id, title));
            })
            .with_on_page_load_handler(move |event, url| {
                let started = matches!(event, PageLoadEvent::Started);
                let _ = p_load.send_event(UserEvent::PageLoad { tab: id, url, started });
            })
            .with_new_window_req_handler(move |url, features| {
                // Real popups (window.open with a size or position: sign-in, payment, exam
                // windows) open as proper WebView2 popups so window.opener and form POSTs keep
                // working. Ordinary target=_blank links open as tabs.
                if features.size.is_some() || features.position.is_some() {
                    return NewWindowResponse::Allow;
                }
                let _ = p_new.send_event(UserEvent::OpenTab(url));
                NewWindowResponse::Deny
            });

        if is_settings {
            builder = builder.with_html(include_str!("settings.html"));
        } else {
            builder = builder.with_url(url);
        }

        // Install extensions once: re-adding an installed extension replaces it (restarting its
        // service worker), so only the first webview of the session passes the folder.
        if !self.extensions_loaded {
            builder = builder.with_extensions_path(&self.paths.extensions);
        }
        let webview = match builder.build_as_child(&self.window) {
            Ok(webview) => webview,
            Err(e) => {
                tracing::error!("failed to create tab: {e}");
                return;
            }
        };
        let controller = webview.controller();
        if let Err(e) = adblock_filter::install(&controller, self.engine.clone(), self.allowlist.clone(), page_url.clone(), self.proxy.clone(), id) {
            tracing::warn!("adblock hook failed for tab {id}: {e}");
        }
        install_shortcuts(&controller, self.proxy.clone(), true);
        install_diagnostics(&controller, id);
        install_favicon(&controller, id, self.proxy.clone());
        if !self.extensions_loaded {
            self.extensions_loaded = true;
        }

        if !background {
            self.send_to_background(self.active);
        }
        self.tabs.push(Tab {
            id,
            webview,
            title: if is_settings { "Settings".to_owned() } else { String::new() },
            url: url.to_owned(),
            loading: !is_settings,
            page_url,
            hidden_since: None,
            suspended: false,
            discarded: false,
            settings_page,
            favicon: String::new(),
            tone: None,
            ambient: Vec::new(),
        });
        if background {
            // Opened behind the current tab without disturbing it (session restore).
            self.send_to_background(self.tabs.len() - 1);
        } else {
            self.active = self.tabs.len() - 1;
        }
        if self.tabs.len() == 1 {
            self.apply_profile();
        }
        self.keep_toolbar_behind();
        self.sync_toolbar();
        self.refresh_chrome_theme();
    }

    fn switch_to(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if index != self.active {
            self.send_to_background(self.active);
        }
        self.active = index;
        self.bring_to_foreground(index);
        self.sync_toolbar();
        self.refresh_chrome_theme();
        self.measure_tone();
    }

    /// Hides a tab that is no longer the visible one and starts its memory-saving clock.
    fn send_to_background(&mut self, index: usize) {
        let low = self.settings.memory.low_memory_background;
        if let Some(tab) = self.tabs.get_mut(index) {
            let _ = tab.webview.set_visible(false);
            tab.hidden_since = Some(Instant::now());
            if low {
                let _ = tab.webview.set_memory_usage_level(MemoryUsageLevel::Low);
            }
        }
    }

    /// Shows a tab, waking it up (and reloading it if it was unloaded).
    fn bring_to_foreground(&mut self, index: usize) {
        let bounds = self.content_rect();
        let Some(tab) = self.tabs.get_mut(index) else { return };
        tab.hidden_since = None;
        let _ = tab.webview.set_memory_usage_level(MemoryUsageLevel::Normal);
        let _ = tab.webview.set_bounds(bounds);
        let _ = tab.webview.set_visible(true);
        if std::mem::take(&mut tab.suspended) {
            if let Ok(core) = unsafe { tab.webview.controller().CoreWebView2() } {
                if let Ok(c3) = core.cast::<ICoreWebView2_3>() {
                    let _ = unsafe { c3.Resume() };
                }
            }
        }
        if std::mem::take(&mut tab.discarded) {
            tab.loading = true;
            if tab.settings_page.get() {
                show_settings_page(&tab.webview);
            } else {
                let _ = tab.webview.load_url(&tab.url);
            }
        }
        let _ = tab.webview.focus();
    }

    /// Suspends, then unloads, tabs that have been in the background long enough. Tabs playing
    /// audio and the settings page are left alone.
    fn memory_tick(&mut self) {
        let m = &self.settings.memory;
        let suspend_after = Duration::from_secs(m.suspend_after_secs);
        let discard_after = Duration::from_secs(m.discard_after_mins * 60);
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            let Some(since) = tab.hidden_since else { continue };
            if i == self.active || tab.settings_page.get() {
                continue;
            }
            let Ok(core) = (unsafe { tab.webview.controller().CoreWebView2() }) else { continue };
            let playing_audio = core.cast::<ICoreWebView2_8>().is_ok_and(|c8| {
                let mut playing = BOOL(0);
                unsafe { c8.IsDocumentPlayingAudio(&mut playing) }.is_ok() && playing.as_bool()
            });
            if playing_audio {
                continue;
            }
            let hidden = since.elapsed();
            if m.discard_after_mins > 0 && !tab.discarded && hidden >= discard_after {
                tracing::info!("unloading background tab {} ({})", tab.id, tab.url);
                tab.discarded = true;
                // Suspend the blank page on a later tick.
                tab.suspended = false;
                tab.loading = false;
                let _ = tab.webview.load_url("about:blank");
                continue;
            }
            if m.suspend_after_secs > 0 && !tab.suspended && !tab.loading && hidden >= suspend_after {
                if let Ok(c3) = core.cast::<ICoreWebView2_3>() {
                    let id = tab.id;
                    let handler = TrySuspendCompletedHandler::create(Box::new(move |result, ok| {
                        if result.is_err() || !ok {
                            tracing::debug!("tab {id} could not be suspended");
                        }
                        Ok(())
                    }));
                    tab.suspended = unsafe { c3.TrySuspend(&handler) }.is_ok();
                }
            }
        }
    }

    /// Sends current settings and live memory usage to every open settings page.
    fn push_settings_state(&self) {
        let usage = self
            .tabs
            .first()
            .and_then(|t| unsafe { t.webview.controller().CoreWebView2() }.ok())
            .and_then(|core| process_memory(&core));
        let state = json!({
            "search_engine": self.settings.search_engine,
            "memory": self.settings.memory,
            "appearance": self.settings.appearance,
            "system_dark": self.system_theme.mode == chickenwm_core::theme::ThemeMode::Dark,
            "app_mode": self.system_theme.app_mode("browser").as_str(),
            "privacy": self.settings.privacy,
            "restart_needed": self.launch_memory.needs_restart(&self.settings.memory),
            "usage_mb": usage.map(|(bytes, _)| bytes / (1024 * 1024)),
            "processes": usage.map(|(_, n)| n),
            "toolbar_separate": self._toolbar_context.is_some(),
            "tabs": self.tabs.len(),
            "asleep": self.tabs.iter().filter(|t| t.suspended || t.discarded).count(),
        });
        for tab in self.tabs.iter().filter(|t| t.settings_page.get()) {
            let _ = tab.webview.evaluate_script(&format!("window.hbSettings && hbSettings.render({state})"));
        }
    }

    fn set_appearance(&mut self, appearance: settings::AppearanceSettings) {
        let relayout = appearance.vertical_tabs != self.settings.appearance.vertical_tabs
            || appearance.sidebar_collapsed != self.settings.appearance.sidebar_collapsed;
        let theme_changed = appearance.theme != self.settings.appearance.theme;
        self.settings.appearance = appearance;
        self.save_settings();
        if relayout {
            self.layout();
        }
        self.sync_toolbar();
        if theme_changed {
            self.apply_profile();
        }
        self.refresh_chrome_theme();
        self.measure_tone();
        self.push_settings_state();
    }

    fn toggle_site_adblock(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(site) = settings::site_of(&tab.url) else { return };
        let list = &mut self.settings.privacy.adblock_allowlist;
        if let Some(i) = list.iter().position(|s| *s == site) {
            list.remove(i);
        } else {
            list.push(site);
        }
        *self.allowlist.borrow_mut() = list.clone();
        self.save_settings();
        let _ = self.tabs[self.active].webview.reload();
        self.sync_toolbar();
        self.push_settings_state();
    }

    fn save_settings(&self) {
        settings::save(&self.paths.root, &self.settings);
    }

    /// Returns false when the last tab closed (the browser should exit).
    fn close_tab(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return true;
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            return false;
        }
        let next = if index < self.active || self.active >= self.tabs.len() {
            self.active.saturating_sub(1).min(self.tabs.len() - 1)
        } else {
            self.active
        };
        self.active = usize::MAX; // force switch_to to show it
        self.switch_to(next);
        true
    }

    fn tab_index(&self, id: u32) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn with_active_core(&self, f: impl FnOnce(&ICoreWebView2)) {
        if let Some(tab) = self.tabs.get(self.active) {
            if let Ok(core) = unsafe { tab.webview.controller().CoreWebView2() } {
                f(&core);
            }
        }
    }

    fn close_popup(&mut self) {
        if self.popup.take().is_some() {
            self.sync_toolbar();
        }
    }

    fn toggle_extension_popup(&mut self, extension_id: String) {
        if self.popup.as_ref().is_some_and(|p| p.extension_id == extension_id) {
            self.close_popup();
            return;
        }
        // The toolbar click itself made the open popup lose focus and close a moment ago.
        if self
            .popup_closed
            .as_ref()
            .is_some_and(|(id, at)| *id == extension_id && at.elapsed() < Duration::from_millis(200))
        {
            self.popup_closed = None;
            return;
        }
        self.close_popup();
        let Some(ext) = self.extensions.iter().find(|e| e.id == extension_id) else {
            tracing::warn!("toggle_extension_popup: extension {extension_id} not found in extensions list");
            return;
        };
        let page = ext.popup.clone().unwrap_or_else(|| "popup.html".to_owned());
        let url = format!("chrome-extension://{}/{}", ext.id, page);
        let bounds = self.popup_rect();
        let p_new = self.proxy.clone();
        let built = profile_builder(&mut self.web_context, &self.settings.memory)
            .with_url(&url)
            .with_bounds(bounds)
            .with_devtools(true)
            .with_focused(true)
            .with_new_window_req_handler(move |target_url, _| {
                let _ = p_new.send_event(UserEvent::OpenTab(target_url));
                NewWindowResponse::Deny
            })
            .build_as_child(&self.window);
        match built {
            Ok(webview) => {
                let controller = webview.controller();
                install_shortcuts(&controller, self.proxy.clone(), false);
                let proxy = self.proxy.clone();
                let mut token = 0i64;
                unsafe {
                    let _ = controller.add_LostFocus(
                        &FocusChangedEventHandler::create(Box::new(move |_, _| {
                            let _ = proxy.send_event(UserEvent::PopupLostFocus);
                            Ok(())
                        })),
                        &mut token,
                    );
                    let hwnd = webview.hwnd();
                    let _ = SetWindowPos(HWND(hwnd.0 as _), Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
                }
                let _ = webview.focus();
                self.popup = Some(Popup {
                    extension_id,
                    webview,
                    opened_at: Instant::now(),
                });
                self.sync_toolbar();
            }
            Err(e) => tracing::error!("failed to open extension popup {url}: {e}"),
        }
    }

    fn toggle_menu(&mut self) {
        if self.popup.as_ref().is_some_and(|p| p.extension_id == "menu") {
            self.close_popup();
            return;
        }
        if self
            .popup_closed
            .as_ref()
            .is_some_and(|(id, at)| id == "menu" && at.elapsed() < Duration::from_millis(200))
        {
            self.popup_closed = None;
            return;
        }
        self.close_popup();

        let bounds = self.menu_rect();
        let p_ipc = self.proxy.clone();
        let built = profile_builder(&mut self.web_context, &self.settings.memory)
            .with_html(include_str!("menu.html"))
            .with_initialization_script(theme_init_script(&self.user_css))
            .with_initialization_script(internal_mode_init(&self.mode_css))
            .with_bounds(bounds)
            .with_devtools(true)
            .with_focused(true)
            .with_ipc_handler(move |req| {
                let _ = p_ipc.send_event(UserEvent::Ipc(req.body().clone()));
            })
            .build_as_child(&self.window);
        match built {
            Ok(webview) => {
                let controller = webview.controller();
                install_shortcuts(&controller, self.proxy.clone(), false);
                let proxy = self.proxy.clone();
                let mut token = 0i64;
                unsafe {
                    let _ = controller.add_LostFocus(
                        &FocusChangedEventHandler::create(Box::new(move |_, _| {
                            let _ = proxy.send_event(UserEvent::PopupLostFocus);
                            Ok(())
                        })),
                        &mut token,
                    );
                    let hwnd = webview.hwnd();
                    let _ = SetWindowPos(HWND(hwnd.0 as _), Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
                }
                if let Some(tab) = self.tabs.get(self.active) {
                    let controller = tab.webview.controller();
                    let mut zoom: f64 = 1.0;
                    if unsafe { controller.ZoomFactor(&mut zoom) }.is_ok() {
                        let _ = webview.evaluate_script(&format!("window.setZoom && setZoom({zoom});"));
                    }
                }
                let _ = webview.focus();
                self.popup = Some(Popup {
                    extension_id: "menu".to_owned(),
                    webview,
                    opened_at: Instant::now(),
                });
                self.sync_toolbar();
            }
            Err(e) => tracing::error!("failed to open menu popup: {e}"),
        }
    }

    fn refresh_extensions(&self) {
        if let Some(tab) = self.tabs.first() {
            if let Err(e) = extensions::query_installed(
                &tab.webview.controller(),
                self.paths.extensions.clone(),
                self.paths.profile.clone(),
                self.proxy.clone(),
            ) {
                tracing::error!("failed to query installed extensions: {e}");
            }
        }
    }

    /// Returns false to exit.
    fn handle(&mut self, event: UserEvent) -> bool {
        match event {
            UserEvent::Ipc(body) => {
                let Ok(cmd) = serde_json::from_str::<ToolbarCmd>(&body) else {
                    return true;
                };
                match cmd {
                    ToolbarCmd::Ready => {
                        self.sync_toolbar();
                        // The toolbar (re)loaded: resend the light/dark override.
                        self.mode_css = String::from("\u{1}");
                        self.refresh_chrome_theme();
                        if let Ok(css) = std::fs::read_to_string(&self.paths.css) {
                            let script = format!("window.hb && hb.setUserStyle({});", serde_json::to_string(&css).unwrap_or_default());
                            let _ = self.toolbar.evaluate_script(&script);
                        }
                        self.refresh_extensions();
                    }
                    ToolbarCmd::Suggest { text, x, w } => self.show_suggestions(&text, x, w),
                    ToolbarCmd::SuggestSel { sel } => {
                        if let Some(s) = self.suggest.as_ref().filter(|_| self.suggest_visible) {
                            let _ = s.evaluate_script(&format!("sg.sel({sel})"));
                        }
                    }
                    ToolbarCmd::SuggestHide => self.hide_suggestions(),
                    ToolbarCmd::Navigate { input } => {
                        self.hide_suggestions();
                        let url = resolve_input(&input, &self.settings.search_engine);
                        if let Some(tab) = self.tabs.get_mut(self.active) {
                            if url == "about:settings" {
                                tab.url = "about:settings".to_owned();
                                tab.title = "Settings".to_owned();
                                tab.settings_page.set(true);
                                show_settings_page(&tab.webview);
                            } else {
                                tab.settings_page.set(false);
                                let _ = tab.webview.load_url(&url);
                            }
                            let _ = tab.webview.focus();
                        }
                        self.sync_toolbar();
                    }
                    ToolbarCmd::NewTab => {
                        self.close_popup();
                        self.open_tab(HOME);
                        let _ = self.toolbar.focus();
                        let _ = self.toolbar.evaluate_script("hb.focusAddress()");
                    }
                    ToolbarCmd::NewWindow => {
                        self.close_popup();
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(exe).spawn();
                        }
                    }
                    ToolbarCmd::CloseTab { id } => {
                        if let Some(i) = self.tab_index(id) {
                            return self.close_tab(i);
                        }
                    }
                    ToolbarCmd::SwitchTab { id } => {
                        self.close_popup();
                        if let Some(i) = self.tab_index(id) {
                            self.switch_to(i);
                        }
                    }
                    ToolbarCmd::Back => self.with_active_core(|c| unsafe {
                        let _ = c.GoBack();
                    }),
                    ToolbarCmd::Forward => self.with_active_core(|c| unsafe {
                        let _ = c.GoForward();
                    }),
                    ToolbarCmd::Reload => self.with_active_core(|c| unsafe {
                        let _ = c.Reload();
                    }),
                    ToolbarCmd::ExtPopup { id } => self.toggle_extension_popup(id),
                    ToolbarCmd::FocusPage => {
                        if let Some(tab) = self.tabs.get(self.active) {
                            let _ = tab.webview.focus();
                        }
                    }
                    ToolbarCmd::ToggleMenu => self.toggle_menu(),
                    ToolbarCmd::OpenSettings => {
                        self.close_popup();
                        self.open_tab("about:settings");
                    }
                    ToolbarCmd::EditCss => {
                        let _ = std::process::Command::new("notepad.exe")
                            .arg(&self.paths.css)
                            .spawn();
                    }
                    ToolbarCmd::ZoomIn => {
                        if let Some(tab) = self.tabs.get(self.active) {
                            let controller = tab.webview.controller();
                            let mut zoom: f64 = 1.0;
                            if unsafe { controller.ZoomFactor(&mut zoom) }.is_ok() {
                                let next: f64 = (zoom + 0.1).min(3.0);
                                let _ = unsafe { controller.SetZoomFactor(next) };
                                if let Some(p) = &self.popup {
                                    let _ = p.webview.evaluate_script(&format!("window.setZoom && setZoom({next});"));
                                }
                            }
                        }
                    }
                    ToolbarCmd::ZoomOut => {
                        if let Some(tab) = self.tabs.get(self.active) {
                            let controller = tab.webview.controller();
                            let mut zoom: f64 = 1.0;
                            if unsafe { controller.ZoomFactor(&mut zoom) }.is_ok() {
                                let next: f64 = (zoom - 0.1).max(0.25);
                                let _ = unsafe { controller.SetZoomFactor(next) };
                                if let Some(p) = &self.popup {
                                    let _ = p.webview.evaluate_script(&format!("window.setZoom && setZoom({next});"));
                                }
                            }
                        }
                    }
                    ToolbarCmd::ZoomReset => {
                        if let Some(tab) = self.tabs.get(self.active) {
                            let controller = tab.webview.controller();
                            let _ = unsafe { controller.SetZoomFactor(1.0) };
                            if let Some(p) = &self.popup {
                                let _ = p.webview.evaluate_script("window.setZoom && setZoom(1.0);");
                            }
                        }
                    }
                    ToolbarCmd::Print => {
                        self.close_popup();
                        if let Some(tab) = self.tabs.get(self.active) {
                            let _ = tab.webview.evaluate_script("window.print();");
                        }
                    }
                    ToolbarCmd::Devtools => {
                        self.close_popup();
                        self.with_active_core(|c| unsafe {
                            let _ = c.OpenDevToolsWindow();
                        });
                    }
                    ToolbarCmd::ClearData => {
                        self.close_popup();
                        self.history.clear();
                        if let Some(tab) = self.tabs.get(self.active) {
                            if let Ok(core) = unsafe { tab.webview.controller().CoreWebView2() } {
                                if let Ok(c13) = core.cast::<ICoreWebView2_13>() {
                                    if let Ok(profile) = unsafe { c13.Profile() } {
                                        if let Ok(p2) = profile.cast::<ICoreWebView2Profile2>() {
                                            let _ = unsafe {
                                                p2.ClearBrowsingDataAll(
                                                    &ClearBrowsingDataCompletedHandler::create(Box::new(|_| {
                                                        tracing::info!("browsing data cleared");
                                                        Ok(())
                                                    })),
                                                )
                                            };
                                        }
                                    }
                                }
                            }
                        }
                    }
                    ToolbarCmd::ProtonPass => {
                        self.close_popup();
                        let id = self
                            .extensions
                            .iter()
                            .find(|e| e.name.contains("Proton Pass"))
                            .map_or_else(|| extensions::PROTON_PASS_ID.to_owned(), |e| e.id.clone());
                        self.toggle_extension_popup(id);
                    }
                    ToolbarCmd::SetSearchEngine { url } => {
                        self.settings.search_engine = url;
                        self.save_settings();
                    }
                    ToolbarCmd::SettingsReady => self.push_settings_state(),
                    ToolbarCmd::SetAppearance { appearance } => self.set_appearance(appearance),
                    ToolbarCmd::SetPrivacy { privacy } => {
                        *self.allowlist.borrow_mut() = privacy.adblock_allowlist.clone();
                        self.settings.privacy = privacy;
                        self.save_settings();
                        self.apply_profile();
                        self.sync_toolbar();
                        self.push_settings_state();
                    }
                    ToolbarCmd::ToggleSiteAdblock => self.toggle_site_adblock(),
                    ToolbarCmd::ToggleSidebar => {
                        let mut a = self.settings.appearance.clone();
                        a.sidebar_collapsed = !a.sidebar_collapsed;
                        self.set_appearance(a);
                    }
                    ToolbarCmd::ToggleDark => {
                        self.close_popup();
                        use chickenwm_core::theme::{AppMode, ThemeMode};
                        let dark_now = self.effective_mode().unwrap_or(self.system_theme.mode_for("browser")) == ThemeMode::Dark;
                        // An explicit choice beats "match website colors".
                        if self.settings.appearance.match_website {
                            let mut a = self.settings.appearance.clone();
                            a.match_website = false;
                            self.set_appearance(a);
                        }
                        self.set_browser_mode(if dark_now { AppMode::Light } else { AppMode::Dark });
                    }
                    ToolbarCmd::SidebarPeek { show } => self.sidebar_peek(show),
                    ToolbarCmd::SetAppMode { mode } => {
                        if let Some(m) = chickenwm_core::theme::AppMode::parse(&mode) {
                            self.set_browser_mode(m);
                        }
                    }
                    ToolbarCmd::FlyoutReady => {
                        if let Some(f) = &self.flyout {
                            let css = serde_json::to_string(&self.user_css).unwrap_or_default();
                            let _ = f.evaluate_script(&format!("window.hb && hb.setUserStyle({css});"));
                            if !self.mode_css.is_empty() {
                                let _ = f.evaluate_script(&mode_apply_js(&self.mode_css));
                            }
                            let _ = f.evaluate_script("window.hb && hb.peek && hb.peek()");
                        }
                        self.sync_toolbar();
                    }
                    ToolbarCmd::ToggleSidebarAutohide => {
                        let mut a = self.settings.appearance.clone();
                        a.sidebar_autohide = !a.sidebar_autohide;
                        self.sidebar_peek(false);
                        self.set_appearance(a);
                    }
                    ToolbarCmd::ToggleVerticalTabs => {
                        self.close_popup();
                        let mut a = self.settings.appearance.clone();
                        a.vertical_tabs = !a.vertical_tabs;
                        self.set_appearance(a);
                    }
                    ToolbarCmd::SetMemoryPreset { preset } => {
                        if let Some(memory) = MemorySettings::preset(&preset) {
                            self.apply_memory_settings(memory);
                        }
                    }
                    ToolbarCmd::SetMemory { mut memory } => {
                        memory.preset = "custom".to_owned();
                        self.apply_memory_settings(memory);
                    }
                    ToolbarCmd::RestartBrowser => {
                        if let Ok(exe) = std::env::current_exe() {
                            // Single-instance mutex: the new process waits for this one to exit.
                            let urls: Vec<String> = self
                                .tabs
                                .iter()
                                .filter(|t| !t.settings_page.get())
                                .map(|t| t.url.clone())
                                .collect();
                            let _ = std::process::Command::new(exe).arg("--wait-for-exit").args(urls).spawn();
                        }
                        return false;
                    }
                    ToolbarCmd::DragWindow => {
                        let _ = self.window.drag_window();
                    }
                    ToolbarCmd::ResizeWindow { edge } => {
                        let direction = match edge.as_str() {
                            "n" => ResizeDirection::North,
                            "nw" => ResizeDirection::NorthWest,
                            "ne" => ResizeDirection::NorthEast,
                            _ => return true,
                        };
                        if !self.window.is_maximized() {
                            let _ = self.window.drag_resize_window(direction);
                        }
                    }
                    ToolbarCmd::ToggleMaximize => self.window.set_maximized(!self.window.is_maximized()),
                    ToolbarCmd::CloseWindow => return false,
                }
            }
            UserEvent::Title(id, title) => {
                if let Some(i) = self.tab_index(id) {
                    // An unloaded tab keeps showing its real title, not about:blank's; the
                    // settings page (NavigateToString) has no useful document title.
                    if !self.tabs[i].discarded && !self.tabs[i].settings_page.get() {
                        self.history.set_title(&self.tabs[i].url, &title);
                        self.tabs[i].title = title;
                        self.sync_toolbar();
                    }
                }
            }
            UserEvent::PageLoad { tab, url, started } => {
                if let Some(i) = self.tab_index(tab) {
                    let t = &mut self.tabs[i];
                    if t.discarded {
                        return true;
                    }
                    // NavigateToString pages report about:blank / data: URLs.
                    let internal = url == "about:blank" || url.starts_with("data:");
                    if t.settings_page.get() && !internal {
                        t.settings_page.set(false);
                    }
                    if !t.settings_page.get() {
                        *t.page_url.borrow_mut() = url.clone();
                        if started {
                            self.history.visit(&url);
                        }
                        t.url = url;
                    }
                    t.loading = started;
                    self.sync_toolbar();
                }
                // Extensions install asynchronously; list them as pages finish loading.
                if !started {
                    self.refresh_extensions();
                    if self.tab_index(tab) == Some(self.active) {
                        self.measure_tone();
                    }
                }
            }
            UserEvent::OpenTab(url) => self.open_tab(&url),
            UserEvent::Shortcut(s) => match s {
                Shortcut::NewTab => return self.handle(UserEvent::Ipc(r#"{"cmd":"new_tab"}"#.into())),
                Shortcut::CloseTab => {
                    if self.popup.is_some() {
                        self.close_popup();
                    } else {
                        return self.close_tab(self.active);
                    }
                }
                Shortcut::FocusAddress => {
                    let _ = self.toolbar.focus();
                    let _ = self.toolbar.evaluate_script("hb.focusAddress()");
                }
                Shortcut::NextTab => self.switch_to((self.active + 1) % self.tabs.len()),
                Shortcut::PrevTab => self.switch_to((self.active + self.tabs.len() - 1) % self.tabs.len()),
                Shortcut::Reload => return self.handle(UserEvent::Ipc(r#"{"cmd":"reload"}"#.into())),
                Shortcut::Back => return self.handle(UserEvent::Ipc(r#"{"cmd":"back"}"#.into())),
                Shortcut::Forward => return self.handle(UserEvent::Ipc(r#"{"cmd":"forward"}"#.into())),
                Shortcut::SelectTab(i) => {
                    // Ctrl+9 = last tab, like other browsers.
                    let i = if i == 8 { self.tabs.len() - 1 } else { i };
                    self.switch_to(i);
                }
                Shortcut::Escape => self.close_popup(),
            },
            UserEvent::AdblockReady(engine) => {
                *self.engine.borrow_mut() = Some(*engine);
                self.sync_toolbar();
            }
            UserEvent::Blocked(_) => {
                self.blocked += 1;
                self.sync_toolbar();
            }
            UserEvent::Extensions(list) => {
                tracing::info!("extensions: {:?}", list.iter().map(|e| (&e.name, &e.id)).collect::<Vec<_>>());
                self.extensions = list;
                self.sync_toolbar();
                // Test hook: open the first extension's popup without a click.
                if std::env::var_os("CHICKENBROWSER_TEST_POPUP").is_some() && self.popup.is_none() {
                    if let Some(id) = self.extensions.first().map(|e| e.id.clone()) {
                        self.toggle_extension_popup(id);
                    }
                }
            }
            UserEvent::RefreshExtensions => {
                self.refresh_extensions();
            }
            UserEvent::PopupLostFocus => {
                if let Some(popup) = &self.popup {
                    // Ignore premature LostFocus during initial focus settlement
                    if popup.opened_at.elapsed() < Duration::from_millis(400) {
                        return true;
                    }
                }
                if let Some(popup) = self.popup.take() {
                    self.popup_closed = Some((popup.extension_id, Instant::now()));
                    self.sync_toolbar();
                }
            }
            UserEvent::CssReload(css) => {
                tracing::info!("applying hot-reloaded browser.css");
                let script = format!("window.hb && hb.setUserStyle({});", serde_json::to_string(&css).unwrap_or_default());
                let _ = self.toolbar.evaluate_script(&script);
                if let Some(f) = &self.flyout {
                    let _ = f.evaluate_script(&script);
                }
                let page_script = theme_apply_js(&css);
                if let Some(s) = &self.suggest {
                    let _ = s.evaluate_script(&page_script);
                }
                for tab in self.tabs.iter().filter(|t| t.settings_page.get()) {
                    let _ = tab.webview.evaluate_script(&page_script);
                }
                if let Some(popup) = self.popup.as_ref().filter(|p| p.extension_id == "menu") {
                    let _ = popup.webview.evaluate_script(&page_script);
                }
                let (r, g, b, a) = chrome_bg(&css);
                let _ = self.toolbar.set_background_color((r, g, b, a));
                self.user_css = css;
            }
            UserEvent::ThemeConfigChanged => {
                if let Ok((_, cfg)) = chickenwm_core::config::load_or_init_config() {
                    if cfg.theme != self.system_theme {
                        self.system_theme = cfg.theme;
                        self.apply_profile();
                        self.refresh_chrome_theme();
                        self.push_settings_state();
                    }
                }
            }
            UserEvent::Favicon(id, uri) => {
                if let Some(i) = self.tab_index(id) {
                    self.tabs[i].favicon = uri;
                    self.sync_toolbar();
                }
            }
            UserEvent::PageTone(id, dark) => {
                if let Some(i) = self.tab_index(id) {
                    self.tabs[i].tone = Some(dark);
                    if i == self.active {
                        self.refresh_chrome_theme();
                    }
                }
            }
            UserEvent::AmbientFallback(id) => {
                if self.tab_index(id) == Some(self.active) {
                    self.measure_ambient_css();
                }
            }
            UserEvent::AmbientTick => {
                if !self.window.is_minimized() {
                    self.measure_ambient();
                }
            }
            UserEvent::PageAmbient(id, colors) => {
                if let Some(i) = self.tab_index(id) {
                    // Ignore sampling noise so a steady page doesn't keep re-rendering the chrome.
                    let close = |a: &[u8; 3], b: &[u8; 3]| {
                        a.iter().zip(b).map(|(x, y)| (*x as i32 - *y as i32).abs()).sum::<i32>() < 14
                    };
                    let same = colors.len() == self.tabs[i].ambient.len()
                        && colors.iter().zip(&self.tabs[i].ambient).all(|(a, b)| close(a, b));
                    if !same {
                        self.tabs[i].ambient = colors;
                        if i == self.active {
                            self.sync_toolbar();
                        }
                    }
                }
            }
            UserEvent::MemoryTick => {
                // Pages can switch their own theme; keep "match website colors" in step.
                self.measure_tone();
                self.history.save_if_dirty();
                self.save_session();
                self.update_ram();
                self.memory_tick();
                if self.tabs.iter().any(|t| t.settings_page.get()) {
                    self.push_settings_state();
                }
            }
        }
        true
    }

    fn apply_memory_settings(&mut self, memory: MemorySettings) {
        let low = memory.low_memory_background;
        self.settings.memory = memory;
        self.save_settings();
        for (i, tab) in self.tabs.iter().enumerate() {
            if i != self.active {
                let level = if low { MemoryUsageLevel::Low } else { MemoryUsageLevel::Normal };
                let _ = tab.webview.set_memory_usage_level(level);
            }
        }
        self.push_settings_state();
    }
}

unsafe extern "system" fn enum_window_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let mut title = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut title) };
    if len > 0 {
        let text = String::from_utf16_lossy(&title[..len as usize]);
        if text.contains("chickenbrowser") {
            let out = unsafe { &mut *(lparam.0 as *mut Option<HWND>) };
            *out = Some(hwnd);
            return BOOL(0);
        }
    }
    BOOL(1)
}

fn focus_existing_instance() -> bool {
    unsafe {
        let mut found_hwnd: Option<HWND> = None;
        let _ = EnumWindows(
            Some(enum_window_callback),
            LPARAM(&mut found_hwnd as *mut _ as isize),
        );
        if let Some(hwnd) = found_hwnd {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
            return true;
        }
    }
    false
}

fn main() -> anyhow::Result<()> {
    chickenwm_core::init_tracing("chickenbrowser");

    // Single-instance guard: if another chickenbrowser is already running, focus it and exit cleanly.
    // This prevents WebView2 from crashing with 0x800700AA ("The requested resource is in use").
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Set by "Restart browser": wait for the old instance to exit instead of focusing it.
    let restarting = args.first().is_some_and(|a| a == "--wait-for-exit");
    if restarting {
        args.remove(0);
    }
    // Developer option: CHICKENBROWSER_PROFILE=<folder> runs a separate instance with its own
    // profile (and its own single-instance lock) next to your everyday browser.
    let isolated_root = std::env::var_os("CHICKENBROWSER_PROFILE").map(PathBuf::from);
    let mutex_name = windows::core::HSTRING::from(match &isolated_root {
        Some(dir) => format!(
            "Local\\chickenbrowser-{}",
            dir.to_string_lossy().bytes().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64))
        ),
        None => "Local\\chickenbrowser-single-instance".to_string(),
    });
    let _instance_mutex = unsafe {
        let mutex = CreateMutexW(None, true, &mutex_name)?;
        if GetLastError() == ERROR_ALREADY_EXISTS && restarting {
            // WAIT_ABANDONED (old process exited holding it) also hands us ownership.
            let _ = WaitForSingleObject(mutex, 15_000);
        } else if GetLastError() == ERROR_ALREADY_EXISTS {
            tracing::info!("another chickenbrowser instance is already running; focusing existing window");
            let _ = focus_existing_instance();
            return Ok(());
        }
        mutex
    };

    let root = isolated_root.unwrap_or_else(|| config_dir().join("chickenbrowser"));
    let css_path = config_dir().join("browser.css");
    if !css_path.exists() {
        let _ = std::fs::write(&css_path, include_str!("browser.css"));
        // Start from the system theme rather than the stock palette.
        if let Ok((_, cfg)) = chickenwm_core::config::load_or_init_config() {
            let palette = cfg.theme.palette(cfg.theme.mode);
            let _ = chickenwm_core::theme::apply_palette_to_file(&palette, &css_path);
        }
    }
    let paths = Paths {
        root: root.clone(),
        profile: root.join("profile"),
        extensions: root.join("extensions"),
        adblock: root.join("adblock"),
        css: css_path,
    };
    let user_css = std::fs::read_to_string(&paths.css).unwrap_or_default();
    let history_path = paths.root.join("history.json");
    let session_path = paths.root.join("session.json");
    std::fs::create_dir_all(&paths.extensions)?;
    // Before WebView2 installs them, so Proton Pass gets its store id (see `ensure_store_keys`).
    extensions::ensure_store_keys(&paths.extensions);
    let mut browser_settings = settings::load(&root);
    // One-time move of the old browser-only light/dark choice into the master theme provider.
    if let Some(mode) = chickenwm_core::theme::AppMode::parse(&browser_settings.appearance.theme)
        .filter(|m| *m != chickenwm_core::theme::AppMode::System)
    {
        if chickenwm_core::theme::set_app_mode("browser", mode).is_ok() {
            browser_settings.appearance.theme = "system".into();
            settings::save(&root, &browser_settings);
        }
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Hot-reload ~/.config/chickenwm/browser.css whenever it is saved
    let proxy_css = proxy.clone();
    let _css_watcher = chickenwm_core::css::spawn_config_watcher(move |ev| {
        match ev {
            chickenwm_core::css::ConfigWatchEvent::CssChanged { file_name, path } => {
                if file_name.eq_ignore_ascii_case("browser.css") {
                    if let Ok(css) = std::fs::read_to_string(&path) {
                        let _ = proxy_css.send_event(UserEvent::CssReload(css));
                    }
                }
            }
            chickenwm_core::css::ConfigWatchEvent::HyprConfChanged(_) => {
                let _ = proxy_css.send_event(UserEvent::ThemeConfigChanged);
            }
        }
    });
    let window = WindowBuilder::new()
        .with_title("chickenbrowser")
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 820.0))
        // No system titlebar: the toolbar is the titlebar (drag, double-click zoom, close).
        // The undecorated shadow keeps the drop shadow and the invisible resize frame on the
        // left/right/bottom edges; the top edge is handled by the toolbar.
        .with_decorations(false)
        .with_undecorated_shadow(true)
        .build(&event_loop)?;

    // Pages, popups and (by default) the toolbar share one profile and so one WebView2 browser
    // process tree. A separate toolbar profile costs a whole second browser + GPU process.
    let mut web_context = WebContext::new(Some(paths.profile.clone()));
    let mut toolbar_context = (!browser_settings.memory.share_toolbar_process)
        .then(|| WebContext::new(Some(root.join("ui"))));
    let toolbar_proxy = proxy.clone();
    let scale = window.scale_factor();
    let width = window.inner_size().width as f64 / scale;
    let toolbar_builder = match toolbar_context.as_mut() {
        Some(ctx) => WebViewBuilder::new_with_web_context(ctx),
        None => profile_builder(&mut web_context, &browser_settings.memory),
    };
    let toolbar = match toolbar_builder
        .with_html(include_str!("toolbar.html"))
        .with_background_color(chrome_bg(&user_css))
        .with_bounds(Rect {
            position: LogicalPosition::new(0.0, 0.0).into(),
            size: LogicalSize::new(width, window.inner_size().height as f64 / scale).into(),
        })
        .with_ipc_handler(move |request| {
            let _ = toolbar_proxy.send_event(UserEvent::Ipc(request.body().clone()));
        })
        .build_as_child(&window)
    {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("failed to create toolbar webview: {e}");
            if focus_existing_instance() {
                return Ok(());
            }
            return Err(e.into());
        }
    };
    install_shortcuts(&toolbar.controller(), proxy.clone(), false);

    let mut browser = Browser {
        window,
        toolbar,
        tabs: Vec::new(),
        active: 0,
        next_id: 1,
        popup: None,
        popup_closed: None,
        web_context,
        _toolbar_context: toolbar_context,
        proxy: proxy.clone(),
        engine: Rc::new(RefCell::new(None)),
        blocked: 0,
        extensions: Vec::new(),
        extensions_loaded: false,
        paths,
        launch_memory: browser_settings.memory.clone(),
        allowlist: Rc::new(RefCell::new(browser_settings.privacy.adblock_allowlist.clone())),
        settings: browser_settings,
        user_css,
        system_theme: chickenwm_core::config::load_or_init_config().map(|(_, c)| c.theme).unwrap_or_default(),
        mode_css: String::new(),
        flyout: None,
        flyout_visible: false,
        history: history::History::load(history_path),
        suggest: None,
        suggest_visible: false,
        ram: None,
        session_path,
        session_saved: String::new(),
        ambient_busy: Rc::new(Cell::new(None)),
    };
    browser.build_suggest();
    browser.build_ram();
    let proxy_amb = proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(1500));
        if proxy_amb.send_event(UserEvent::AmbientTick).is_err() {
            break;
        }
    });
    adblock_filter::spawn_engine_loader(browser.paths.adblock.clone(), proxy.clone());
    if args.is_empty() {
        browser.restore_session();
        if browser.tabs.is_empty() {
            browser.open_tab(HOME);
        }
    }
    for arg in &args {
        let url = resolve_input(arg, &browser.settings.search_engine);
        browser.open_tab(&url);
    }

    let proxy_mem = proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        if proxy_mem.send_event(UserEvent::MemoryTick).is_err() {
            break;
        }
    });

    // Periodically probe extensions shortly after startup to ensure they appear promptly
    let proxy_ext = proxy.clone();
    std::thread::spawn(move || {
        for delay in [200, 600, 1500] {
            std::thread::sleep(Duration::from_millis(delay));
            let _ = proxy_ext.send_event(UserEvent::RefreshExtensions);
        }
    });

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                browser.save_session();
                browser.history.save_if_dirty();
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent { event: WindowEvent::Resized(_), .. } => browser.layout(),
            Event::UserEvent(event) => {
                if !browser.handle(event) {
                    // The close button / closing the last tab: tabs are still intact here
                    // (the last-tab case leaves the list empty, which save_session ignores).
                    browser.save_session();
                    browser.history.save_if_dirty();
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{resolve_input, SEARCH};

    #[test]
    fn address_bar_input_resolution() {
        assert_eq!(resolve_input("proton.me", SEARCH), "https://proton.me");
        assert_eq!(resolve_input("https://a.b/c", SEARCH), "https://a.b/c");
        assert_eq!(resolve_input("localhost:3000", SEARCH), "https://localhost:3000");
        assert_eq!(resolve_input("rust borrow checker", SEARCH), "https://duckduckgo.com/?q=rust+borrow+checker");
        assert_eq!(resolve_input("c++", SEARCH), "https://duckduckgo.com/?q=c%2B%2B");
        assert_eq!(resolve_input("", SEARCH), "https://duckduckgo.com/");
        assert_eq!(resolve_input("about:settings", SEARCH), "about:settings");
        assert_eq!(resolve_input("chrome://settings", SEARCH), "about:settings");
    }
}
