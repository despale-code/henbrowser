use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::TrySuspendCompletedHandler;
use windows::core::{Interface, BOOL};
use wry::{MemoryUsageLevel, WebViewBuilderExtWindows};
use wry::WebViewExtWindows;
use wry::{NewWindowResponse, PageLoadEvent, WebView};

use crate::{
    adblock_filter, downloads, install_diagnostics, install_favicon, install_shortcuts, internal,
    internal_mode_init, profile_builder, serve_internal, settings, show_settings_page,
    theme_init_script, Browser, UserEvent, HOME, PRIVATE_START_URL,
};

pub(crate) struct Tab {
    pub(crate) id: u32,
    pub(crate) webview: WebView,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) loading: bool,
    pub(crate) page_url: Rc<RefCell<String>>,
    /// When the tab left the foreground; `None` while it is the active tab.
    pub(crate) hidden_since: Option<Instant>,
    pub(crate) suspended: bool,
    /// Unloaded to `about:blank`; `url` / `title` keep the real page and reload on switch.
    pub(crate) discarded: bool,
    /// Just reloaded from the unloaded state; if WebView2 aborts that navigation back to
    /// `about:blank`, keep `url` intact and try once more.
    pub(crate) reload_pending: bool,
    /// Colour group: 0 = none, 1..=6 = palette slot.
    pub(crate) group: u8,
    /// Pinned to the top/start of the tab strip / vertical sidebar.
    pub(crate) pinned: bool,
    /// True while this tab is showing `about:settings` (so it may post settings IPC).
    pub(crate) settings_page: Rc<Cell<bool>>,
    pub(crate) favicon: String,
    /// Detected page background: `Some(true)` = dark, `Some(false)` = light.
    pub(crate) tone: Option<bool>,
    /// Up to five `[r, g, b]` colors sampled from the page for the ambient chrome glow.
    pub(crate) ambient: Vec<[u8; 3]>,
    /// InPrivate: nothing it visits is kept (history, session, cookies), and no extensions.
    pub(crate) private: bool,
}

impl Browser {
    /// Writes the open tabs (and which one is active) when they differ from the last write.
    pub(crate) fn save_session(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
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
        let kept: Vec<(&str, u8, bool)> = all
            .iter()
            .zip(&self.tabs)
            .filter(|(_, t)| !t.private)
            .map(|(u, t)| (u.as_str(), t.group, t.pinned))
            .filter(|(u, _, _)| !u.is_empty() && *u != "about:blank")
            .collect();
        let urls: Vec<&str> = kept.iter().map(|k| k.0).collect();
        let groups: Vec<u8> = kept.iter().map(|k| k.1).collect();
        let pinned: Vec<bool> = kept.iter().map(|k| k.2).collect();
        let active_url = all.get(self.active).map(String::as_str).unwrap_or("");
        let active = urls.iter().position(|u| *u == active_url).unwrap_or(0);
        let json = json!({
            "active": active,
            "urls": urls,
            "groups": groups,
            "pinned": pinned
        })
        .to_string();
        if json != self.session_saved && std::fs::write(&self.session_path, &json).is_ok() {
            self.session_saved = json;
        }
    }

    /// Reopens the tabs from the previous run.
    pub(crate) fn restore_session(&mut self) {
        #[derive(Deserialize)]
        struct Session {
            active: usize,
            urls: Vec<String>,
            #[serde(default)]
            groups: Vec<u8>,
            #[serde(default)]
            pinned: Vec<bool>,
        }
        // Sessions saved before internal pages moved from `chicken.` to `hen.`.
        let Some(s) = std::fs::read_to_string(&self.session_path)
            .ok()
            .map(|t| t.replace("http://chicken.", "http://hen."))
            .and_then(|t| serde_json::from_str::<Session>(&t).ok())
        else {
            return;
        };
        let active = s.active.min(s.urls.len().saturating_sub(1));
        for (i, url) in s.urls.iter().enumerate() {
            if i == active || url == "about:settings" {
                self.open_tab(url);
            } else {
                self.open_tab_in("about:blank", true);
                if let Some(tab) = self.tabs.last_mut() {
                    tab.url = url.clone();
                    tab.discarded = true;
                }
            }
            if let Some(tab) = self.tabs.last_mut() {
                tab.group = s.groups.get(i).copied().unwrap_or(0).min(6);
                tab.pinned = s.pinned.get(i).copied().unwrap_or(false);
            }
        }
        if !self.tabs.is_empty() {
            self.switch_to(active);
        }
    }

    /// Colours a tab and moves it next to the other tabs of that colour.
    pub(crate) fn set_tab_group(&mut self, id: u32, group: u8) {
        let Some(i) = self.tab_index(id) else { return };
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        self.tabs[i].group = group.min(6);
        if group > 0 {
            if let Some(last) = self
                .tabs
                .iter()
                .rposition(|t| t.group == group && t.pinned == self.tabs[i].pinned && t.id != id)
            {
                let tab = self.tabs.remove(i);
                let at = if last >= i { last } else { last + 1 };
                self.tabs.insert(at.min(self.tabs.len()), tab);
            }
        }
        if let Some(a) = active_id.and_then(|a| self.tab_index(a)) {
            self.active = a;
        }
        self.sync_toolbar();
        self.save_session();
    }

    /// Pins or unpins a tab, keeping pinned tabs grouped at the front of the tab list.
    pub(crate) fn pin_tab(&mut self, id: u32) {
        let Some(i) = self.tab_index(id) else { return };
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        let new_pinned = !self.tabs[i].pinned;
        self.tabs[i].pinned = new_pinned;
        let tab = self.tabs.remove(i);
        let target = if new_pinned {
            self.tabs.iter().take_while(|t| t.pinned).count()
        } else {
            self.tabs.iter().take_while(|t| t.pinned).count()
        };
        self.tabs.insert(target.min(self.tabs.len()), tab);
        if let Some(a) = active_id.and_then(|a| self.tab_index(a)) {
            self.active = a;
        }
        self.sync_toolbar();
        self.save_session();
    }

    /// Reorders a tab via drag-and-drop in the tab strip / vertical sidebar.
    pub(crate) fn reorder_tab(&mut self, from_id: u32, to_id: u32) {
        if from_id == to_id {
            return;
        }
        let (Some(from_idx), Some(to_idx)) = (self.tab_index(from_id), self.tab_index(to_id)) else {
            return;
        };
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        let target_pinned = self.tabs[to_idx].pinned;
        let mut tab = self.tabs.remove(from_idx);
        tab.pinned = target_pinned;
        let insert_at = to_idx.min(self.tabs.len());
        self.tabs.insert(insert_at, tab);
        if let Some(a) = active_id.and_then(|a| self.tab_index(a)) {
            self.active = a;
        }
        self.sync_toolbar();
        self.save_session();
    }

    pub(crate) fn open_tab(&mut self, url: &str) {
        self.open_tab_in(url, false);
    }

    /// `background`: create the tab hidden and leave the current tab visible and active.
    pub(crate) fn open_tab_in(&mut self, url: &str, background: bool) {
        self.open_tab_with(url, background, false);
    }

    /// A private tab on the start page (no history on it), focused for typing.
    pub(crate) fn open_private_tab(&mut self) {
        self.close_popup();
        self.refresh_start();
        self.open_tab_with(PRIVATE_START_URL, false, true);
        let _ = self.toolbar.focus();
        let _ = self.toolbar.evaluate_script("window.hb && hb.focusAddress()");
    }

    pub(crate) fn open_tab_with(&mut self, url: &str, background: bool, private: bool) {
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
        let installed_ids: Vec<String> =
            if private { Vec::new() } else { self.extensions.iter().map(|e| e.id.clone()).collect() };

        let start = std::sync::Arc::clone(&self.start);
        let proto_proxy = std::sync::Mutex::new(self.proxy.clone());
        let mut builder = profile_builder(&mut self.web_context, &self.settings.memory)
            .with_custom_protocol("hen".into(), move |_, request| {
                let uri = request.uri().to_string();
                let host = request.uri().host().unwrap_or_default().to_owned();
                let html = serve_internal(&start, &proto_proxy, &host, &uri);
                wry::http::Response::builder()
                    .header("Content-Type", "text/html; charset=utf-8")
                    .body(std::borrow::Cow::Owned(html.into_bytes()))
                    .unwrap_or_default()
            })
            .with_initialization_script(theme_init_script(&self.user_css))
            .with_initialization_script(internal_mode_init(&self.mode_css))
            .with_initialization_script(
                if self.allowlist.borrow().iter().any(|s| s == "youtube.com") {
                    ""
                } else {
                    internal::YOUTUBE_JS
                },
            )
            .with_initialization_script(internal::webstore_js(&installed_ids))
            .with_bounds(bounds)
            .with_devtools(true)
            .with_ipc_handler(move |req| {
                // The settings page and our own `hen:` pages (served by the browser itself) may talk to it.
                let internal = req.uri().host().is_some_and(|h| h.starts_with("hen."));
                if ipc_allowed.get() || internal {
                    let _ = p_ipc.send_event(UserEvent::Ipc(req.body().clone()));
                } else if req.uri().host() == Some("chromewebstore.google.com") {
                    // The store's "Add to Chrome" (see `internal::webstore_js`): only an install
                    // request with a well-formed id gets through from a web page.
                    let msg: serde_json::Value = serde_json::from_str(req.body()).unwrap_or_default();
                    if let Some(id) = msg["id"].as_str().filter(|id| {
                        msg["cmd"] == "webstore_install" && crate::ext_install::is_extension_id(id)
                    }) {
                        let cmd = json!({ "cmd": "install_extension", "input": id });
                        let _ = p_ipc.send_event(UserEvent::Ipc(cmd.to_string()));
                    }
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

        if private {
            builder = builder.with_incognito(true);
        } else if !self.extensions_loaded {
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
        if let Err(e) = adblock_filter::install(
            &controller,
            self.engine.clone(),
            self.allowlist.clone(),
            page_url.clone(),
            self.proxy.clone(),
            id,
        ) {
            tracing::warn!("adblock hook failed for tab {id}: {e}");
        }
        install_shortcuts(&controller, self.proxy.clone(), true);
        install_diagnostics(&controller, id);
        install_favicon(&controller, id, self.proxy.clone());
        downloads::install_downloads(&controller, self.proxy.clone(), self.settings.downloads.directory.clone());
        install_history_changed(&controller, self.proxy.clone());
        if self.settings.downloads.speed_limit_kbps > 0 {
            if let Ok(core) = unsafe { controller.CoreWebView2() } {
                crate::apply_network_conditions(&core, self.settings.downloads.speed_limit_kbps);
            }
        }
        if !private && !self.extensions_loaded {
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
            reload_pending: false,
            group: 0,
            pinned: false,
            settings_page,
            favicon: String::new(),
            tone: None,
            ambient: Vec::new(),
            private,
        });
        if background {
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

    pub(crate) fn switch_to(&mut self, index: usize) {
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
    pub(crate) fn send_to_background(&mut self, index: usize) {
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
    pub(crate) fn bring_to_foreground(&mut self, index: usize) {
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
                *tab.page_url.borrow_mut() = tab.url.clone();
                tab.reload_pending = true;
                let _ = tab.webview.load_url(&tab.url);
            }
        }
        let _ = tab.webview.focus();
    }

    /// Whether `url` is excluded from background tab sleep/discard (`never_sleep_sites` or per-site `never_sleep`).
    pub(crate) fn is_site_never_sleep(&self, url: &str) -> bool {
        let Some(site) = settings::site_of(url) else { return false };
        if self.settings.memory.never_sleep_sites.iter().any(|s| s == &site) {
            return true;
        }
        self.settings
            .sites
            .get(&site)
            .is_some_and(|s| s.never_sleep)
    }

    /// Suspends, then unloads, tabs that have been in the background long enough. Tabs playing
    /// audio, pinned tabs on never-sleep sites, and the settings page are left alone.
    pub(crate) fn memory_tick(&mut self) {
        let m = &self.settings.memory;
        let suspend_after = Duration::from_secs(m.suspend_after_secs);
        let discard_after = Duration::from_secs(m.discard_after_mins * 60);
        let never_sleep_urls: Vec<bool> = self
            .tabs
            .iter()
            .map(|t| self.is_site_never_sleep(&t.url))
            .collect();
        let mut changed = false;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            let Some(since) = tab.hidden_since else { continue };
            if i == self.active || tab.settings_page.get() || never_sleep_urls.get(i).copied().unwrap_or(false) {
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
                tab.suspended = false;
                tab.loading = false;
                let _ = tab.webview.load_url("about:blank");
                changed = true;
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
                    if tab.suspended {
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.sync_toolbar();
        }
    }

    /// Returns false when the last tab closed (the browser should exit).
    pub(crate) fn close_tab(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return true;
        }
        let tab = &self.tabs[index];
        if !tab.private && tab.url != HOME && !tab.url.is_empty() && !tab.url.starts_with("about:") {
            self.closed_tabs.push(tab.url.clone());
            if self.closed_tabs.len() > 30 {
                self.closed_tabs.remove(0);
            }
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

    pub(crate) fn tab_index(&self, id: u32) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    pub(crate) fn with_active_core(&self, f: impl FnOnce(&ICoreWebView2)) {
        if let Some(tab) = self.tabs.get(self.active) {
            if let Ok(core) = unsafe { tab.webview.controller().CoreWebView2() } {
                f(&core);
            }
        }
    }
}

/// Same-document navigations (pushState on YouTube and other SPAs) change what Back/Forward can
/// do without a page load; tell the toolbar.
fn install_history_changed(controller: &ICoreWebView2Controller, proxy: tao::event_loop::EventLoopProxy<UserEvent>) {
    let Ok(core) = (unsafe { controller.CoreWebView2() }) else { return };
    let mut token = 0i64;
    unsafe {
        let _ = core.add_HistoryChanged(
            &webview2_com::HistoryChangedEventHandler::create(Box::new(move |_, _| {
                let _ = proxy.send_event(UserEvent::NavState);
                Ok(())
            })),
            &mut token,
        );
    }
}
