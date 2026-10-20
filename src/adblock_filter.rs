//! Built-in network AdGuard ad/tracker blocking.
//!
//! Official AdGuard AdBlocker filter lists are downloaded into `~/.config/chickenwm/henbrowser/adblock/`
//! (refreshed periodically), compiled into the adblock engine on a background thread, and consulted
//! for sub-resource and iframe requests a tab makes via WebView2's `WebResourceRequested` event.
//! Top-level page loads are never blocked, so a false positive can't leave you on an error page.

use crate::UserEvent;
use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use adblock::Engine;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, SystemTime};
use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{take_pwstr, WebResourceRequestedEventHandler};
use windows_core::{Interface, HSTRING, PWSTR};

const LISTS: [(&str, &str); 7] = [
    ("adguard-base.txt", "https://filters.adtidy.org/extension/chromium/filters/2.txt"),
    ("adguard-tracking.txt", "https://filters.adtidy.org/extension/chromium/filters/3.txt"),
    ("adguard-annoyances.txt", "https://filters.adtidy.org/extension/chromium/filters/14.txt"),
    ("adguard-url-tracking.txt", "https://filters.adtidy.org/extension/chromium/filters/17.txt"),
    ("adguard-social.txt", "https://filters.adtidy.org/extension/chromium/filters/4.txt"),
    ("adguard-mobile.txt", "https://filters.adtidy.org/extension/chromium/filters/11.txt"),
    ("adguard-dns.txt", "https://adguardteam.github.io/AdGuardSDNSFilter/Filters/filter.txt"),
];
const REFRESH_AFTER: Duration = Duration::from_secs(4 * 24 * 60 * 60);

/// Shared engine slot; `None` until the background build finishes (requests pass meanwhile).
pub type SharedEngine = Rc<RefCell<Option<Engine>>>;

/// Sites (see `settings::site_of`) where blocking is turned off from the shield button.
pub type SharedAllowlist = Rc<RefCell<Vec<String>>>;

/// Extracts cosmetic CSS rules (element hiding selectors) for a given URL.
pub fn cosmetic_css_for_url(engine: &Engine, url: &str) -> Option<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let res = engine.url_cosmetic_resources(url);
    if res.hide_selectors.is_empty() {
        return None;
    }
    let selectors: Vec<&str> = res.hide_selectors.iter().map(|s| s.as_str()).collect();
    Some(format!("{} {{ display: none !important; }}", selectors.join(",\n")))
}

/// Returns the list text, downloading it first if it's missing or stale. A failed refresh falls
/// back to the cached copy.
fn ensure_list(dir: &Path, file: &str, url: &str) -> Option<String> {
    let path = dir.join(file);
    let stale = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() > REFRESH_AFTER)
        .unwrap_or(true);
    if stale {
        let tmp = dir.join(format!("{file}.download"));
        let ok = Command::new("curl.exe")
            .args(["-sSL", "--fail", "--max-time", "60", "-o"])
            .arg(&tmp)
            .arg(url)
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            let _ = std::fs::rename(&tmp, &path);
        } else {
            let _ = std::fs::remove_file(&tmp);
            tracing::warn!("adblock: couldn't refresh {file}; using cached copy if present");
        }
    }
    std::fs::read_to_string(&path).ok()
}

/// Builds the engine on a background thread and hands it to the UI via `UserEvent::AdblockReady`.
pub fn spawn_engine_loader(dir: PathBuf, proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        let _ = std::fs::create_dir_all(&dir);
        let mut set = FilterSet::new(false);
        let mut lists = 0;
        for (file, url) in LISTS {
            if let Some(text) = ensure_list(&dir, file, url) {
                set.add_filter_list(text, ParseOptions::default());
                lists += 1;
            }
        }
        if lists == 0 {
            tracing::warn!("adguard: no filter lists available; blocking disabled");
            return;
        }
        let engine = Engine::new_with_filter_set(set);
        tracing::info!("adguard: engine ready ({lists} lists)");
        let _ = proxy.send_event(UserEvent::AdblockReady(Box::new(engine)));
    });
}

fn request_type(context: COREWEBVIEW2_WEB_RESOURCE_CONTEXT) -> &'static str {
    match context {
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET => "stylesheet",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE => "image",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA => "media",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT => "font",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT => "script",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST
        | COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH
        | COREWEBVIEW2_WEB_RESOURCE_CONTEXT_EVENT_SOURCE => "xmlhttprequest",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET => "websocket",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING => "ping",
        _ => "other",
    }
}

/// Hooks request blocking into a tab. `page_url` is kept current by the tab so third-party
/// rules can tell first- from third-party requests.
pub fn install(
    controller: &ICoreWebView2Controller,
    engine: SharedEngine,
    allowlist: SharedAllowlist,
    page_url: Rc<RefCell<String>>,
    proxy: EventLoopProxy<UserEvent>,
    _tab_id: u32,
) -> windows_core::Result<()> {
    unsafe {
        let webview = controller.CoreWebView2()?;
        let filter = HSTRING::from("*");
        match webview.cast::<ICoreWebView2_22>() {
            // Also covers requests from workers and iframes.
            Ok(wv22) => wv22.AddWebResourceRequestedFilterWithRequestSourceKinds(
                &filter,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
            )?,
            Err(_) => webview.AddWebResourceRequestedFilter(&filter, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)?,
        }
        let env = webview.cast::<ICoreWebView2_2>()?.Environment()?;

        let mut token = 0i64;
        webview.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else {
                    return Ok(());
                };
                let engine = engine.borrow();
                let Some(engine) = engine.as_ref() else {
                    return Ok(());
                };
                let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT::default();
                args.ResourceContext(&mut context)?;
                let uri = {
                    let mut uri = PWSTR::null();
                    args.Request()?.Uri(&mut uri)?;
                    take_pwstr(uri)
                };
                if !(uri.starts_with("http://") || uri.starts_with("https://") || uri.starts_with("ws")) {
                    return Ok(());
                }
                let source = page_url.borrow();
                if allowlist.borrow().iter().any(|site| crate::settings::url_on_site(&source, site)) {
                    return Ok(());
                }

                // If this is a document context:
                // - If URI matches the main page URL, it's the top-level document: don't block.
                // - If URI does NOT match the main page URL, it's an iframe (subdocument): block if ad!
                let req_type = if context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT {
                    let clean_uri = uri.split(['?', '#']).next().unwrap_or(&uri).trim_end_matches('/');
                    let clean_source = source.split(['?', '#']).next().unwrap_or(&source).trim_end_matches('/');
                    if clean_uri == clean_source {
                        return Ok(());
                    }
                    "subdocument"
                } else {
                    request_type(context)
                };

                let Ok(request) = Request::new(&uri, &source, req_type, "GET") else {
                    return Ok(());
                };
                if engine.check_network_request(&request).should_block() {
                    let response = env.CreateWebResourceResponse(
                        None,
                        403,
                        &HSTRING::from("Blocked by AdGuard AdBlocker"),
                        &HSTRING::new(),
                    )?;
                    args.SetResponse(&response)?;
                    tracing::debug!("blocked {} request {uri} (page {source})", req_type);
                    let _ = proxy.send_event(UserEvent::Blocked);
                }
                Ok(())
            })),
            &mut token,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosmetic_resources() {
        let mut set = FilterSet::new(false);
        set.add_filter_list("example.com##.ad-box\n||doubleclick.net^".to_string(), ParseOptions::default());
        let engine = Engine::new_with_filter_set(set);
        let css = cosmetic_css_for_url(&engine, "https://example.com/").expect("has cosmetic css");
        assert!(css.contains(".ad-box"));
        assert!(css.contains("display: none !important;"));
    }
}
