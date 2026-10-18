//! Built-in pages served by the `chicken` protocol (`http://hen.<page>/`) and the scripts the
//! browser injects into pages: reader mode, picture-in-picture, forced dark mode and the
//! YouTube ad layer.
//!
//! Pages are rendered from a snapshot the browser keeps current (`InternalData`). Actions on a
//! page (forget a site, open a download) are plain links back to the same host with a query;
//! the protocol handler turns them into `UserEvent`s and redirects back to the page.

use crate::history;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Download {
    pub id: u64,
    pub url: String,
    pub path: String,
    pub received: u64,
    /// 0 when the server didn't say.
    pub total: u64,
    /// `in_progress` | `paused` | `completed` | `cancelled` | `interrupted`
    pub state: String,
    /// A paused or failed download can pick up where it stopped.
    pub can_resume: bool,
}

/// What the internal pages show; refreshed by the browser.
#[derive(Clone)]
pub struct InternalData {
    /// browser.css plus the light/dark override, as `<style>` blocks.
    pub theme: String,
    pub search: String,
    pub show_search: bool,
    pub bookmarks: Vec<(String, String)>,
    pub bookmark_items: Vec<crate::bookmarks::Bookmark>,
    pub top: Vec<(String, String)>,
    pub history: Vec<history::Entry>,
    pub downloads: Vec<Download>,
    pub sites: std::collections::BTreeMap<String, crate::settings::SiteSettings>,
    pub adblock_allowlist: Vec<String>,
    pub never_sleep_sites: Vec<String>,
}

impl Default for InternalData {
    fn default() -> Self {
        Self {
            theme: String::new(),
            search: String::new(),
            show_search: true,
            bookmarks: Vec::new(),
            bookmark_items: Vec::new(),
            top: Vec::new(),
            history: Vec::new(),
            downloads: Vec::new(),
            sites: Default::default(),
            adblock_allowlist: Vec::new(),
            never_sleep_sites: Vec::new(),
        }
    }
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// `a%20b+c` → `a b c`.
pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `?a=1&b=x%20y` → value of `key`.
pub fn query_param(uri: &str, key: &str) -> Option<String> {
    let q = uri.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == key).then(|| url_decode(v))
    })
}

const PAGE_CSS: &str = r#"
  * { box-sizing: border-box; }
  body { margin: 0; background: var(--bg-color); color: var(--fg-color); font-family: var(--font-family, ui-monospace, monospace); font-size: 13px; }
  main { width: min(860px, 100% - 32px); margin: 40px auto; }
  h1 { font-size: 18px; font-weight: normal; margin: 0 0 18px; }
  form { display: flex; gap: 8px; margin-bottom: 16px; }
  input { flex: 1; height: 36px; padding: 0 12px; border: 1px solid var(--border-color); border-radius: var(--border-radius, 8px); background: var(--field-bg, var(--surface-color)); color: var(--fg-color); font: inherit; outline: none; }
  a { color: var(--fg-color); }
  .row { display: flex; align-items: center; gap: 12px; padding: 8px 10px; border-bottom: 1px solid var(--border-color); }
  .row:hover { background: var(--hover-bg); }
  .grow { flex: 1; min-width: 0; }
  .title, .url { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .url, .meta { color: var(--subtext-color); font-size: 12px; }
  .btn { padding: 4px 10px; border: 1px solid var(--border-color); border-radius: 6px; text-decoration: none; color: var(--fg-color); font-size: 12px; white-space: nowrap; }
  .btn:hover { background: var(--hover-bg); }
  .day { margin: 22px 0 6px; color: var(--subtext-color); font-size: 11px; letter-spacing: .08em; text-transform: uppercase; }
  .bar { height: 4px; background: var(--border-color); border-radius: 2px; margin-top: 6px; overflow: hidden; }
  .bar > div { height: 100%; background: var(--accent-color); }
  .empty { color: var(--faint-color, var(--subtext-color)); }
"#;

fn page(title: &str, theme: &str, body: &str, refresh_secs: Option<u32>) -> String {
    let refresh = refresh_secs.map(|s| format!(r#"<meta http-equiv="refresh" content="{s}">"#)).unwrap_or_default();
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8">{refresh}<title>{t}</title>
<style>:root{{--bg-color:#f0e6d3;--surface-color:#fffcf5;--fg-color:#2b1d14;--subtext-color:#6b5545;--border-color:rgba(59,42,31,.28);--hover-bg:rgba(59,42,31,.12);--accent-color:#3b2a1f}}</style>
{theme}<style>{PAGE_CSS}</style></head><body><main>{body}</main></body></html>"#,
        t = html_escape(title)
    )
}

fn day_label(secs: u64, now: u64) -> String {
    let days_ago = now / 86_400 - secs / 86_400;
    match days_ago {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        n if n < 7 => format!("{n} days ago"),
        n if n < 30 => format!("{} weeks ago", n / 7),
        n => format!("{} months ago", n / 30),
    }
}

/// `http://hen.history/?q=…`: visited pages, newest first, grouped by day, with
/// "forget this site".
pub fn history_page(data: &InternalData, query: &str) -> String {
    let now = history::now();
    let q = query.trim().to_lowercase();
    let mut entries: Vec<&history::Entry> = data
        .history
        .iter()
        .filter(|e| q.is_empty() || e.url.to_lowercase().contains(&q) || e.title.to_lowercase().contains(&q))
        .collect();
    entries.sort_by(|a, b| b.last.cmp(&a.last));
    let mut body = format!(
        r#"<h1>History</h1><form><input name="q" value="{}" placeholder="Search history" autofocus></form>"#,
        html_escape(query)
    );
    let mut current_day = String::new();
    for e in entries.iter().take(500) {
        let day = day_label(e.last, now);
        if day != current_day {
            body.push_str(&format!(r#"<div class="day">{day}</div>"#));
            current_day = day;
        }
        let site = crate::settings::site_of(&e.url).unwrap_or_default();
        let title = if e.title.is_empty() { &e.url } else { &e.title };
        body.push_str(&format!(
            r#"<div class="row"><div class="grow"><div class="title"><a href="{u}">{t}</a></div><div class="url">{u} · {v} visit{s}</div></div><a class="btn" href="/?forget={site_q}" title="Remove every page from {site_h} from history">Forget site</a></div>"#,
            u = html_escape(&e.url),
            t = html_escape(title),
            v = e.visits,
            s = if e.visits == 1 { "" } else { "s" },
            site_q = html_escape(&site),
            site_h = html_escape(&site),
        ));
    }
    if entries.is_empty() {
        body.push_str(r#"<p class="empty">Nothing here.</p>"#);
    }
    page("History", &data.theme, &body, None)
}

/// `http://hen.downloads/`: downloads of this session (refreshes while any is running).
pub fn downloads_page(data: &InternalData) -> String {
    let mut body = String::from(r#"<h1>Downloads</h1>"#);
    let running = data.downloads.iter().any(|d| d.state == "in_progress");
    if data.downloads.is_empty() {
        body.push_str(r#"<p class="empty">No downloads yet in this session.</p>"#);
    } else {
        body.push_str(r#"<p><a class="btn" href="/?clear=1">Clear finished</a></p>"#);
    }
    for d in data.downloads.iter().rev() {
        let name = std::path::Path::new(&d.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| d.url.clone());
        let size = |b: u64| {
            if b >= 1 << 30 {
                format!("{:.2} GB", b as f64 / f64::from(1u32 << 30))
            } else if b >= 1 << 20 {
                format!("{:.1} MB", b as f64 / f64::from(1u32 << 20))
            } else {
                format!("{:.0} KB", b as f64 / 1024.0)
            }
        };
        let (status, pct) = match d.state.as_str() {
            "completed" => (format!("Done · {}", size(d.received)), 100.0),
            "interrupted" => ("Failed".to_owned(), 0.0),
            "cancelled" => ("Cancelled".to_owned(), 0.0),
            "paused" => (format!("Paused · {}", size(d.received)), 0.0),
            _ if d.total > 0 => (
                format!("{} of {}", size(d.received), size(d.total)),
                d.received as f64 * 100.0 / d.total as f64,
            ),
            _ => (format!("{} so far", size(d.received)), 0.0),
        };
        let actions = if d.state == "completed" {
            format!(r#"<a class="btn" href="/?open={id}">Open</a> <a class="btn" href="/?show={id}">Show in folder</a>"#, id = d.id)
        } else {
            String::new()
        };
        let bar = if d.state == "in_progress" { format!(r#"<div class="bar"><div style="width:{pct:.0}%"></div></div>"#) } else { String::new() };
        body.push_str(&format!(
            r#"<div class="row"><div class="grow"><div class="title">{n}</div><div class="meta">{st} · <span class="url">{u}</span></div>{bar}</div>{actions}</div>"#,
            n = html_escape(&name),
            st = html_escape(&status),
            u = html_escape(&d.url),
        ));
    }
    page("Downloads", &data.theme, &body, running.then_some(1))
}

/// `http://hen.bookmarks/`: full bookmarks & Read Later manager with search, folder filter,
/// and HTML import.
pub fn bookmarks_page(data: &InternalData, query: &str, folder_filter: &str) -> String {
    let q = query.trim().to_lowercase();
    let mut folders = std::collections::BTreeSet::new();
    for b in &data.bookmark_items {
        if !b.folder.trim().is_empty() {
            folders.insert(b.folder.trim().to_owned());
        }
    }
    let mut body = format!(
        r#"<h1>Bookmarks &amp; Read Later</h1>
<form><input name="q" value="{}" placeholder="Search bookmarks" autofocus><input type="hidden" name="folder" value="{}"></form>
<form style="margin-bottom:14px"><input name="import_path" placeholder="Path to exported bookmarks.html (Netscape format)"><button class="btn" type="submit" style="background:var(--surface-color);cursor:pointer">Import HTML</button></form>"#,
        html_escape(query),
        html_escape(folder_filter),
    );
    body.push_str(r#"<div style="display:flex;gap:6px;flex-wrap:wrap;margin-bottom:16px">"#);
    let pill = |label: &str, val: &str| {
        let active = if folder_filter == val { "border-color:var(--accent-color);font-weight:bold" } else { "" };
        format!(
            r#"<a class="btn" style="{active}" href="/?folder={v}&amp;q={q}">{l}</a>"#,
            v = html_escape(val),
            q = html_escape(query),
            l = html_escape(label)
        )
    };
    body.push_str(&pill("All", ""));
    body.push_str(&pill("Read Later", "__read_later__"));
    for f in &folders {
        body.push_str(&pill(f, f));
    }
    body.push_str("</div>");

    let filtered: Vec<&crate::bookmarks::Bookmark> = data
        .bookmark_items
        .iter()
        .filter(|b| {
            let folder_ok = match folder_filter {
                "" => true,
                "__read_later__" => b.read_later || b.folder.eq_ignore_ascii_case("Read Later"),
                f => b.folder == f,
            };
            let q_ok = q.is_empty()
                || b.url.to_lowercase().contains(&q)
                || b.title.to_lowercase().contains(&q)
                || b.folder.to_lowercase().contains(&q);
            folder_ok && q_ok
        })
        .collect();

    if filtered.is_empty() {
        body.push_str(r#"<p class="empty">No matching bookmarks.</p>"#);
    }
    for b in filtered {
        let title = if b.title.is_empty() { &b.url } else { &b.title };
        let folder_badge = if b.read_later {
            "Read Later"
        } else if b.folder.is_empty() {
            "Unsorted"
        } else {
            &b.folder
        };
        body.push_str(&format!(
            r#"<div class="row"><div class="grow"><div class="title"><a href="{u}">{t}</a></div><div class="url">{u} · <span class="meta">[{f}]</span></div></div><form style="margin:0"><input type="hidden" name="folder_url" value="{u}"><input name="set_folder" value="{raw_f}" placeholder="Folder" style="width:110px;height:28px;font-size:11px"></form><a class="btn" href="/?remove={u}">Remove</a></div>"#,
            u = html_escape(&b.url),
            t = html_escape(title),
            f = html_escape(folder_badge),
            raw_f = html_escape(&b.folder),
        ));
    }
    page("Bookmarks", &data.theme, &body, None)
}

/// `http://hen.sites/`: list, edit, and reset all per-site settings (`zoom`, `force_dark`,
/// `never_sleep`, and `adblock_allowlist`).
pub fn sites_page(data: &InternalData) -> String {
    let mut all_sites = std::collections::BTreeSet::new();
    for k in data.sites.keys() {
        all_sites.insert(k.clone());
    }
    for s in &data.adblock_allowlist {
        all_sites.insert(s.clone());
    }
    for s in &data.never_sleep_sites {
        all_sites.insert(s.clone());
    }
    let mut body = String::from(
        r#"<h1>Per-Site Settings</h1>
<p class="meta" style="margin-bottom:14px">Manage site-specific zoom, forced dark mode, tab sleep exclusions, and ad-blocker allowlist.</p>
<form style="margin-bottom:16px"><input name="add_site" placeholder="example.com" required><button class="btn" type="submit" style="background:var(--surface-color);cursor:pointer">Add / Configure Site</button><a class="btn" href="/?reset_all=1">Reset all sites</a></form>"#,
    );
    if all_sites.is_empty() {
        body.push_str(r#"<p class="empty">No custom per-site settings configured.</p>"#);
    }
    for site in all_sites {
        let cfg = data.sites.get(&site).cloned().unwrap_or_default();
        let zoom_pct = (cfg.zoom.unwrap_or(1.0) * 100.0).round() as i32;
        let allow_ads = data.adblock_allowlist.iter().any(|s| s == &site);
        let no_sleep = cfg.never_sleep || data.never_sleep_sites.iter().any(|s| s == &site);
        body.push_str(&format!(
            r#"<div class="row"><div class="grow"><div class="title">{s}</div><div class="meta">Zoom: {z}% · Dark: {d} · Never sleep: {ns} · Adblock: {ab}</div></div><a class="btn" href="/?zoom_site={s}&amp;zoom=100">100%</a><a class="btn" href="/?toggle_dark={s}">Toggle Dark</a><a class="btn" href="/?toggle_sleep={s}">Toggle Never-Sleep</a><a class="btn" href="/?toggle_adblock={s}">Toggle Adblock</a><a class="btn" href="/?reset_site={s}">Reset</a></div>"#,
            s = html_escape(&site),
            z = zoom_pct,
            d = if cfg.force_dark { "Forced" } else { "Default" },
            ns = if no_sleep { "Yes" } else { "No" },
            ab = if allow_ads { "Off (Allowed)" } else { "On" },
        ));
    }
    page("Site Settings", &data.theme, &body, None)
}

/// Builds the Reader mode script with configured default typography (`font`, `width`, `theme`)
/// plus interactive typography controls and a `"Save to Read Later"` button.
pub fn reader_js_with_settings(r: &crate::settings::ReaderSettings) -> String {
    READER_JS
        .replace("__READER_FONT__", &r.font)
        .replace("__READER_WIDTH__", &r.width.to_string())
        .replace("__READER_THEME__", &r.theme)
}

/// Reader mode: rebuilds the page around its main text (the element with the most paragraph
/// text), in a readable column with typography controls (`font`, `width`, `theme`) and a
/// `"Save to Read Later"` action. Running it again reloads.
pub const READER_JS: &str = r#"(function(){
  if (window.__chickenReader) { location.reload(); return; }
  const score = (el) => [...el.querySelectorAll('p')].reduce((n, p) => n + (p.innerText || '').trim().length, 0);
  let best = document.querySelector('article') || document.body;
  let bestScore = score(best);
  for (const el of document.querySelectorAll('main, [role=main], .post, .article, .content, #content, .entry-content, section, div')) {
    const s = score(el);
    if (s > bestScore * 1.2 && (el.innerText || '').length < (document.body.innerText || '').length * 0.98) { best = el; bestScore = s; }
  }
  const clone = best.cloneNode(true);
  clone.querySelectorAll('script, style, nav, aside, footer, form, iframe, button, [role=navigation], .ad, .ads, .share, .social, .comments').forEach(n => n.remove());
  const title = (document.querySelector('h1') || {}).innerText || document.title;
  const pageUrl = location.href;
  window.__chickenReader = true;
  document.head.innerHTML = '<meta charset="utf-8"><title>' + document.title.replace(/</g,'&lt;') + '</title>';
  const initFont = ("__READER_FONT__".startsWith("__") ? "serif" : "__READER_FONT__");
  const initWidth = parseInt("__READER_WIDTH__", 10) || 680;
  const initTheme = ("__READER_THEME__".startsWith("__") ? "match" : "__READER_THEME__");
  const style = document.createElement('style');
  style.textContent = `
    :root { --r-bg: #f7f1e6; --r-fg: #2b1d14; --r-link: #6b4a2b; --r-width: ${initWidth}px; --r-font: Georgia, 'Iowan Old Style', serif; }
    @media (prefers-color-scheme: dark) { :root[data-rtheme="match"] { --r-bg: #1d1c1a; --r-fg: #e8e4dc; --r-link: #d8c7a8; } }
    :root[data-rtheme="light"] { --r-bg: #ffffff; --r-fg: #1f1f1f; --r-link: #0b57d0; }
    :root[data-rtheme="sepia"] { --r-bg: #f7f1e6; --r-fg: #2b1d14; --r-link: #6b4a2b; }
    :root[data-rtheme="dark"]  { --r-bg: #161618; --r-fg: #e4e2dd; --r-link: #8ab4f8; }
    :root[data-rfont="serif"] { --r-font: Georgia, 'Iowan Old Style', serif; }
    :root[data-rfont="sans"]  { --r-font: system-ui, -apple-system, 'Segoe UI', sans-serif; }
    :root[data-rfont="mono"]  { --r-font: 'DepartureMono Nerd Font', 'Cascadia Code', ui-monospace, monospace; }
    html { background: var(--r-bg); color: var(--r-fg); transition: background 180ms, color 180ms; }
    body { max-width: var(--r-width); margin: 64px auto 48px; padding: 0 20px; font: 19px/1.65 var(--r-font); }
    h1 { font-size: 32px; line-height: 1.25; margin-bottom: 28px; }
    img, video, figure { max-width: 100%; height: auto; }
    pre, code { font-size: 14px; white-space: pre-wrap; }
    a { color: var(--r-link); }
    .chicken-reader-bar { position: fixed; top: 10px; right: 16px; display: flex; gap: 6px; align-items: center; font: 12px system-ui; background: rgba(128,128,128,0.14); backdrop-filter: blur(8px); padding: 5px 10px; border-radius: 8px; z-index: 9999; }
    .chicken-reader-bar button, .chicken-reader-bar a { all: unset; cursor: pointer; padding: 3px 7px; border-radius: 5px; color: inherit; opacity: 0.85; }
    .chicken-reader-bar button:hover, .chicken-reader-bar a:hover { opacity: 1; background: rgba(128,128,128,0.22); }`;
  document.head.appendChild(style);
  const root = document.documentElement;
  root.dataset.rfont = initFont;
  root.dataset.rtheme = initTheme;
  document.body.innerHTML = '';
  const bar = document.createElement('div');
  bar.className = 'chicken-reader-bar';
  bar.innerHTML = `
    <button data-font="serif">Serif</button>
    <button data-font="sans">Sans</button>
    <button data-font="mono">Mono</button>
    <span>·</span>
    <button data-w="560">Narrow</button>
    <button data-w="680">Medium</button>
    <button data-w="840">Wide</button>
    <span>·</span>
    <button data-theme="match">Theme</button>
    <button data-theme="sepia">Sepia</button>
    <button data-theme="dark">Dark</button>
    <span>·</span>
    <a id="chicken-read-later">Save to Read Later</a>
    <span>·</span>
    <button id="chicken-reader-exit">Exit (F9)</button>`;
  bar.querySelectorAll('[data-font]').forEach(b => b.onclick = () => { root.dataset.rfont = b.dataset.font; });
  bar.querySelectorAll('[data-w]').forEach(b => b.onclick = () => { root.style.setProperty('--r-width', b.dataset.w + 'px'); });
  bar.querySelectorAll('[data-theme]').forEach(b => b.onclick = () => { root.dataset.rtheme = b.dataset.theme; });
  const rl = bar.querySelector('#chicken-read-later');
  rl.href = 'http://hen.bookmarks/?read_later=' + encodeURIComponent(pageUrl) + '&title=' + encodeURIComponent(title);
  bar.querySelector('#chicken-reader-exit').onclick = () => location.reload();
  const h = document.createElement('h1');
  h.textContent = title;
  document.body.append(bar, h);
  if (clone.querySelector('h1')) clone.querySelector('h1').remove();
  document.body.appendChild(clone);
  scrollTo(0, 0);
})();"#;

/// Picture-in-picture for the page's largest playing (or first) video.
pub const PIP_JS: &str = r#"(async function(){
  try {
    if (document.pictureInPictureElement) { await document.exitPictureInPicture(); return; }
    const vids = [...document.querySelectorAll('video')].filter(v => v.readyState > 0);
    const v = vids.find(v => !v.paused) || vids.sort((a, b) => b.clientWidth * b.clientHeight - a.clientWidth * a.clientHeight)[0];
    if (!v) { alert('No video on this page.'); return; }
    v.disablePictureInPicture = false;
    await v.requestPictureInPicture();
  } catch (e) { alert('Picture-in-picture is not available here: ' + e.message); }
})();"#;

/// Forced dark mode for a site that has none: invert, then re-invert media.
pub const DARK_JS: &str = r#"(function(){
  if (document.getElementById('hen-force-dark')) return;
  const s = document.createElement('style');
  s.id = 'hen-force-dark';
  s.textContent = 'html{filter:invert(.92) hue-rotate(180deg)!important;background:#fff!important}img,video,picture,canvas,iframe,svg image,[style*="background-image"]{filter:invert(1) hue-rotate(180deg)!important}';
  (document.head || document.documentElement).appendChild(s);
})();"#;

pub const UNDARK_JS: &str = "document.getElementById('hen-force-dark')?.remove();";

/// Chrome Web Store support: the store's own "Add to Chrome" button calls Chrome-only APIs
/// (`chrome.webstorePrivate`, `chrome.management`) that WebView2 doesn't have. These stand-ins
/// answer the store's questions (installed? installable?) and turn an install into a message
/// the browser acts on (it downloads the package and installs it). `__HEN_INSTALLED__` is
/// replaced with the ids already installed. Runs only on chromewebstore.google.com.
pub fn webstore_js(installed: &[String]) -> String {
    let ids = serde_json::to_string(installed).unwrap_or_else(|_| "[]".into());
    WEBSTORE_JS.replace("__HEN_INSTALLED__", &ids)
}

const WEBSTORE_JS: &str = r#"(function(){
  if (location.hostname !== 'chromewebstore.google.com') return;
  const installed = new Set(__HEN_INSTALLED__);
  // Look like Chrome to the store (it hides the install button for other browsers).
  try {
    const brands = [
      { brand: 'Chromium', version: (navigator.userAgent.match(/Chrome\/(\d+)/) || [])[1] || '131' },
      { brand: 'Google Chrome', version: (navigator.userAgent.match(/Chrome\/(\d+)/) || [])[1] || '131' },
      { brand: 'Not_A Brand', version: '24' },
    ];
    if (navigator.userAgentData) {
      const real = navigator.userAgentData;
      Object.defineProperty(navigator, 'userAgentData', { get: () => ({
        brands, mobile: false, platform: real.platform || 'Windows',
        getHighEntropyValues: (h) => real.getHighEntropyValues(h).then((v) => Object.assign(v, { brands, fullVersionList: brands })),
        toJSON: () => ({ brands, mobile: false, platform: real.platform || 'Windows' }),
      }) });
    }
  } catch (_) {}
  // A method that answers through a callback (last argument) or a promise, like Chrome's.
  const api = (fn) => function (...args) {
    const cb = typeof args[args.length - 1] === 'function' ? args.pop() : null;
    const result = fn(...args);
    if (cb) { setTimeout(() => cb(result), 0); return; }
    return Promise.resolve(result);
  };
  const install = (id) => {
    if (!/^[a-p]{32}$/.test(id || '')) return 'invalid_id';
    window.ipc.postMessage(JSON.stringify({ cmd: 'webstore_install', id }));
    installed.add(id);
    return '';
  };
  const known = {
    beginInstallWithManifest3: api((d) => install(d && d.id)),
    completeInstall: api(() => ''),
    install: api((id) => install(id)),
    getExtensionStatus: api((id) => (installed.has(id) ? 'enabled' : 'installable')),
    getBrowserLogin: api(() => ({ login: '' })),
    getStoreLogin: api(() => ''),
    setStoreLogin: api(() => undefined),
    getWebGLStatus: api(() => 'webgl_allowed'),
    getIsLauncherEnabled: api(() => false),
    isInIncognitoMode: api(() => false),
    isPendingCustodianApproval: api(() => false),
    getReferrerChain: api(() => ''),
    getMV2DeprecationStatus: api(() => 'inactive'),
    getExtensionInfo: api(() => undefined),
  };
  // Anything else the store asks answers "nothing", rather than throwing.
  const webstorePrivate = new Proxy(known, { get: (t, k) => (k in t ? t[k] : api(() => undefined)) });
  const extInfo = (id) => ({ id, enabled: true, installType: 'normal', type: 'extension', mayDisable: true });
  const management = new Proxy({
    getAll: api(() => [...installed].map(extInfo)),
    get: api((id) => (installed.has(id) ? extInfo(id) : undefined)),
    setEnabled: api(() => undefined),
    uninstall: api(() => undefined),
    onInstalled: { addListener() {}, removeListener() {} },
    onUninstalled: { addListener() {}, removeListener() {} },
    onEnabled: { addListener() {}, removeListener() {} },
    onDisabled: { addListener() {}, removeListener() {} },
  }, { get: (t, k) => (k in t ? t[k] : api(() => undefined)) });
  window.chrome = window.chrome || {};
  try { Object.defineProperty(window.chrome, 'webstorePrivate', { value: webstorePrivate, configurable: true }); } catch (_) {}
  try { Object.defineProperty(window.chrome, 'management', { value: management, configurable: true }); } catch (_) {}
})();"#;

/// YouTube ad layer (the network blocker alone can't reach these: the ads come in the same
/// JSON as the video). Strips ad fields from the player response before the player reads it,
/// skips ads that still start, and hides ad slots on the page.
pub const YOUTUBE_JS: &str = r#"(function(){
  if (!/(^|\.)youtube\.com$/.test(location.hostname) || window.__chickenYt) return;
  window.__chickenYt = true;
  const AD_KEYS = ['adPlacements', 'adSlots', 'playerAds', 'adBreakHeartbeatParams'];
  const prune = (o) => {
    if (!o || typeof o !== 'object') return o;
    for (const k of AD_KEYS) if (k in o) delete o[k];
    if (o.playerResponse) prune(o.playerResponse);
    if (o.response) prune(o.response);
    return o;
  };
  const parse = JSON.parse;
  JSON.parse = function(text, reviver) { return prune(parse.call(this, text, reviver)); };
  let ipr;
  try {
    Object.defineProperty(window, 'ytInitialPlayerResponse', { configurable: true, get: () => ipr, set: (v) => { ipr = prune(v); } });
  } catch (_) {}
  const _fetch = window.fetch;
  window.fetch = async function(...args) {
    const res = await _fetch.apply(this, args);
    const url = String(args[0] && args[0].url || args[0]);
    if (!/\/youtubei\/v1\/(player|next)/.test(url)) return res;
    try {
      const json = prune(await res.clone().json());
      return new Response(JSON.stringify(json), { status: res.status, statusText: res.statusText, headers: res.headers });
    } catch (_) { return res; }
  };
  const css = document.createElement('style');
  css.textContent = 'ytd-ad-slot-renderer,ytd-in-feed-ad-layout-renderer,ytd-promoted-sparkles-web-renderer,ytd-display-ad-renderer,#player-ads,#masthead-ad,ytd-banner-promo-renderer,.ytd-merch-shelf-renderer,ytd-statement-banner-renderer,ytd-rich-item-renderer:has(ytd-ad-slot-renderer){display:none!important}';
  const addCss = () => (document.head || document.documentElement).appendChild(css);
  if (document.documentElement) addCss(); else document.addEventListener('DOMContentLoaded', addCss);
  setInterval(() => {
    const player = document.querySelector('.html5-video-player');
    if (player && player.classList.contains('ad-showing')) {
      const v = player.querySelector('video');
      if (v && isFinite(v.duration)) { v.muted = true; v.currentTime = v.duration; }
      document.querySelector('.ytp-ad-skip-button, .ytp-ad-skip-button-modern, .ytp-skip-ad-button')?.click();
    }
  }, 300);
})();"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing() {
        assert_eq!(query_param("http://hen.history/?q=rust%20book&x=1", "q").as_deref(), Some("rust book"));
        assert_eq!(query_param("http://hen.history/?forget=github.com", "forget").as_deref(), Some("github.com"));
        assert_eq!(query_param("http://hen.history/", "q"), None);
        assert_eq!(url_decode("a+b%2Bc%zz"), "a b+c%zz");
    }

    #[test]
    fn history_page_groups_and_filters() {
        let now = history::now();
        let data = InternalData {
            history: vec![
                history::Entry { url: "https://doc.rust-lang.org/".into(), title: "Rust <docs>".into(), visits: 3, last: now },
                history::Entry { url: "https://github.com/".into(), title: "GitHub".into(), visits: 1, last: now - 86_400 },
            ],
            ..Default::default()
        };
        let html = history_page(&data, "");
        assert!(html.contains("Today") && html.contains("Yesterday"));
        assert!(html.contains("Rust &lt;docs&gt;"));
        assert!(html.contains("forget=github.com"));
        let html = history_page(&data, "rust");
        assert!(!html.contains("GitHub"));
    }

    #[test]
    fn downloads_page_states() {
        let mut data = InternalData::default();
        assert!(downloads_page(&data).contains("No downloads"));
        data.downloads.push(Download { id: 7, url: "https://x/a.zip".into(), path: r"C:\d\a.zip".into(), received: 512 << 10, total: 1 << 20, state: "in_progress".into(), can_resume: false });
        let html = downloads_page(&data);
        assert!(html.contains("a.zip") && html.contains("width:50%") && html.contains("http-equiv=\"refresh\""));
    }
}
