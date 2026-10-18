use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::window::ResizeDirection;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{ClearBrowsingDataCompletedHandler, FocusChangedEventHandler};
use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::*;
use wry::MemoryUsageLevel;
use wry::WebViewExtWindows;
use wry::{NewWindowResponse, Rect, WebView};

use crate::settings::MemorySettings;
use crate::{
    display_url, downloads, ext_install, extensions, history, install_shortcuts, internal, internal_mode_init,
    mode_apply_js, process_memory, profile_builder, resolve_input, settings, show_settings_page,
    theme_apply_js, theme_init_script, Browser, Shortcut, UserEvent, HOME, POPUP_H, POPUP_W,
    SHELF_H, SIDEBAR_COLLAPSED_W, SIDEBAR_STRIP_W, SIDEBAR_W, START_URL, SUGGEST_MAX, SUGGEST_PAD,
    SUGGEST_ROW_H, TOOLBAR_BOOKMARK_H, WIN_BORDER,
};

pub(crate) struct Popup {
    pub(crate) extension_id: String,
    pub(crate) webview: WebView,
    pub(crate) opened_at: Instant,
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub(crate) enum ToolbarCmd {
    Ready,
    Navigate { input: String },
    NewTab,
    NewWindow,
    CloseTab { id: u32 },
    SwitchTab { id: u32 },
    PinTab { id: u32 },
    ReorderTab { from_id: u32, to_id: u32 },
    Back,
    Forward,
    Reload,
    Home,
    ExtPopup { id: String },
    FocusPage,
    ToggleMenu,
    OpenSettings,
    OpenBookmarks,
    OpenSites,
    ToggleBookmarksBar,
    EditCss,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    Print,
    Devtools,
    ClearData,
    /// A Chrome Web Store link or id, or a `.crx` / `.zip` / folder path.
    InstallExtension { input: String },
    RemoveExtension { id: String },
    OpenExtensionsFolder,
    OpenDownload { id: u64 },
    ShowDownload { id: u64 },
    CancelDownload { id: u64 },
    PauseDownload { id: u64 },
    /// The ▾ button on a download card: a native menu of actions for it.
    DownloadMenu { id: u64 },
    ShelfReady,
    ShelfClose,
    SetSearchEngine { url: String },
    SettingsReady,
    SetAppearance { appearance: settings::AppearanceSettings },
    SetPrivacy { privacy: settings::PrivacySettings },
    SetReader { reader: settings::ReaderSettings },
    ToggleSiteAdblock,
    ToggleSiteSleep,
    ToggleSidebar,
    ToggleDark,
    SidebarPeek { show: bool },
    SetAppMode { mode: String },
    FlyoutReady,
    ToggleSidebarAutohide,
    ToggleVerticalTabs,
    SetMemoryPreset { preset: String },
    SetMemory { memory: MemorySettings },
    RestartBrowser,
    Suggest { text: String, x: f64, w: f64, #[serde(default)] y: f64 },
    NewPrivateTab,
    SuggestSel { sel: i32 },
    SuggestHide,
    ToggleBookmark,
    Reader,
    Pip,
    OpenHistory,
    OpenDownloads,
    ToggleSiteDark,
    SetTabGroup { id: u32, group: u8 },
    SwitchToTab { id: u32 },
    DragWindow,
    ResizeWindow { edge: String },
    ToggleMaximize,
    CloseWindow,
}

impl Browser {
    pub(crate) fn logical_size(&self) -> (f64, f64) {
        let size = self.window.inner_size();
        let scale = self.window.scale_factor();
        (size.width as f64 / scale, size.height as f64 / scale)
    }

    /// Where the page starts from the left: the window border plus the sidebar (which holds all
    /// of the browser's controls; there is no top bar).
    pub(crate) fn sidebar_w(&self) -> f64 {
        let a = &self.settings.appearance;
        WIN_BORDER
            + match a.sidebar_collapsed {
                _ if a.sidebar_autohide => SIDEBAR_STRIP_W,
                true => SIDEBAR_COLLAPSED_W,
                false => SIDEBAR_W,
            }
    }

    /// Where the page starts from the top: the window border, and the bookmarks bar when shown.
    pub(crate) fn top_chrome_h(&self) -> f64 {
        if self.settings.appearance.show_bookmarks_bar {
            WIN_BORDER + TOOLBAR_BOOKMARK_H
        } else {
            WIN_BORDER
        }
    }

    pub(crate) fn content_rect(&self) -> Rect {
        let (w, h) = self.logical_size();
        let left = self.sidebar_w();
        let top = self.top_chrome_h();
        let bottom = WIN_BORDER + if self.shelf_visible { SHELF_H } else { 0.0 };
        Rect {
            position: LogicalPosition::new(left, top).into(),
            size: LogicalSize::new((w - left - WIN_BORDER).max(1.0), (h - top - bottom).max(1.0)).into(),
        }
    }

    pub(crate) fn shelf_rect(&self) -> Rect {
        let (w, h) = self.logical_size();
        let left = self.sidebar_w();
        Rect {
            position: LogicalPosition::new(left, (h - WIN_BORDER - SHELF_H).max(0.0)).into(),
            size: LogicalSize::new((w - left - WIN_BORDER).max(1.0), SHELF_H).into(),
        }
    }

    /// Shows the download bar (building it the first time) and refreshes it.
    pub(crate) fn show_shelf(&mut self) {
        if self.shelf.is_none() {
            let rect = self.shelf_rect();
            let p_ipc = self.proxy.clone();
            let built = profile_builder(&mut self.web_context, &self.settings.memory)
                .with_html(include_str!("shelf.html"))
                .with_initialization_script(theme_init_script(&self.user_css))
                .with_initialization_script(internal_mode_init(&self.mode_css))
                .with_bounds(rect)
                .with_ipc_handler(move |req| {
                    let _ = p_ipc.send_event(UserEvent::Ipc(req.body().clone()));
                })
                .build_as_child(&self.window);
            match built {
                Ok(w) => self.shelf = Some(w),
                Err(e) => {
                    tracing::warn!("couldn't create the download bar: {e}");
                    return;
                }
            }
        }
        if !self.shelf_visible {
            self.shelf_visible = true;
            if let Some(s) = &self.shelf {
                let _ = s.set_visible(true);
            }
            self.layout();
        }
        self.sync_shelf();
    }

    pub(crate) fn hide_shelf(&mut self) {
        if self.shelf_visible {
            self.shelf_visible = false;
            if let Some(s) = &self.shelf {
                let _ = s.set_visible(false);
            }
            self.layout();
        }
    }

    pub(crate) fn sync_shelf(&self) {
        let Some(shelf) = &self.shelf else { return };
        let downloads: Vec<internal::Download> = self
            .start
            .lock()
            .map(|d| d.downloads.iter().rev().take(12).cloned().collect())
            .unwrap_or_default();
        let notice = self
            .notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(10))
            .map(|(text, _)| text.as_str());
        let state = json!({ "downloads": downloads, "notice": notice });
        let _ = shelf.evaluate_script(&format!("window.hbShelf && hbShelf.render({state})"));
    }

    /// A short status line on the download bar (and the settings page).
    pub(crate) fn set_notice(&mut self, text: String) {
        tracing::info!("{text}");
        self.notice = Some((text, Instant::now()));
        self.show_shelf();
        self.push_settings_state();
    }

    fn download_path(&self, id: u64) -> Option<String> {
        let data = self.start.lock().ok()?;
        data.downloads.iter().find(|d| d.id == id).map(|d| d.path.clone())
    }

    /// Installs an extension from a Chrome Web Store link or id, or a `.crx` / `.zip` / folder.
    pub(crate) fn install_extension(&mut self, input: String) {
        let from_store = ext_install::store_id_from_url(input.trim()).is_some() || ext_install::is_extension_id(input.trim());
        self.set_notice(if from_store { "Downloading extensionâ¦".into() } else { "Installing extensionâ¦".into() });
        let proxy = self.proxy.clone();
        ext_install::install_input_async(input, self.paths.extensions.clone(), move |result| {
            let _ = proxy.send_event(UserEvent::ExtensionUnpacked(result));
        });
    }

    /// The ▾ menu on a download card, as a native popup at the pointer.
    pub(crate) fn download_menu(&mut self, id: u64) {
        let Some(d) = self
            .start
            .lock()
            .ok()
            .and_then(|data| data.downloads.iter().find(|x| x.id == id).cloned())
        else {
            return;
        };
        const PAUSE: usize = 1;
        const BROWSE: usize = 2;
        const COPY: usize = 3;
        const INSTALL: usize = 4;
        const CANCEL: usize = 5;
        let picked = unsafe {
            use windows::core::HSTRING;
            let Ok(menu) = CreatePopupMenu() else { return };
            let add = |cmd: usize, text: &str| {
                let _ = AppendMenuW(menu, MF_STRING, cmd, &HSTRING::from(text));
            };
            let resumable = d.state == "paused" || (d.state == "interrupted" && d.can_resume);
            if d.state == "in_progress" {
                add(PAUSE, "Pause download");
            } else if resumable {
                add(PAUSE, "Resume download");
            }
            if d.state == "completed" && d.path.to_ascii_lowercase().ends_with(".crx") {
                add(INSTALL, "Install extension");
            }
            add(BROWSE, "Browse in Explorer");
            add(COPY, "Copy download URL");
            if d.state == "in_progress" || d.state == "paused" {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                add(CANCEL, "Cancel");
            }
            let mut pt = windows::Win32::Foundation::POINT::default();
            let _ = GetCursorPos(&mut pt);
            let hwnd = HWND(tao::platform::windows::WindowExtWindows::hwnd(&self.window) as _);
            let _ = SetForegroundWindow(hwnd);
            let picked = TrackPopupMenuEx(
                menu,
                (TPM_RETURNCMD | TPM_RIGHTALIGN | TPM_BOTTOMALIGN | TPM_NONOTIFY).0,
                pt.x,
                pt.y,
                hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            picked.0 as usize
        };
        match picked {
            PAUSE => downloads::toggle_pause(id),
            BROWSE => downloads::show_in_folder(&d.path),
            COPY => downloads::set_clipboard_text(&d.url),
            INSTALL => self.install_extension(d.path),
            CANCEL => downloads::cancel(id),
            _ => {}
        }
    }

    pub(crate) fn toolbar_rect(&self) -> Rect {
        let (w, h) = self.logical_size();
        Rect {
            position: LogicalPosition::new(0.0, 0.0).into(),
            size: LogicalSize::new(w, h).into(),
        }
    }

    pub(crate) fn flyout_rect(&self) -> Rect {
        let (_, h) = self.logical_size();
        Rect {
            position: LogicalPosition::new(WIN_BORDER, WIN_BORDER).into(),
            size: LogicalSize::new(SIDEBAR_W, (h - 2.0 * WIN_BORDER).max(1.0)).into(),
        }
    }

    pub(crate) fn sidebar_peek(&mut self, show: bool) {
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
                .with_initialization_script("window.__HEN_FLYOUT = true;")
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
                let _ = SetWindowPos(
                    HWND(hwnd.0 as _),
                    Some(HWND_TOP),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            let _ = f.evaluate_script("window.hb && hb.peek && hb.peek()");
        }
        self.flyout_visible = true;
        self.sync_toolbar();
    }

    pub(crate) fn build_suggest(&mut self) {
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

    /// `x`, `w`, `y`: the address field's left, width and bottom; the list hangs below it.
    pub(crate) fn show_suggestions(&mut self, text: &str, x: f64, w: f64, y: f64) {
        let tab_query = text.strip_prefix('@');
        let needle = tab_query.unwrap_or(text).trim().to_lowercase();
        let mut items: Vec<serde_json::Value> = self
            .tabs
            .iter()
            .filter(|_| !needle.is_empty() || tab_query.is_some())
            .filter(|t| t.title.to_lowercase().contains(&needle) || t.url.to_lowercase().contains(&needle))
            .take(if tab_query.is_some() { SUGGEST_MAX } else { 3 })
            .map(|t| {
                let title = if t.title.is_empty() { t.url.clone() } else { t.title.clone() };
                json!({ "url": t.url, "title": title, "tab": t.id })
            })
            .collect();
        if tab_query.is_none() {
            for b in self.bookmarks.query(text, SUGGEST_MAX) {
                if !items.iter().any(|i| i["url"] == b.url.as_str()) {
                    items.push(json!({ "url": b.url, "title": b.title, "bookmark": true }));
                }
            }
        }
        // A private tab gets no history suggestions.
        let private = self.tabs.get(self.active).is_some_and(|t| t.private);
        for e in if tab_query.is_some() || private { Vec::new() } else { self.history.query(text, SUGGEST_MAX) } {
            if items.len() >= SUGGEST_MAX {
                break;
            }
            if !items.iter().any(|i| i["url"] == e.url.as_str()) {
                items.push(json!({ "url": e.url, "title": e.title }));
            }
        }
        if items.is_empty() || self.suggest.is_none() {
            self.hide_suggestions();
            return;
        }
        let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".into());
        let _ = self.toolbar.evaluate_script(&format!("window.hb && hb.suggestions({json})"));
        let rect = Rect {
            position: LogicalPosition::new(x, y + 2.0).into(),
            size: LogicalSize::new(w.max(420.0), items.len() as f64 * SUGGEST_ROW_H + SUGGEST_PAD).into(),
        };
        if let Some(s) = &self.suggest {
            let _ = s.set_bounds(rect);
            let _ = s.evaluate_script(&format!("sg.set({json}); sg.sel(-1);"));
            let _ = s.set_visible(true);
            unsafe {
                let _ = SetWindowPos(
                    HWND(s.hwnd().0 as _),
                    Some(HWND_TOP),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        self.suggest_visible = true;
    }

    pub(crate) fn hide_suggestions(&mut self) {
        if !self.suggest_visible {
            return;
        }
        if let Some(s) = &self.suggest {
            let _ = s.set_visible(false);
        }
        self.suggest_visible = false;
    }

    /// Extension popups and the menu open from the bottom of the sidebar, over the page.
    pub(crate) fn popup_rect(&self) -> Rect {
        let (w, h) = self.logical_size();
        let width = POPUP_W.min(w - 20.0);
        let height = POPUP_H.min((h - 20.0).max(100.0));
        Rect {
            position: LogicalPosition::new(self.sidebar_w() + 6.0, (h - WIN_BORDER - height - 6.0).max(0.0)).into(),
            size: LogicalSize::new(width, height).into(),
        }
    }

    pub(crate) fn menu_rect(&self) -> Rect {
        let (w, h) = self.logical_size();
        let width = 260.0f64.min(w - 20.0);
        let height = 600.0f64.min((h - 20.0).max(100.0));
        Rect {
            position: LogicalPosition::new(self.sidebar_w() + 6.0, (h - WIN_BORDER - height - 6.0).max(0.0)).into(),
            size: LogicalSize::new(width, height).into(),
        }
    }

    pub(crate) fn layout(&self) {
        let _ = self.toolbar.set_bounds(self.toolbar_rect());
        self.keep_toolbar_behind();
        if let (Some(f), true) = (&self.flyout, self.flyout_visible) {
            let _ = f.set_bounds(self.flyout_rect());
        }
        if let (Some(s), true) = (&self.shelf, self.shelf_visible) {
            let _ = s.set_bounds(self.shelf_rect());
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

    pub(crate) fn refresh_start(&self) {
        let mut theme = format!("<style>{}</style>", self.user_css.replace("</style", "<\\/style"));
        if self.mode_css.contains('{') {
            theme.push_str(&format!("<style>{}</style>", self.mode_css));
        }
        let pair = |url: &str, title: &str| (url.to_owned(), title.to_owned());
        if let Ok(mut d) = self.start.lock() {
            d.theme = theme;
            d.search = self.settings.search_engine.clone();
            d.show_search = self.settings.appearance.start_show_search;
            d.bookmarks = self.bookmarks.all().iter().map(|b| pair(&b.url, &b.title)).collect();
            d.bookmark_items = self.bookmarks.all().to_vec();
            d.top = self.history.top_sites(8).iter().map(|e| pair(&e.url, &e.title)).collect();
            d.history = self.history.entries().to_vec();
            d.sites = self.settings.sites.clone();
            d.adblock_allowlist = self.settings.privacy.adblock_allowlist.clone();
            d.never_sleep_sites = self.settings.memory.never_sleep_sites.clone();
        }
    }

    pub(crate) fn sync_toolbar(&self) {
        let active = self.tabs.get(self.active);
        let (mut can_back, mut can_forward) = (false, false);
        self.with_active_core(|c| unsafe {
            let (mut b, mut f) = (windows_core::BOOL(0), windows_core::BOOL(0));
            let _ = c.CanGoBack(&mut b);
            let _ = c.CanGoForward(&mut f);
            (can_back, can_forward) = (b.as_bool(), f.as_bool());
        });
        let state = json!({
            "can_back": can_back,
            "can_forward": can_forward,
            "tabs": self.tabs.iter().enumerate().map(|(i, t)| json!({
                "id": t.id,
                "title": t.title,
                "url": t.url,
                "loading": t.loading,
                "active": i == self.active,
                "group": t.group,
                "pinned": t.pinned,
                "icon": t.favicon,
                "asleep": t.suspended || t.discarded,
                "sleeping": t.suspended || t.discarded,
                "never_sleep": self.is_site_never_sleep(&t.url),
                "private": t.private,
            })).collect::<Vec<_>>(),
            "bookmarks": self.bookmarks.all().iter().take(24).map(|b| json!({
                "url": b.url,
                "title": b.title,
                "folder": b.folder,
            })).collect::<Vec<_>>(),
            "layout": {
                "vertical": self.settings.appearance.vertical_tabs,
                "autohide": self.settings.appearance.sidebar_autohide,
                "collapsed": self.settings.appearance.sidebar_collapsed,
                "bookmarks_bar": self.settings.appearance.show_bookmarks_bar,
                "sidebar": self.sidebar_w(),
                "top": self.top_chrome_h(),
                "border": WIN_BORDER,
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
            "ambient": active
                .filter(|t| !t.settings_page.get() && !t.discarded)
                .map(|t| t.ambient.as_slice())
                .unwrap_or(&[]),
            "site_adblock": active
                .and_then(|t| settings::site_of(&t.url))
                .map(|site| !self.allowlist.borrow().contains(&site)),
            "site_never_sleep": active
                .map(|t| self.is_site_never_sleep(&t.url)),
            "private": active.is_some_and(|t| t.private),
            "url": active.map(|t| t.url.as_str()).filter(|u| !u.starts_with(START_URL)).map(display_url).unwrap_or_default(),
            "bookmarked": active
                .filter(|t| !t.settings_page.get() && history::canonical(&t.url).is_some())
                .map(|t| self.bookmarks.contains(&t.url)),
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
        self.window.set_title(&format!("{title} â Hen Browser"));
    }

    pub(crate) fn close_popup(&mut self) {
        if self.popup.take().is_some() {
            self.sync_toolbar();
        }
    }

    pub(crate) fn toggle_extension_popup(&mut self, extension_id: String) {
        if self.popup.as_ref().is_some_and(|p| p.extension_id == extension_id) {
            self.close_popup();
            return;
        }
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
                    let _ = SetWindowPos(
                        HWND(hwnd.0 as _),
                        Some(HWND_TOP),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                    );
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

    pub(crate) fn toggle_menu(&mut self) {
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
        let ram = serde_json::to_string(&self.ram_text()).unwrap_or_default();
        let built = profile_builder(&mut self.web_context, &self.settings.memory)
            .with_html(include_str!("menu.html"))
            .with_initialization_script(format!("window.__ramText = {ram};"))
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
                    let _ = SetWindowPos(
                        HWND(hwnd.0 as _),
                        Some(HWND_TOP),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                    );
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

    pub(crate) fn refresh_extensions(&self) {
        if let Some(tab) = self.tabs.iter().find(|t| !t.private) {
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

    /// Sends current settings and live memory usage to every open settings page.
    pub(crate) fn push_settings_state(&self) {
        let usage = self
            .tabs
            .first()
            .and_then(|t| unsafe { t.webview.controller().CoreWebView2() }.ok())
            .and_then(|core| process_memory(&core));
        let state = json!({
            "search_engine": self.settings.search_engine,
            "memory": self.settings.memory,
            "appearance": self.settings.appearance,
            "reader": self.settings.reader,
            "sites": self.settings.sites,
            "system_dark": self.system_theme.mode == chickenwm_core::theme::ThemeMode::Dark,
            "app_mode": self.system_theme.app_mode("browser").as_str(),
            "privacy": self.settings.privacy,
            "restart_needed": self.launch_memory.needs_restart(&self.settings.memory),
            "usage_mb": usage.map(|(bytes, _)| bytes / (1024 * 1024)),
            "processes": usage.map(|(_, n)| n),
            "toolbar_separate": self._toolbar_context.is_some(),
            "tabs": self.tabs.len(),
            "asleep": self.tabs.iter().filter(|t| t.suspended || t.discarded).count(),
            "extensions": self.extensions,
            "extensions_dir": self.paths.extensions.display().to_string(),
            "ext_notice": self.notice.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(30)).map(|(t, _)| t.as_str()),
        });
        for tab in self.tabs.iter().filter(|t| t.settings_page.get()) {
            let _ = tab.webview.evaluate_script(&format!("window.hbSettings && hbSettings.render({state})"));
        }
    }

    pub(crate) fn set_appearance(&mut self, mut appearance: settings::AppearanceSettings) {
        appearance.vertical_tabs = true;
        let relayout = appearance.vertical_tabs != self.settings.appearance.vertical_tabs
            || appearance.sidebar_collapsed != self.settings.appearance.sidebar_collapsed
            || appearance.show_bookmarks_bar != self.settings.appearance.show_bookmarks_bar;
        let theme_changed = appearance.theme != self.settings.appearance.theme;
        self.settings.appearance = appearance;
        self.save_settings();
        self.refresh_start();
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

    pub(crate) fn apply_memory_settings(&mut self, memory: MemorySettings) {
        let low = memory.low_memory_background;
        self.settings.memory = memory;
        self.save_settings();
        self.refresh_start();
        for (i, tab) in self.tabs.iter().enumerate() {
            if i != self.active {
                let level = if low { MemoryUsageLevel::Low } else { MemoryUsageLevel::Normal };
                let _ = tab.webview.set_memory_usage_level(level);
            }
        }
        self.sync_toolbar();
        self.push_settings_state();
    }

    /// Returns false to exit.
    pub(crate) fn handle(&mut self, event: UserEvent) -> bool {
        match event {
            UserEvent::Ipc(body) => {
                let Ok(cmd) = serde_json::from_str::<ToolbarCmd>(&body) else {
                    return true;
                };
                match cmd {
                    ToolbarCmd::Ready => {
                        self.sync_toolbar();
                        self.mode_css = String::from("\u{1}");
                        self.refresh_chrome_theme();
                        if let Ok(css) = std::fs::read_to_string(&self.paths.css) {
                            let script = format!(
                                "window.hb && hb.setUserStyle({});",
                                serde_json::to_string(&css).unwrap_or_default()
                            );
                            let _ = self.toolbar.evaluate_script(&script);
                        }
                        self.refresh_extensions();
                    }
                    ToolbarCmd::Suggest { text, x, w, y } => self.show_suggestions(&text, x, w, y),
                    ToolbarCmd::NewPrivateTab => self.open_private_tab(),
                    ToolbarCmd::SuggestSel { sel } => {
                        if let Some(s) = self.suggest.as_ref().filter(|_| self.suggest_visible) {
                            let _ = s.evaluate_script(&format!("sg.sel({sel})"));
                        }
                    }
                    ToolbarCmd::SuggestHide => self.hide_suggestions(),
                    ToolbarCmd::ToggleBookmark => self.toggle_bookmark(),
                    ToolbarCmd::Reader => {
                        self.close_popup();
                        let js = internal::reader_js_with_settings(&self.settings.reader);
                        self.eval_active(&js);
                    }
                    ToolbarCmd::Pip => {
                        self.close_popup();
                        self.eval_active(internal::PIP_JS);
                    }
                    ToolbarCmd::OpenHistory => {
                        self.close_popup();
                        self.open_internal("http://hen.history/");
                    }
                    ToolbarCmd::OpenDownloads => {
                        self.close_popup();
                        self.open_internal("http://hen.downloads/");
                    }
                    ToolbarCmd::OpenBookmarks => {
                        self.close_popup();
                        self.open_internal("http://hen.bookmarks/");
                    }
                    ToolbarCmd::OpenSites => {
                        self.close_popup();
                        self.open_internal("http://hen.sites/");
                    }
                    ToolbarCmd::ToggleBookmarksBar => {
                        let mut a = self.settings.appearance.clone();
                        a.show_bookmarks_bar = !a.show_bookmarks_bar;
                        self.set_appearance(a);
                    }
                    ToolbarCmd::ToggleSiteDark => {
                        self.close_popup();
                        self.toggle_site_dark();
                    }
                    ToolbarCmd::ToggleSiteSleep => {
                        self.close_popup();
                        self.toggle_site_sleep();
                    }
                    ToolbarCmd::SetTabGroup { id, group } => self.set_tab_group(id, group),
                    ToolbarCmd::PinTab { id } => self.pin_tab(id),
                    ToolbarCmd::ReorderTab { from_id, to_id } => self.reorder_tab(from_id, to_id),
                    ToolbarCmd::SwitchToTab { id } => {
                        self.hide_suggestions();
                        if let Some(i) = self.tab_index(id) {
                            self.switch_to(i);
                        }
                    }
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
                        self.refresh_start();
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
                    ToolbarCmd::Home => {
                        if let Some(tab) = self.tabs.get_mut(self.active) {
                            tab.settings_page.set(false);
                            let _ = tab.webview.load_url(HOME);
                        }
                    }
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
                    ToolbarCmd::ZoomIn | ToolbarCmd::ZoomOut | ToolbarCmd::ZoomReset => {
                        let mut applied = None;
                        if let Some(tab) = self.tabs.get(self.active) {
                            let controller = tab.webview.controller();
                            let mut zoom: f64 = 1.0;
                            if unsafe { controller.ZoomFactor(&mut zoom) }.is_ok() {
                                let next: f64 = match cmd {
                                    ToolbarCmd::ZoomIn => (zoom + 0.1).min(3.0),
                                    ToolbarCmd::ZoomOut => (zoom - 0.1).max(0.25),
                                    _ => 1.0,
                                };
                                let _ = unsafe { controller.SetZoomFactor(next) };
                                if let Some(p) = &self.popup {
                                    let _ = p.webview.evaluate_script(&format!("window.setZoom && setZoom({next});"));
                                }
                                applied = Some(next);
                            }
                        }
                        if let Some(z) = applied {
                            self.remember_zoom(z);
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
                    ToolbarCmd::InstallExtension { input } => self.install_extension(input),
                    ToolbarCmd::RemoveExtension { id } => {
                        let dir = self.extensions.iter().find(|e| e.id == id).map(|e| e.dir.clone().into());
                        if let Some(tab) = self.tabs.iter().find(|t| !t.private) {
                            if let Err(e) = extensions::remove_from_profile(
                                &tab.webview.controller(),
                                id,
                                dir,
                                self.proxy.clone(),
                            ) {
                                tracing::warn!("couldn't remove extension: {e}");
                            }
                        }
                    }
                    ToolbarCmd::OpenExtensionsFolder => {
                        downloads::open_file(&self.paths.extensions.display().to_string());
                    }
                    ToolbarCmd::OpenDownload { id } => {
                        if let Some(path) = self.download_path(id) {
                            downloads::open_file(&path);
                        }
                    }
                    ToolbarCmd::ShowDownload { id } => {
                        if let Some(path) = self.download_path(id) {
                            downloads::show_in_folder(&path);
                        }
                    }
                    ToolbarCmd::CancelDownload { id } => downloads::cancel(id),
                    ToolbarCmd::PauseDownload { id } => downloads::toggle_pause(id),
                    ToolbarCmd::DownloadMenu { id } => self.download_menu(id),
                    ToolbarCmd::ShelfReady => self.sync_shelf(),
                    ToolbarCmd::ShelfClose => self.hide_shelf(),
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
                        self.refresh_start();
                        self.apply_profile();
                        self.sync_toolbar();
                        self.push_settings_state();
                    }
                    ToolbarCmd::SetReader { reader } => {
                        self.settings.reader = reader;
                        self.save_settings();
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
                        let dark_now = self.effective_mode().unwrap_or(self.system_theme.mode_for("browser"))
                            == ThemeMode::Dark;
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
                    // Tabs always live in the sidebar now.
                    ToolbarCmd::ToggleVerticalTabs => self.close_popup(),
                    ToolbarCmd::SetMemoryPreset { preset } => {
                        if let Some(mut memory) = MemorySettings::preset(&preset) {
                            memory.never_sleep_sites = self.settings.memory.never_sleep_sites.clone();
                            self.apply_memory_settings(memory);
                        }
                    }
                    ToolbarCmd::SetMemory { mut memory } => {
                        memory.preset = "custom".to_owned();
                        self.apply_memory_settings(memory);
                    }
                    ToolbarCmd::RestartBrowser => {
                        if let Ok(exe) = std::env::current_exe() {
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
                    if !self.tabs[i].discarded && !self.tabs[i].settings_page.get() {
                        self.history.set_title(&self.tabs[i].url, &title);
                        self.bookmarks.set_title(&self.tabs[i].url, &title);
                        self.tabs[i].title = title;
                        self.sync_toolbar();
                    }
                }
            }
            UserEvent::PageLoad { tab, url, started } => {
                tracing::debug!("tab {tab}: page load {} {url}", if started { "started" } else { "finished" });
                if let Some(i) = self.tab_index(tab) {
                    let t = &mut self.tabs[i];
                    if t.discarded {
                        return true;
                    }
                    let internal = url == "about:blank" || url.starts_with("data:");
                    if t.settings_page.get() && !internal {
                        t.settings_page.set(false);
                    }
                    if t.reload_pending && url == "about:blank" && !t.settings_page.get() {
                        if !started {
                            t.reload_pending = false;
                            tracing::info!("tab {tab}: reload of {} was aborted, retrying", t.url);
                            let _ = t.webview.load_url(&t.url);
                        }
                        return true;
                    }
                    if !started && url != "about:blank" {
                        t.reload_pending = false;
                    }
                    if !t.settings_page.get() {
                        *t.page_url.borrow_mut() = url.clone();
                        if started && !t.private {
                            self.history.visit(&url);
                        }
                        t.url = url;
                    }
                    t.loading = started;
                    self.sync_toolbar();
                }
                if !started {
                    if let Some(i) = self.tab_index(tab) {
                        self.apply_site_settings(i);
                    }
                    self.refresh_extensions();
                    if self.tab_index(tab) == Some(self.active) {
                        self.measure_tone();
                    }
                }
            }
            UserEvent::OpenTab(url) => self.open_tab(&url),
            UserEvent::Shortcut(s) => match s {
                Shortcut::NewTab => return self.handle(UserEvent::Ipc(r#"{"cmd":"new_tab"}"#.into())),
                Shortcut::NewPrivateTab => self.open_private_tab(),
                Shortcut::Bookmark => self.toggle_bookmark(),
                Shortcut::Reader => return self.handle(UserEvent::Ipc(r#"{"cmd":"reader"}"#.into())),
                Shortcut::Pip => return self.handle(UserEvent::Ipc(r#"{"cmd":"pip"}"#.into())),
                Shortcut::History => return self.handle(UserEvent::Ipc(r#"{"cmd":"open_history"}"#.into())),
                Shortcut::Downloads => return self.handle(UserEvent::Ipc(r#"{"cmd":"open_downloads"}"#.into())),
                Shortcut::TabSearch => {
                    let _ = self.toolbar.focus();
                    let _ = self.toolbar.evaluate_script("hb.tabSearch && hb.tabSearch()");
                }
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
                if std::env::var_os("HENBROWSER_TEST_POPUP").is_some() && self.popup.is_none() {
                    if let Some(id) = self.extensions.first().map(|e| e.id.clone()) {
                        self.toggle_extension_popup(id);
                    }
                }
            }
            UserEvent::NavState => self.sync_toolbar(),
            UserEvent::RefreshExtensions => {
                self.refresh_extensions();
            }
            UserEvent::PopupLostFocus => {
                if let Some(popup) = &self.popup {
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
                let script = format!(
                    "window.hb && hb.setUserStyle({});",
                    serde_json::to_string(&css).unwrap_or_default()
                );
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
                let (r, g, b, a) = crate::chrome_bg(&css);
                let _ = self.toolbar.set_background_color((r, g, b, a));
                self.user_css = css;
            }
            UserEvent::ForgetSite(site) => {
                let n = self.history.forget_site(&site);
                tracing::info!("forgot {n} history entries of {site}");
                self.refresh_start();
            }
            UserEvent::BookmarkAction(action) => {
                match action {
                    crate::BookmarkAction::ReadLater { url, title } => {
                        self.bookmarks.add_read_later(&url, &title);
                    }
                    crate::BookmarkAction::Remove { url } => {
                        self.bookmarks.remove_url(&url);
                    }
                    crate::BookmarkAction::SetFolder { url, folder } => {
                        self.bookmarks.set_folder(&url, &folder);
                    }
                    crate::BookmarkAction::ImportHtml { path } => {
                        if let Ok(html) = std::fs::read_to_string(&path) {
                            let n = self.bookmarks.import_html(&html);
                            tracing::info!("imported {n} bookmarks from {path}");
                        }
                    }
                }
                self.refresh_start();
                self.sync_toolbar();
            }
            UserEvent::SiteAction(action) => {
                match action {
                    crate::SiteAction::Add(site) => {
                        self.settings.sites.entry(site).or_default();
                    }
                    crate::SiteAction::Reset(site) => {
                        self.settings.sites.remove(&site);
                        self.settings.privacy.adblock_allowlist.retain(|s| s != &site);
                        self.settings.memory.never_sleep_sites.retain(|s| s != &site);
                        *self.allowlist.borrow_mut() = self.settings.privacy.adblock_allowlist.clone();
                    }
                    crate::SiteAction::ResetAll => {
                        self.settings.sites.clear();
                        self.settings.privacy.adblock_allowlist.clear();
                        self.settings.memory.never_sleep_sites.clear();
                        self.allowlist.borrow_mut().clear();
                    }
                    crate::SiteAction::ToggleDark(site) => {
                        let e = self.settings.sites.entry(site.clone()).or_default();
                        e.force_dark = !e.force_dark;
                        if *e == settings::SiteSettings::default() {
                            self.settings.sites.remove(&site);
                        }
                    }
                    crate::SiteAction::ToggleSleep(site) => {
                        let e = self.settings.sites.entry(site.clone()).or_default();
                        e.never_sleep = !e.never_sleep;
                        self.settings.memory.never_sleep_sites.retain(|s| s != &site);
                        if *e == settings::SiteSettings::default() {
                            self.settings.sites.remove(&site);
                        }
                    }
                    crate::SiteAction::ToggleAdblock(site) => {
                        let list = &mut self.settings.privacy.adblock_allowlist;
                        if let Some(i) = list.iter().position(|s| *s == site) {
                            list.remove(i);
                        } else {
                            list.push(site);
                        }
                        *self.allowlist.borrow_mut() = list.clone();
                    }
                    crate::SiteAction::SetZoom { site, zoom } => {
                        let e = self.settings.sites.entry(site.clone()).or_default();
                        e.zoom = ((zoom - 1.0).abs() > 0.001).then_some(zoom);
                        if *e == settings::SiteSettings::default() {
                            self.settings.sites.remove(&site);
                        }
                    }
                }
                self.save_settings();
                self.refresh_start();
                self.sync_toolbar();
                self.push_settings_state();
            }
            UserEvent::Download(d) => {
                let mut is_new = false;
                if let Ok(mut data) = self.start.lock() {
                    match data.downloads.iter_mut().find(|x| x.id == d.id) {
                        Some(x) => *x = d,
                        None => {
                            is_new = true;
                            data.downloads.push(d);
                        }
                    }
                }
                if is_new {
                    self.show_shelf();
                } else {
                    self.sync_shelf();
                }
            }
            UserEvent::ExtensionUnpacked(result) => match result {
                Ok(dir) => match self.tabs.iter().find(|t| !t.private) {
                    Some(tab) => extensions::add_to_profile(&tab.webview.controller(), &dir, self.proxy.clone()),
                    None => self.set_notice("Extension unpacked; it loads on the next start".into()),
                },
                Err(e) => self.set_notice(format!("Couldn't install the extension: {e}")),
            },
            UserEvent::ExtensionInstalled(result) => {
                match result {
                    Ok(name) if name.is_empty() => self.set_notice("Extension installed".into()),
                    Ok(name) => self.set_notice(format!("Installed {name}")),
                    Err(e) => self.set_notice(format!("Couldn't install the extension: {e}")),
                }
                self.refresh_extensions();
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
                self.measure_tone();
                self.history.save_if_dirty();
                self.save_session();
                self.refresh_start();
                self.update_ram();
                self.sync_shelf();
                self.memory_tick();
                if self.tabs.iter().any(|t| t.settings_page.get()) {
                    self.push_settings_state();
                }
            }
        }
        true
    }
}
