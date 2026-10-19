//! Hen Browser: WebView2 browser with a HTML/CSS toolbar, tabs, adblock and unpacked extensions.
//!
//! Layout: one tao Window holds a child WebView for the toolbar (address bar + tab strip)
//! and one child WebView per tab (only the active tab is visible). Extension popups open in a
//! floating child WebView anchored to the top-right of the content area.
//!
//! All WebViews share one `WebContext` pointing at `~/.config/chickenwm/henbrowser/profile`, so cookies,
//! storage and installed extensions persist and `chrome.tabs` / content scripts see the tabs.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adblock_filter;
mod bookmarks;
mod chrome_ipc;
mod downloads;
mod ext_install;
mod extensions;
mod history;
mod internal;
mod settings;
mod tabs;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use tao::platform::windows::WindowBuilderExtWindows;
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Window, WindowBuilder};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    AcceleratorKeyPressedEventHandler, CapturePreviewCompletedHandler, FaviconChangedEventHandler,
};
use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM};
use windows::Win32::System::Com::{IStream, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET};
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::Win32::UI::WindowsAndMessaging::*;
use wry::WebViewBuilderExtWindows;
use wry::WebViewExtWindows;
use wry::{Rect, WebContext, WebView, WebViewBuilder};

use chrome_ipc::Popup;
use downloads::shell_execute;
use extensions::ExtensionInfo;
use internal::html_escape;
use settings::{BrowserSettings, MemorySettings};
use tabs::Tab;

pub(crate) const TOOLBAR_BOOKMARK_H: f64 = 28.0; // optional bookmarks bar
pub(crate) const SHELF_H: f64 = 46.0; // download bar along the bottom
pub(crate) const SIDEBAR_MIN_W: f64 = 180.0;
pub(crate) const SIDEBAR_MAX_W: f64 = 520.0;
pub(crate) const SIDEBAR_COLLAPSED_W: f64 = 56.0;
pub(crate) const SIDEBAR_STRIP_W: f64 = 16.0;
pub(crate) const POPUP_W: f64 = 380.0;
pub(crate) const POPUP_H: f64 = 560.0;
pub(crate) const SUGGEST_ROW_H: f64 = 34.0;
pub(crate) const SUGGEST_PAD: f64 = 10.0;
pub(crate) const SUGGEST_MAX: usize = 8;
// wry maps a custom protocol `hen` on Windows to `http://hen.<page>/`; the address bar shows `hen:<page>`.
pub(crate) const START_URL: &str = "http://hen.start/";
/// The start page in a private tab: no most-visited sites or history on it.
pub(crate) const PRIVATE_START_URL: &str = "http://hen.start/?private";
/// The gradient border around the whole window (drawn by the toolbar view, behind everything).
pub(crate) const WIN_BORDER: f64 = 3.0;
pub(crate) const HOME: &str = START_URL;
pub(crate) const SEARCH: &str = "https://duckduckgo.com/?q=";

#[derive(Debug, Clone)]
pub(crate) enum BookmarkAction {
    ReadLater { url: String, title: String },
    Remove { url: String },
    SetFolder { url: String, folder: String },
    ImportHtml { path: String },
}

#[derive(Debug, Clone)]
pub(crate) enum SiteAction {
    Add(String),
    Reset(String),
    ResetAll,
    ToggleDark(String),
    ToggleSleep(String),
    ToggleAdblock(String),
    SetZoom { site: String, zoom: f64 },
}

pub(crate) enum UserEvent {
    Ipc(String),
    Title(u32, String),
    PageLoad { tab: u32, url: String, started: bool },
    OpenTab(String),
    Shortcut(Shortcut),
    AdblockReady(Box<adblock::Engine>),
    Blocked,
    Extensions(Vec<ExtensionInfo>),
    RefreshExtensions,
    /// An extension package was unpacked into the extensions folder (or failed to).
    ExtensionUnpacked(Result<PathBuf, String>),
    /// A runtime extension install finished: its name, or the error.
    ExtensionInstalled(Result<String, String>),
    /// Back/Forward availability may have changed.
    NavState,
    /// Re-read live downloads (a backstop for missed WebView2 download events).
    DownloadPoll,
    PopupLostFocus,
    CssReload(String),
    ThemeConfigChanged,
    MemoryTick,
    ForgetSite(String),
    BookmarkAction(BookmarkAction),
    SiteAction(SiteAction),
    Download(internal::Download),
    Favicon(u32, String),
    PageTone(u32, bool),
    PageAmbient(u32, Vec<[u8; 3]>),
    AmbientFallback(u32),
    AmbientTick,
}

#[derive(Clone, Copy)]
pub(crate) enum Shortcut {
    NewTab,
    NewPrivateTab,
    CloseTab,
    ReopenClosedTab,
    FocusAddress,
    Reload,
    Back,
    Forward,
    NextTab,
    PrevTab,
    SelectTab(usize),
    Escape,
    Bookmark,
    History,
    Downloads,
    TabSearch,
    Reader,
    Pip,
}

pub(crate) struct Paths {
    pub(crate) root: PathBuf,
    pub(crate) profile: PathBuf,
    pub(crate) extensions: PathBuf,
    pub(crate) adblock: PathBuf,
    pub(crate) css: PathBuf,
    pub(crate) downloads_history: PathBuf,
}

pub(crate) struct Browser {
    pub(crate) window: Window,
    pub(crate) toolbar: WebView,
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: usize,
    pub(crate) next_id: u32,
    pub(crate) closed_tabs: Vec<String>,
    pub(crate) popup: Option<Popup>,
    pub(crate) popup_closed: Option<(String, Instant)>,
    pub(crate) web_context: WebContext,
    pub(crate) _toolbar_context: Option<WebContext>,
    pub(crate) proxy: EventLoopProxy<UserEvent>,
    pub(crate) engine: Rc<RefCell<Option<adblock::Engine>>>,
    pub(crate) blocked: u64,
    pub(crate) extensions: Vec<ExtensionInfo>,
    pub(crate) extensions_loaded: bool,
    pub(crate) paths: Paths,
    pub(crate) settings: BrowserSettings,
    pub(crate) launch_memory: MemorySettings,
    pub(crate) allowlist: Rc<RefCell<Vec<String>>>,
    pub(crate) user_css: String,
    pub(crate) system_theme: chickenwm_core::theme::ThemeConfig,
    pub(crate) mode_css: String,
    pub(crate) flyout: Option<WebView>,
    pub(crate) flyout_visible: bool,
    pub(crate) history: history::History,
    pub(crate) bookmarks: bookmarks::Bookmarks,
    pub(crate) start: std::sync::Arc<std::sync::Mutex<internal::InternalData>>,
    pub(crate) suggest: Option<WebView>,
    pub(crate) suggest_visible: bool,
    pub(crate) session_path: PathBuf,
    pub(crate) session_saved: String,
    pub(crate) ambient_busy: Rc<Cell<Option<Instant>>>,
    /// The download bar along the bottom, built on the first download.
    pub(crate) shelf: Option<WebView>,
    pub(crate) shelf_visible: bool,
    /// Downloads to open as soon as they finish.
    pub(crate) open_when_done: std::collections::HashSet<u64>,
    /// A short status line on the download bar (extension installs), with when it was set.
    pub(crate) notice: Option<(String, Instant)>,
}

fn config_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("chickenwm")
}

pub(crate) fn profile_builder<'a>(
    web_context: &'a mut WebContext,
    memory: &MemorySettings,
) -> WebViewBuilder<'a> {
    WebViewBuilder::new_with_web_context(web_context)
        .with_browser_extensions_enabled(true)
        .with_additional_browser_args(&memory.browser_args())
        .with_user_agent(user_agent())
}

/// What websites see: WebView2's Chromium user agent with Edge's `Edg/` token replaced by
/// `HenBrowser/<version>`. The `Chrome/` token stays so sites treat it as Chromium.
pub(crate) fn user_agent() -> &'static str {
    static UA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    UA.get_or_init(|| hen_user_agent(&wry::webview_version().unwrap_or_default()))
}

fn hen_user_agent(runtime_version: &str) -> String {
    let major = runtime_version.split('.').next().filter(|m| !m.is_empty()).unwrap_or("131");
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/{major}.0.0.0 Safari/537.36 HenBrowser/{}",
        env!("CARGO_PKG_VERSION")
    )
}

pub(crate) fn theme_apply_js(css: &str) -> String {
    let encoded = serde_json::to_string(css).unwrap_or_default();
    format!(
        "(function(css){{let el=document.getElementById('user-theme');if(!el){{el=document.createElement('style');el.id='user-theme';(document.head||document.documentElement).appendChild(el);}}el.textContent=css;}})({encoded});"
    )
}

pub(crate) fn theme_init_script(css: &str) -> String {
    let apply = theme_apply_js(css);
    format!(
        "if(location.hostname===''||location.protocol==='about:'||location.protocol==='data:'){{if(document.head||document.documentElement){{{apply}}}else{{document.addEventListener('DOMContentLoaded',function(){{{apply}}});}}}}"
    )
}

pub(crate) fn internal_mode_init(css: &str) -> String {
    if css.is_empty() {
        return String::new();
    }
    let apply = mode_apply_js(css);
    format!(
        "if(location.hostname===''||location.protocol==='about:'||location.protocol==='data:'){{if(document.head||document.documentElement){{{apply}}}else{{document.addEventListener('DOMContentLoaded',function(){{{apply}}});}}}}"
    )
}

pub(crate) fn chrome_bg(css: &str) -> (u8, u8, u8, u8) {
    let hex = css
        .lines()
        .find_map(|l| l.split_once("--bg-color:"))
        .map(|(_, v)| v.trim().trim_end_matches(';').trim().trim_start_matches('#'));
    if let Some(h) = hex.filter(|h| h.len() == 6) {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        ) {
            return (r, g, b, 255);
        }
    }
    (242, 241, 240, 255)
}

pub(crate) fn install_favicon(
    controller: &ICoreWebView2Controller,
    tab: u32,
    proxy: EventLoopProxy<UserEvent>,
) {
    let Ok(core15) =
        (unsafe { controller.CoreWebView2() }).and_then(|c| c.cast::<ICoreWebView2_15>())
    else {
        return;
    };
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

fn read_stream(stream: &IStream) -> Option<Vec<u8>> {
    unsafe {
        let mut stat = STATSTG::default();
        stream.Stat(&mut stat, STATFLAG_NONAME).ok()?;
        let len = stat.cbSize as usize;
        stream.Seek(0, STREAM_SEEK_SET, None).ok()?;
        let mut buf = vec![0u8; len];
        let mut read = 0u32;
        if stream
            .Read(buf.as_mut_ptr().cast(), len as u32, Some(&mut read))
            .is_err()
        {
            return None;
        }
        buf.truncate(read as usize);
        Some(buf)
    }
}

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
    let small = img
        .resize_exact(W, H, image::imageops::FilterType::Triangle)
        .to_rgb8();
    let mut out: Vec<[u8; 3]> = Vec::new();
    for (x0, y0, x1, y1) in REGIONS {
        let mut buckets = vec![(0u32, 0u32, 0u32, 0u32); 512];
        let (px0, px1) = ((x0 * W as f32) as u32, ((x1 * W as f32) as u32).max(1).min(W));
        let (py0, py1) = ((y0 * H as f32) as u32, ((y1 * H as f32) as u32).max(1).min(H));
        for y in py0..py1 {
            for x in px0..px1 {
                let p = small.get_pixel(x, y).0;
                let i = ((p[0] >> 5) as usize) << 6
                    | ((p[1] >> 5) as usize) << 3
                    | (p[2] >> 5) as usize;
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
        let c = [
            (best.1 / best.0) as u8,
            (best.2 / best.0) as u8,
            (best.3 / best.0) as u8,
        ];
        let distinct = out.iter().all(|o| {
            (o[0] as i32 - c[0] as i32).abs()
                + (o[1] as i32 - c[1] as i32).abs()
                + (o[2] as i32 - c[2] as i32).abs()
                >= 36
        });
        if distinct {
            out.push(c);
        }
    }
    out.truncate(5);
    (!out.is_empty()).then_some(out)
}

fn apply_profile_prefs(controller: &ICoreWebView2Controller, dark: Option<bool>, tracking: &str) {
    unsafe {
        let Ok(profile) = controller
            .CoreWebView2()
            .and_then(|c| c.cast::<ICoreWebView2_13>())
            .and_then(|c| c.Profile())
        else {
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

fn mode_css(
    theme: &chickenwm_core::theme::ThemeConfig,
    mode: chickenwm_core::theme::ThemeMode,
) -> String {
    let palette = theme.palette(mode);
    let mut css = String::from(":root {");
    for (k, v) in palette.css_vars("browser.css") {
        css.push_str(&format!(" {k}: {v};"));
    }
    css.push_str(&format!(" color-scheme: {}; }}", mode.as_str()));
    css
}

pub(crate) fn mode_apply_js(css: &str) -> String {
    format!(
        "(function(css){{let el=document.getElementById('mode-theme');if(!el){{el=document.createElement('style');el.id='mode-theme';(document.head||document.documentElement).appendChild(el);}}el.textContent=css;}})({});",
        serde_json::to_string(css).unwrap_or_default()
    )
}

pub(crate) fn apply_network_conditions(core: &ICoreWebView2, speed_limit_kbps: u32) {
    use webview2_com::CallDevToolsProtocolMethodCompletedHandler;
    let throughput = if speed_limit_kbps > 0 {
        (speed_limit_kbps as f64) * 1024.0
    } else {
        -1.0
    };
    let params = serde_json::json!({
        "offline": false,
        "latency": 0,
        "downloadThroughput": throughput,
        "uploadThroughput": -1
    }).to_string();
    unsafe {
        let _ = core.CallDevToolsProtocolMethod(
            &windows::core::HSTRING::from("Network.emulateNetworkConditions"),
            &windows::core::HSTRING::from(params),
            &CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(()))),
        );
    }
}

pub(crate) fn install_diagnostics(controller: &ICoreWebView2Controller, tab: u32) {
    use webview2_com::{
        CallDevToolsProtocolMethodCompletedHandler, DevToolsProtocolEventReceivedEventHandler,
        NavigationCompletedEventHandler,
    };
    let Ok(core) = (unsafe { controller.CoreWebView2() }) else {
        return;
    };
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
        let _ = core.add_NavigationStarting(
            &webview2_com::NavigationStartingEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    let mut uri = windows::core::PWSTR::null();
                    args.Uri(&mut uri)?;
                    let mut id = 0u64;
                    let _ = args.NavigationId(&mut id);
                    tracing::debug!(
                        "tab {tab}: navigation {id} starting {}",
                        webview2_com::take_pwstr(uri)
                    );
                }
                Ok(())
            })),
            &mut token,
        );
    }
    if std::env::var_os("HENBROWSER_DEBUG").is_none() {
        return;
    }
    for (event, enable) in [
        ("Runtime.exceptionThrown", "Runtime.enable"),
        ("Log.entryAdded", "Log.enable"),
    ] {
        unsafe {
            if let Ok(receiver) =
                core.GetDevToolsProtocolEventReceiver(&windows::core::HSTRING::from(event))
            {
                let _ = receiver.add_DevToolsProtocolEventReceived(
                    &DevToolsProtocolEventReceivedEventHandler::create(Box::new(move |_, args| {
                        if let Some(args) = args {
                            let mut json = windows::core::PWSTR::null();
                            args.ParameterObjectAsJson(&mut json)?;
                            let text = webview2_com::take_pwstr(json);
                            tracing::warn!(
                                "tab {tab} {event}: {}",
                                text.chars().take(600).collect::<String>()
                            );
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

/// Serves `http://hen.<page>/`. Actions come back as links with a query (`?forget=site`,
/// `?open=id`, …): they're carried out, then the page reloads without the query.
pub(crate) fn serve_internal(
    data: &std::sync::Mutex<internal::InternalData>,
    proxy: &std::sync::Mutex<EventLoopProxy<UserEvent>>,
    host: &str,
    uri: &str,
) -> String {
    let back = |page: &str| {
        format!(r#"<!doctype html><meta http-equiv="refresh" content="0;url=http://{page}/">"#)
    };
    let Ok(mut d) = data.lock() else {
        return String::new();
    };
    // wry hands the handler `hen://downloads/` for a page loaded as `http://hen.downloads/`.
    let host_owned = if host.starts_with("hen.") { host.to_owned() } else { format!("hen.{host}") };
    let host = host_owned.as_str();
    match host {
        "hen.history" => {
            if let Some(site) = internal::query_param(uri, "forget") {
                d.history.retain(|e| !settings::url_on_site(&e.url, &site));
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(UserEvent::ForgetSite(site));
                }
                return back(host);
            }
            internal::history_page(&d, &internal::query_param(uri, "q").unwrap_or_default())
        }
        "hen.downloads" => {
            let find = |key: &str| {
                internal::query_param(uri, key)
                    .and_then(|id| id.parse::<u64>().ok())
                    .and_then(|id| d.downloads.iter().find(|x| x.id == id).map(|x| x.path.clone()))
            };
            if let Some(path) = find("open") {
                shell_execute("open", &path, None);
                return back(host);
            }
            if let Some(path) = find("show") {
                shell_execute("open", "explorer.exe", Some(&format!("/select,\"{path}\"")));
                return back(host);
            }
            if internal::query_param(uri, "clear").is_some() {
                d.downloads.retain(|x| x.state == "in_progress" || x.state == "paused");
                return back(host);
            }
            internal::downloads_page(&d)
        }
        "hen.bookmarks" => {
            if let Some(url) = internal::query_param(uri, "read_later") {
                let title = internal::query_param(uri, "title").unwrap_or_default();
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(UserEvent::BookmarkAction(BookmarkAction::ReadLater {
                        url,
                        title,
                    }));
                }
                return back(host);
            }
            if let Some(url) = internal::query_param(uri, "remove") {
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(UserEvent::BookmarkAction(BookmarkAction::Remove { url }));
                }
                return back(host);
            }
            if let (Some(url), Some(folder)) = (
                internal::query_param(uri, "folder_url"),
                internal::query_param(uri, "set_folder"),
            ) {
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(UserEvent::BookmarkAction(BookmarkAction::SetFolder {
                        url,
                        folder,
                    }));
                }
                return back(host);
            }
            if let Some(path) = internal::query_param(uri, "import_path") {
                if !path.trim().is_empty() {
                    if let Ok(p) = proxy.lock() {
                        let _ = p.send_event(UserEvent::BookmarkAction(BookmarkAction::ImportHtml {
                            path,
                        }));
                    }
                }
                return back(host);
            }
            let q = internal::query_param(uri, "q").unwrap_or_default();
            let folder = internal::query_param(uri, "folder").unwrap_or_default();
            internal::bookmarks_page(&d, &q, &folder)
        }
        "hen.sites" => {
            if let Some(site) = internal::query_param(uri, "add_site") {
                let s = site.trim().trim_start_matches("www.").to_ascii_lowercase();
                if !s.is_empty() {
                    if let Ok(p) = proxy.lock() {
                        let _ = p.send_event(UserEvent::SiteAction(SiteAction::Add(s)));
                    }
                }
                return back(host);
            }
            if internal::query_param(uri, "reset_all").is_some() {
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(UserEvent::SiteAction(SiteAction::ResetAll));
                }
                return back(host);
            }
            if let Some(site) = internal::query_param(uri, "reset_site") {
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(UserEvent::SiteAction(SiteAction::Reset(site)));
                }
                return back(host);
            }
            if let Some(site) = internal::query_param(uri, "toggle_dark") {
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(UserEvent::SiteAction(SiteAction::ToggleDark(site)));
                }
                return back(host);
            }
            if let Some(site) = internal::query_param(uri, "toggle_sleep") {
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(UserEvent::SiteAction(SiteAction::ToggleSleep(site)));
                }
                return back(host);
            }
            if let Some(site) = internal::query_param(uri, "toggle_adblock") {
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(UserEvent::SiteAction(SiteAction::ToggleAdblock(site)));
                }
                return back(host);
            }
            if let (Some(site), Some(z)) = (
                internal::query_param(uri, "zoom_site"),
                internal::query_param(uri, "zoom").and_then(|v| v.parse::<f64>().ok()),
            ) {
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(UserEvent::SiteAction(SiteAction::SetZoom {
                        site,
                        zoom: (z / 100.0).clamp(0.25, 3.0),
                    }));
                }
                return back(host);
            }
            internal::sites_page(&d)
        }
        _ if uri.contains("private") => {
            // A private tab knows nothing about where you have been.
            let mut private = internal::InternalData { top: Vec::new(), history: Vec::new(), ..d.clone() };
            private.theme.push_str("<style>body{filter:saturate(0.6) brightness(0.92)}</style>");
            start_page_html(&private)
        }
        _ => start_page_html(&d),
    }
}

pub(crate) fn start_page_html(data: &internal::InternalData) -> String {
    let tiles = |items: &[(String, String)]| -> String {
        items
            .iter()
            .map(|(url, title)| {
                let host = url
                    .split_once("://")
                    .map_or(url.as_str(), |(_, r)| r)
                    .split('/')
                    .next()
                    .unwrap_or_default();
                let label = if title.trim().is_empty() {
                    host.trim_start_matches("www.")
                } else {
                    title.trim()
                };
                format!(
                    r#"<a class="tile" href="{u}" title="{u}"><img src="https://{h}/favicon.ico" alt="" onerror="this.style.visibility='hidden'"><span>{l}</span></a>"#,
                    u = html_escape(url),
                    h = html_escape(host),
                    l = html_escape(label),
                )
            })
            .collect()
    };
    let mut sections = String::new();
    sections.push_str(
        r#"<h2>Bookmarks <a href="http://hen.bookmarks/" style="float:right;text-transform:none;text-decoration:none;color:var(--subtext-color)">Manage &amp; Read Later →</a></h2>"#,
    );
    if data.bookmarks.is_empty() {
        sections.push_str(
            r#"<p class="empty">No bookmarks yet — press Ctrl+D or the star in the address bar.</p>"#,
        );
    } else {
        sections.push_str(&format!(r#"<div class="grid">{}</div>"#, tiles(&data.bookmarks)));
    }
    let read_later: Vec<(String, String)> = data
        .bookmark_items
        .iter()
        .filter(|b| b.read_later)
        .map(|b| (b.url.clone(), b.title.clone()))
        .collect();
    if !read_later.is_empty() {
        sections.push_str(&format!(
            r#"<h2>Read Later</h2><div class="grid">{}</div>"#,
            tiles(&read_later)
        ));
    }
    if !data.top.is_empty() {
        sections.push_str(&format!(
            r#"<h2>Most visited</h2><div class="grid">{}</div>"#,
            tiles(&data.top)
        ));
    }
    let search = data.search.split('?').next().unwrap_or(&data.search);
    let mut html = include_str!("start.html")
        .replace("<!--THEME-->", &data.theme)
        .replace("<!--SEARCH-->", &html_escape(search))
        .replace("<!--SECTIONS-->", &sections);
    if !data.show_search {
        html = html.replace(
            r#"id="search">"#,
            r#"id="search" style="display:none">"#,
        );
    }
    html
}

pub(crate) fn show_settings_page(webview: &WebView) {
    if let Ok(core) = unsafe { webview.controller().CoreWebView2() } {
        let html = windows::core::HSTRING::from(include_str!("settings.html"));
        let _ = unsafe { core.NavigateToString(&html) };
    }
}

pub(crate) fn process_memory(core: &ICoreWebView2) -> Option<(u64, u32)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
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
            let Ok(info) = infos.GetValueAtIndex(i) else {
                continue;
            };
            let mut pid = 0i32;
            if info.ProcessId(&mut pid).is_err() {
                continue;
            }
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32)
            else {
                continue;
            };
            let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
            let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
            if K32GetProcessMemoryInfo(
                handle,
                &mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
                size,
            )
            .as_bool()
            {
                total += counters.PrivateUsage as u64;
            }
            let _ = CloseHandle(handle);
        }
        Some((total, count))
    }
}

/// How a URL shows in the address bar: internal pages as `hen:<page>`.
pub(crate) fn display_url(url: &str) -> String {
    match url.strip_prefix("http://hen.") {
        Some(rest) => {
            let (page, tail) = rest.split_once('/').unwrap_or((rest, ""));
            format!("hen:{page}{tail}")
        }
        None => url.to_owned(),
    }
}

pub(crate) fn resolve_input(input: &str, search_base: &str) -> String {
    let input = input.trim();
    if input.is_empty() {
        return HOME.to_owned();
    }
    if [
        "about:start",
        "about:newtab",
        "about:home",
        "chrome://newtab",
        "edge://newtab",
    ]
    .iter()
    .any(|p| input.eq_ignore_ascii_case(p))
    {
        return START_URL.to_owned();
    }
    // `hen:downloads`, `hen://history`, ... -> the internal page.
    if let Some(page) = input.get(..4).filter(|p| p.eq_ignore_ascii_case("hen:")).map(|_| input[4..].trim_start_matches('/')) {
        let page = page.trim_end_matches('/').to_ascii_lowercase();
        if page.is_empty() || page == "start" || page == "newtab" {
            return START_URL.to_owned();
        }
        if page.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return format!("http://hen.{page}/");
        }
    }
    if input.eq_ignore_ascii_case("about:bookmarks")
        || input.eq_ignore_ascii_case("chrome://bookmarks")
    {
        return "http://hen.bookmarks/".to_owned();
    }
    if input.eq_ignore_ascii_case("about:sites") || input.eq_ignore_ascii_case("chrome://sites") {
        return "http://hen.sites/".to_owned();
    }
    if input.eq_ignore_ascii_case("about:settings")
        || input.eq_ignore_ascii_case("chrome://settings")
        || input.eq_ignore_ascii_case("edge://settings")
        || input.eq_ignore_ascii_case("hypr://settings")
        || input.eq_ignore_ascii_case("settings")
    {
        return "about:settings".to_owned();
    }
    let has_scheme = [
        "http://",
        "https://",
        "file://",
        "about:",
        "chrome-extension://",
        "edge://",
    ]
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
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
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
        (true, true, false) if vk == b'T' as u32 => Shortcut::ReopenClosedTab,
        (true, true, false) if vk == b'N' as u32 => Shortcut::NewPrivateTab,
        (true, false, false) if vk == b'W' as u32 => Shortcut::CloseTab,
        (true, false, false) if vk == b'L' as u32 => Shortcut::FocusAddress,
        (true, false, false) if vk == b'R' as u32 => Shortcut::Reload,
        (true, false, false) if vk == b'D' as u32 => Shortcut::Bookmark,
        (true, false, false) if vk == b'H' as u32 => Shortcut::History,
        (true, false, false) if vk == b'J' as u32 => Shortcut::Downloads,
        (true, true, false) if vk == b'A' as u32 => Shortcut::TabSearch,
        (true, true, false) if vk == b'P' as u32 => Shortcut::Pip,
        (false, false, false) if vk == 0x78 => Shortcut::Reader, // F9
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

pub(crate) fn install_shortcuts(
    controller: &ICoreWebView2Controller,
    proxy: EventLoopProxy<UserEvent>,
    page: bool,
) {
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
                let shortcut = shortcut_for(
                    vk,
                    key_down(VK_CONTROL),
                    key_down(VK_SHIFT),
                    key_down(VK_MENU),
                );
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
    /// Memory used by all of the browser's processes, for the menu's footer.
    pub(crate) fn ram_text(&self) -> String {
        let own = unsafe {
            use windows::Win32::System::ProcessStatus::{
                K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
            };
            use windows::Win32::System::Threading::GetCurrentProcess;
            let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
            let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
            if K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
                size,
            )
            .as_bool()
            {
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
        text
    }

    /// Refreshes the RAM line in the menu while it is open.
    pub(crate) fn update_ram(&self) {
        if let Some(menu) = self.popup.as_ref().filter(|p| p.extension_id == "menu") {
            let text = serde_json::to_string(&self.ram_text()).unwrap_or_default();
            let _ = menu.webview.evaluate_script(&format!("window.setRam && setRam({text})"));
        }
    }

    pub(crate) fn keep_toolbar_behind(&self) {
        let hwnd = self.toolbar.hwnd();
        unsafe {
            let _ = SetWindowPos(
                HWND(hwnd.0 as _),
                Some(HWND_BOTTOM),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    pub(crate) fn effective_mode(&self) -> Option<chickenwm_core::theme::ThemeMode> {
        use chickenwm_core::theme::ThemeMode;
        if self.settings.appearance.match_website {
            if let Some(dark) = self.tabs.get(self.active).and_then(|t| t.tone) {
                let page = if dark {
                    ThemeMode::Dark
                } else {
                    ThemeMode::Light
                };
                if page != self.system_theme.mode_for("browser") {
                    return Some(page);
                }
            }
        }
        None
    }

    pub(crate) fn set_browser_mode(&mut self, mode: chickenwm_core::theme::AppMode) {
        match chickenwm_core::theme::set_app_mode("browser", mode) {
            Ok(theme) => self.system_theme = theme,
            Err(e) => tracing::warn!("couldn't save the browser's theme mode: {e}"),
        }
        self.apply_profile();
        self.refresh_chrome_theme();
        self.push_settings_state();
    }

    pub(crate) fn refresh_chrome_theme(&mut self) {
        let css = self
            .effective_mode()
            .map(|m| mode_css(&self.system_theme, m))
            .unwrap_or_default();
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
        if let Some(s) = &self.shelf {
            let _ = s.evaluate_script(&script);
        }
        for tab in self.tabs.iter().filter(|t| t.settings_page.get()) {
            let _ = tab.webview.evaluate_script(&script);
        }
        if let Some(popup) = self.popup.as_ref().filter(|p| p.extension_id == "menu") {
            let _ = popup.webview.evaluate_script(&script);
        }
        let bg = if css.is_empty() {
            chrome_bg(&self.user_css)
        } else {
            chrome_bg(&css)
        };
        let _ = self.toolbar.set_background_color(bg);
        self.mode_css = css;
    }

    pub(crate) fn apply_profile(&self) {
        let dark =
            Some(self.system_theme.mode_for("browser") == chickenwm_core::theme::ThemeMode::Dark);
        if let Some(tab) = self.tabs.first() {
            apply_profile_prefs(
                &tab.webview.controller(),
                dark,
                &self.settings.privacy.tracking_prevention,
            );
        }
    }

    pub(crate) fn measure_tone(&self) {
        self.measure_ambient();
        if !self.settings.appearance.match_website {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.settings_page.get() || tab.discarded {
            return;
        }
        let (id, proxy) = (tab.id, self.proxy.clone());
        let _ = tab.webview.evaluate_script_with_callback(PAGE_TONE_JS, move |result| {
            match result.trim() {
                "true" => {
                    let _ = proxy.send_event(UserEvent::PageTone(id, true));
                }
                "false" => {
                    let _ = proxy.send_event(UserEvent::PageTone(id, false));
                }
                _ => {}
            }
        });
    }

    pub(crate) fn measure_ambient(&self) {
        if !self.settings.appearance.ambient {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.settings_page.get() || tab.discarded {
            return;
        }
        if self
            .ambient_busy
            .get()
            .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        let Some(stream) = (unsafe { SHCreateMemStream(None) }) else {
            self.measure_ambient_css();
            return;
        };
        self.ambient_busy.set(Some(Instant::now()));
        let (id, proxy, busy, read_from) = (
            tab.id,
            self.proxy.clone(),
            self.ambient_busy.clone(),
            stream.clone(),
        );
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |result| {
            busy.set(None);
            match result.ok().and_then(|_| read_stream(&read_from)) {
                Some(bytes) => {
                    std::thread::spawn(move || match ambient_from_jpeg(&bytes) {
                        Some(colors) => {
                            let _ = proxy.send_event(UserEvent::PageAmbient(id, colors));
                        }
                        None => {
                            let _ = proxy.send_event(UserEvent::AmbientFallback(id));
                        }
                    });
                }
                None => {
                    let _ = proxy.send_event(UserEvent::AmbientFallback(id));
                }
            }
            Ok(())
        }));
        let started = match unsafe { tab.webview.controller().CoreWebView2() } {
            Ok(core) => unsafe {
                core.CapturePreview(
                    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_JPEG,
                    &stream,
                    &handler,
                )
                .is_ok()
            },
            Err(_) => false,
        };
        if !started {
            self.ambient_busy.set(None);
            self.measure_ambient_css();
        }
    }

    pub(crate) fn measure_ambient_css(&self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.settings_page.get() || tab.discarded {
            return;
        }
        let (id, proxy) = (tab.id, self.proxy.clone());
        let _ = tab
            .webview
            .evaluate_script_with_callback(PAGE_AMBIENT_JS, move |result| {
                if let Ok(colors) = serde_json::from_str::<Vec<[u8; 3]>>(result.trim()) {
                    let _ = proxy.send_event(UserEvent::PageAmbient(id, colors));
                }
            });
    }

    pub(crate) fn eval_active(&self, js: &str) {
        if let Some(tab) = self.tabs.get(self.active).filter(|t| !t.settings_page.get()) {
            let _ = tab.webview.evaluate_script(js);
        }
    }

    pub(crate) fn open_internal(&mut self, url: &str) {
        self.refresh_start();
        if let Some(i) = self.tabs.iter().position(|t| t.url.starts_with(url)) {
            if let Some(tab) = self.tabs.get(i) {
                let _ = tab.webview.load_url(url);
            }
            self.switch_to(i);
        } else {
            self.open_tab(url);
        }
    }

    pub(crate) fn apply_site_settings(&self, index: usize) {
        let Some(tab) = self
            .tabs
            .get(index)
            .filter(|t| !t.settings_page.get() && !t.discarded)
        else {
            return;
        };
        let site = settings::site_of(&tab.url).unwrap_or_default();
        let s = self.settings.sites.get(&site).cloned().unwrap_or_default();
        let controller = tab.webview.controller();
        let mut current = 1.0;
        if unsafe { controller.ZoomFactor(&mut current) }.is_ok() {
            let want = s.zoom.unwrap_or(1.0);
            if (current - want).abs() > 0.001 {
                let _ = unsafe { controller.SetZoomFactor(want) };
            }
        }
        if s.force_dark {
            let _ = tab.webview.evaluate_script(internal::DARK_JS);
        }
    }

    pub(crate) fn remember_zoom(&mut self, zoom: f64) {
        let Some(site) = self.tabs.get(self.active).and_then(|t| settings::site_of(&t.url)) else {
            return;
        };
        let entry = self.settings.sites.entry(site.clone()).or_default();
        entry.zoom = ((zoom - 1.0).abs() > 0.001).then_some(zoom);
        if *entry == settings::SiteSettings::default() {
            self.settings.sites.remove(&site);
        }
        self.save_settings();
        self.refresh_start();
    }

    pub(crate) fn toggle_site_dark(&mut self) {
        let Some(site) = self.tabs.get(self.active).and_then(|t| settings::site_of(&t.url)) else {
            return;
        };
        let entry = self.settings.sites.entry(site.clone()).or_default();
        entry.force_dark = !entry.force_dark;
        let on = entry.force_dark;
        if *entry == settings::SiteSettings::default() {
            self.settings.sites.remove(&site);
        }
        self.save_settings();
        self.refresh_start();
        self.eval_active(if on {
            internal::DARK_JS
        } else {
            internal::UNDARK_JS
        });
    }

    pub(crate) fn toggle_site_sleep(&mut self) {
        let Some(site) = self.tabs.get(self.active).and_then(|t| settings::site_of(&t.url)) else {
            return;
        };
        let entry = self.settings.sites.entry(site.clone()).or_default();
        entry.never_sleep = !entry.never_sleep;
        self.settings.memory.never_sleep_sites.retain(|s| s != &site);
        if *entry == settings::SiteSettings::default() {
            self.settings.sites.remove(&site);
        }
        self.save_settings();
        self.refresh_start();
        self.sync_toolbar();
    }

    pub(crate) fn toggle_bookmark(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.settings_page.get() || tab.discarded {
            return;
        }
        let added = self.bookmarks.toggle(&tab.url, &tab.title);
        tracing::info!(
            "{} bookmark {}",
            if added { "added" } else { "removed" },
            tab.url
        );
        self.refresh_start();
        self.sync_toolbar();
    }

    pub(crate) fn toggle_site_adblock(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(site) = settings::site_of(&tab.url) else {
            return;
        };
        let list = &mut self.settings.privacy.adblock_allowlist;
        if let Some(i) = list.iter().position(|s| *s == site) {
            list.remove(i);
        } else {
            list.push(site);
        }
        *self.allowlist.borrow_mut() = list.clone();
        self.save_settings();
        self.refresh_start();
        let _ = self.tabs[self.active].webview.reload();
        self.sync_toolbar();
        self.push_settings_state();
    }

    pub(crate) fn save_settings(&self) {
        settings::save(&self.paths.root, &self.settings);
    }
}

unsafe extern "system" fn enum_window_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let mut title = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut title) };
    if len > 0 {
        let text = String::from_utf16_lossy(&title[..len as usize]);
        if text.contains("Hen Browser") {
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
    chickenwm_core::init_tracing("henbrowser");

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let restarting = args.first().is_some_and(|a| a == "--wait-for-exit");
    if restarting {
        args.remove(0);
    }
    let isolated_root = std::env::var_os("HENBROWSER_PROFILE").map(PathBuf::from);
    let mutex_name = windows::core::HSTRING::from(match &isolated_root {
        Some(dir) => format!(
            "Local\\henbrowser-{}",
            dir.to_string_lossy()
                .bytes()
                .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64))
        ),
        None => "Local\\henbrowser-single-instance".to_string(),
    });
    let _instance_mutex = unsafe {
        let mutex = CreateMutexW(None, true, &mutex_name)?;
        if GetLastError() == ERROR_ALREADY_EXISTS && restarting {
            let _ = WaitForSingleObject(mutex, 15_000);
        } else if GetLastError() == ERROR_ALREADY_EXISTS {
            tracing::info!("another henbrowser instance is already running; focusing existing window");
            let _ = focus_existing_instance();
            return Ok(());
        }
        mutex
    };

    // The data folder from before the browser was named Hen Browser moves over once.
    let legacy = config_dir().join(concat!("chicken", "browser"));
    let current = config_dir().join("henbrowser");
    if isolated_root.is_none() && legacy.is_dir() && !current.exists() {
        match std::fs::rename(&legacy, &current) {
            Ok(()) => tracing::info!("moved {} to {}", legacy.display(), current.display()),
            Err(e) => tracing::warn!("couldn't move {} to {}: {e}", legacy.display(), current.display()),
        }
    }
    let root = isolated_root.unwrap_or(current);
    let css_path = config_dir().join("browser.css");
    if !css_path.exists() {
        let _ = std::fs::write(&css_path, include_str!("browser.css"));
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
        downloads_history: root.join("downloads.json"),
    };
    let user_css = std::fs::read_to_string(&paths.css).unwrap_or_default();
    let history_path = paths.root.join("history.json");
    let bookmarks_path = paths.root.join("bookmarks.json");
    let session_path = paths.root.join("session.json");
    std::fs::create_dir_all(&paths.extensions)?;
    ext_install::normalize_root(&paths.extensions);
    let mut browser_settings = settings::load(&root);
    // There is no top bar any more: the tabs and every control live in the sidebar.
    browser_settings.appearance.vertical_tabs = true;
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
        .with_window_icon(tao::window::Icon::from_rgba(include_bytes!("../assets/icon-64.rgba").to_vec(), 64, 64).ok())
        .with_title("Hen Browser")
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 820.0))
        .with_decorations(false)
        .with_undecorated_shadow(false)
        .build(&event_loop)?;

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

    let initial_downloads: Vec<internal::Download> = std::fs::read_to_string(&paths.downloads_history)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut browser = Browser {
        window,
        toolbar,
        tabs: Vec::new(),
        active: 0,
        next_id: 1,
        closed_tabs: Vec::new(),
        popup: None,
        popup_closed: None,
        web_context,
        _toolbar_context: toolbar_context,
        proxy: proxy.clone(),
        engine: Rc::new(RefCell::new(None)),
        blocked: 0,
        extensions: Vec::new(),
        extensions_loaded: extensions::all_installed(&paths.extensions, &paths.profile),
        paths,
        launch_memory: browser_settings.memory.clone(),
        allowlist: Rc::new(RefCell::new(
            browser_settings.privacy.adblock_allowlist.clone(),
        )),
        settings: browser_settings,
        user_css,
        system_theme: chickenwm_core::config::load_or_init_config()
            .map(|(_, c)| c.theme)
            .unwrap_or_default(),
        mode_css: String::new(),
        flyout: None,
        flyout_visible: false,
        history: history::History::load(history_path),
        bookmarks: bookmarks::Bookmarks::load(bookmarks_path),
        start: std::sync::Arc::new(std::sync::Mutex::new(internal::InternalData {
            downloads: initial_downloads,
            ..Default::default()
        })),
        suggest: None,
        suggest_visible: false,
        session_path,
        session_saved: String::new(),
        ambient_busy: Rc::new(Cell::new(None)),
        shelf: None,
        shelf_visible: false,
        open_when_done: Default::default(),
        notice: None,
    };
    browser.build_suggest();
    let proxy_amb = proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(1500));
        if proxy_amb.send_event(UserEvent::AmbientTick).is_err() {
            break;
        }
    });
    adblock_filter::spawn_engine_loader(browser.paths.adblock.clone(), proxy.clone());
    browser.refresh_start();
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

    // Download state backstop: cheap when nothing is downloading.
    let proxy_dl = proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        if proxy_dl.send_event(UserEvent::DownloadPoll).is_err() {
            break;
        }
    });

    let proxy_mem = proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        if proxy_mem.send_event(UserEvent::MemoryTick).is_err() {
            break;
        }
    });

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
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                browser.save_session();
                browser.history.save_if_dirty();
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(_),
                ..
            } => browser.layout(),
            Event::UserEvent(event) => {
                if !browser.handle(event) {
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
        assert_eq!(
            resolve_input("rust borrow checker", SEARCH),
            "https://duckduckgo.com/?q=rust+borrow+checker"
        );
        assert_eq!(
            resolve_input("c++", SEARCH),
            "https://duckduckgo.com/?q=c%2B%2B"
        );
        assert_eq!(resolve_input("", SEARCH), super::START_URL);
        assert_eq!(resolve_input("about:newtab", SEARCH), super::START_URL);
        assert_eq!(resolve_input("about:settings", SEARCH), "about:settings");
        assert_eq!(resolve_input("chrome://settings", SEARCH), "about:settings");
        assert_eq!(
            resolve_input("about:bookmarks", SEARCH),
            "http://hen.bookmarks/"
        );
        assert_eq!(
            resolve_input("about:sites", SEARCH),
            "http://hen.sites/"
        );
    }

    #[test]
    fn start_page_lists_bookmarks_and_escapes() {
        let mut data = super::internal::InternalData {
            theme: "<style>:root{--fg-color:red}</style>".into(),
            search: "https://duckduckgo.com/?q=".into(),
            show_search: true,
            bookmarks: vec![("https://doc.rust-lang.org/std/".into(), "Rust <std>".into())],
            top: vec![("https://github.com/".into(), String::new())],
            ..Default::default()
        };
        let html = super::start_page_html(&data);
        assert!(html.contains(r#"action="https://duckduckgo.com/""#));
        assert!(html.contains("Rust &lt;std&gt;"));
        assert!(html.contains(">github.com<"));
        assert!(html.contains("--fg-color:red"));
        assert!(!html.contains("<!--SECTIONS-->"));

        data.show_search = false;
        let html_no_search = super::start_page_html(&data);
        assert!(html_no_search.contains(r#"id="search" style="display:none""#));
    }

    #[test]
    fn sites_and_bookmarks_internal_pages_render() {
        let mut data = super::internal::InternalData::default();
        data.sites.insert(
            "github.com".into(),
            super::settings::SiteSettings {
                zoom: Some(1.25),
                force_dark: true,
                never_sleep: true,
            },
        );
        let sites_html = super::internal::sites_page(&data);
        assert!(sites_html.contains("github.com"));
        assert!(sites_html.contains("Zoom: 125%"));
        assert!(sites_html.contains("Never sleep: Yes"));

        data.bookmark_items.push(super::bookmarks::Bookmark {
            url: "https://rust-lang.org/".into(),
            title: "Rust".into(),
            added: 1,
            folder: "Languages".into(),
            read_later: true,
        });
        let bm_html = super::internal::bookmarks_page(&data, "rust", "__read_later__");
        assert!(bm_html.contains("https://rust-lang.org/"));
    }

    #[test]
    fn user_agent_names_hen_browser() {
        let ua = super::hen_user_agent("131.0.2903.70");
        assert!(ua.contains("Chrome/131.0.0.0 ") && ua.contains(" HenBrowser/") && !ua.contains("Edg/"));
    }
}
