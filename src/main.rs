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
use webview2_com::{AcceleratorKeyPressedEventHandler, ClearBrowsingDataCompletedHandler, FocusChangedEventHandler, TrySuspendCompletedHandler};
use windows::core::{w, BOOL, Interface};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM};
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_F5, VK_LEFT, VK_MENU, VK_RIGHT, VK_SHIFT,
    VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, SetForegroundWindow, SetWindowPos, ShowWindow, HWND_TOP,
    SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_RESTORE,
};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{
    MemoryUsageLevel, NewWindowResponse, PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder,
    WebViewBuilderExtWindows, WebViewExtWindows,
};

/// Height of the toolbar + tab strip (logical px); must match `toolbar.html`.
const TOOLBAR_H: f64 = 82.0;
/// Height of the toolbar row alone; extension popovers open just below it, Safari-style.
const TOOLBAR_ROW_H: f64 = 48.0;
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
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum ToolbarCmd {
    Ready,
    Navigate { input: String },
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
}

/// Every webview on the shared profile must be created with identical environment options
/// (extensions flag, browser arguments), or WebView2 refuses to create it — so all of them are
/// built through here.
fn profile_builder<'a>(context: &'a mut WebContext, memory: &MemorySettings) -> WebViewBuilder<'a> {
    WebViewBuilder::new_with_web_context(context)
        .with_browser_extensions_enabled(true)
        .with_additional_browser_args(memory.browser_args())
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

    fn content_rect(&self) -> Rect {
        let (w, h) = self.logical_size();
        Rect {
            position: LogicalPosition::new(0.0, TOOLBAR_H).into(),
            size: LogicalSize::new(w, (h - TOOLBAR_H).max(1.0)).into(),
        }
    }

    fn toolbar_rect(&self) -> Rect {
        let (w, _) = self.logical_size();
        Rect {
            position: LogicalPosition::new(0.0, 0.0).into(),
            size: LogicalSize::new(w, TOOLBAR_H).into(),
        }
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
        let height = 370.0f64.min((h - TOOLBAR_ROW_H - 10.0).max(100.0));
        Rect {
            position: LogicalPosition::new((w - width - 8.0).max(0.0), TOOLBAR_ROW_H).into(),
            size: LogicalSize::new(width, height).into(),
        }
    }

    fn layout(&self) {
        let _ = self.toolbar.set_bounds(self.toolbar_rect());
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
            })).collect::<Vec<_>>(),
            "url": active.map(|t| t.url.as_str()).unwrap_or(""),
            "blocked": self.blocked,
            "adblock": self.engine.borrow().is_some(),
            "extensions": self.extensions,
            "popup": self.popup.as_ref().map(|p| p.extension_id.as_str()),
        });
        let _ = self.toolbar.evaluate_script(&format!("window.hb && hb.render({state})"));
        let title = active
            .map(|t| if t.title.is_empty() { t.url.clone() } else { t.title.clone() })
            .unwrap_or_default();
        self.window.set_title(&format!("{title} — chickenbrowser"));
    }

    fn open_tab(&mut self, url: &str) {
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
            .with_new_window_req_handler(move |url, _features| {
                // target=_blank, window.open, extension-opened pages: open as a tab.
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
        if let Err(e) = adblock_filter::install(&controller, self.engine.clone(), page_url.clone(), self.proxy.clone(), id) {
            tracing::warn!("adblock hook failed for tab {id}: {e}");
        }
        install_shortcuts(&controller, self.proxy.clone(), true);
        if !self.extensions_loaded {
            self.extensions_loaded = true;
        }

        self.send_to_background(self.active);
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
        });
        self.active = self.tabs.len() - 1;
        self.sync_toolbar();
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
                        if let Ok(css) = std::fs::read_to_string(&self.paths.css) {
                            let script = format!("window.hb && hb.setUserStyle({});", serde_json::to_string(&css).unwrap_or_default());
                            let _ = self.toolbar.evaluate_script(&script);
                        }
                        self.refresh_extensions();
                    }
                    ToolbarCmd::Navigate { input } => {
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
                        t.url = url;
                    }
                    t.loading = started;
                    self.sync_toolbar();
                }
                // Extensions install asynchronously; list them as pages finish loading.
                if !started {
                    self.refresh_extensions();
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
                let page_script = theme_apply_js(&css);
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
            UserEvent::MemoryTick => {
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
    let _instance_mutex = unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\chickenbrowser-single-instance"))?;
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

    let root = config_dir().join("chickenbrowser");
    let css_path = config_dir().join("browser.css");
    if !css_path.exists() {
        let _ = std::fs::write(&css_path, include_str!("browser.css"));
        // Start from the system theme rather than the stock palette.
        if let Ok((_, cfg)) = chickenwm_core::config::load_or_init_config() {
            let palette = chickenwm_core::theme::Palette::new(cfg.theme.accent, cfg.theme.mode);
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
    std::fs::create_dir_all(&paths.extensions)?;
    // Before WebView2 installs them, so Proton Pass gets its store id (see `ensure_store_keys`).
    extensions::ensure_store_keys(&paths.extensions);
    let browser_settings = settings::load(&root);

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Hot-reload ~/.config/chickenwm/browser.css whenever it is saved
    let proxy_css = proxy.clone();
    let _css_watcher = chickenwm_core::css::spawn_config_watcher(move |ev| {
        if let chickenwm_core::css::ConfigWatchEvent::CssChanged { file_name, path } = ev {
            if file_name.eq_ignore_ascii_case("browser.css") {
                if let Ok(css) = std::fs::read_to_string(&path) {
                    let _ = proxy_css.send_event(UserEvent::CssReload(css));
                }
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
            size: LogicalSize::new(width, TOOLBAR_H).into(),
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
        settings: browser_settings,
        user_css,
    };
    adblock_filter::spawn_engine_loader(browser.paths.adblock.clone(), proxy.clone());
    if args.is_empty() {
        browser.open_tab(HOME);
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
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => *control_flow = ControlFlow::Exit,
            Event::WindowEvent { event: WindowEvent::Resized(_), .. } => browser.layout(),
            Event::UserEvent(event) => {
                if !browser.handle(event) {
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
