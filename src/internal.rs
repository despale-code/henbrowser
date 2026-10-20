//! Built-in pages served by the `chicken` protocol (`http://hen.<page>/`) and the scripts the
//! browser injects into pages: reader mode, picture-in-picture, forced dark mode and the
//! YouTube ad layer.
//!
//! Pages are rendered from a snapshot the browser keeps current (`InternalData`). Actions on a
//! page (forget a site, open a download) are plain links back to the same host with a query;
//! the protocol handler turns them into `UserEvent`s and redirects back to the page.

use crate::history;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
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
        r#"<!doctype html><html><head><meta charset="utf-8">{refresh}<title>{t} — Hen Browser</title>
<style>:root{{--bg-color:#f0e6d3;--surface-color:#fffcf5;--fg-color:#2b1d14;--subtext-color:#6b5545;--border-color:rgba(59,42,31,.28);--hover-bg:rgba(59,42,31,.12);--accent-color:#3b2a1f}}</style>
{theme}<style>{PAGE_CSS}</style></head><body><main>{body}</main></body></html>"#,
        t = html_escape(title)
    )
}

/// History entries as JSON for the page (newest first, with each one's site for "forget site").
pub fn history_json(entries: &[history::Entry]) -> String {
    let mut sorted: Vec<&history::Entry> = entries.iter().collect();
    sorted.sort_by(|a, b| b.last.cmp(&a.last));
    let items: Vec<serde_json::Value> = sorted
        .iter()
        .take(3000)
        .map(|e| {
            serde_json::json!({
                "url": e.url,
                "title": e.title,
                "visits": e.visits,
                "last": e.last,
                "site": crate::settings::site_of(&e.url).unwrap_or_default(),
            })
        })
        .collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".into()).replace("</", r"<\/")
}

/// `http://hen.history/?q=…`: visited pages, newest first, grouped by day. Search, removing a
/// page, forgetting a site and clearing everything happen in the page (no reloads); the browser
/// pushes the updated list back (`hbHistory.render`).
pub fn history_page(data: &InternalData, query: &str) -> String {
    let body = HISTORY_BODY
        .replace("__ENTRIES__", &history_json(&data.history))
        .replace("__QUERY__", &serde_json::to_string(query).unwrap_or_else(|_| "\"\"".into()).replace("</", r"<\/"));
    page("History", &data.theme, &body, None)
}

const HISTORY_BODY: &str = r#"
<style>
  .head { display: flex; align-items: center; gap: 10px; margin-bottom: 8px; }
  .head h1 { margin: 0; flex: 1; }
  .head input { flex: 0 1 300px; }
  button.btn { background: transparent; cursor: pointer; font: inherit; font-size: 12px; }
  .entry { display: flex; align-items: center; gap: 12px; padding: 7px 10px; border-bottom: 1px solid var(--border-color); }
  .entry:hover { background: var(--hover-bg); }
  .entry .time { flex: none; width: 70px; white-space: nowrap; color: var(--subtext-color); font-size: 12px; }
  .entry .info { flex: 1; min-width: 0; }
  .entry a.title { display: block; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; text-decoration: none; font-size: 14px; }
  .entry a.title:hover { text-decoration: underline; }
  .entry .url { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; color: var(--subtext-color); font-size: 12px; }
  .entry .acts { display: flex; gap: 6px; flex: none; opacity: 0; transition: opacity 120ms; }
  .entry:hover .acts, .entry:focus-within .acts { opacity: 1; }
  .count { color: var(--subtext-color); font-size: 12px; margin-bottom: 4px; }
</style>
<div class="head">
  <h1>History</h1>
  <input id="q" placeholder="Search history" spellcheck="false" autofocus>
  <select id="time-range" class="btn" style="border: 1px solid var(--border-color); padding: 4px 8px; border-radius: 4px; background: var(--bg-color); color: var(--fg-color);">
    <option value="hour">Last 1 hour</option>
    <option value="day">Last 24 hours</option>
    <option value="week">Last 7 days</option>
    <option value="all" selected>All time</option>
  </select>
  <button class="btn" id="clear">Clear history</button>
</div>
<div class="count" id="count"></div>
<div id="list"></div>
<script>
  const post = (m) => window.ipc.postMessage(JSON.stringify(m));
  const $ = (id) => document.getElementById(id);
  let entries = __ENTRIES__;
  $("q").value = __QUERY__;

  const startOfDay = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  function dayLabel(secs) {
    const d = new Date(secs * 1000);
    const days = Math.round((startOfDay(new Date()) - startOfDay(d)) / 86400000);
    if (days === 0) return "Today";
    if (days === 1) return "Yesterday";
    return d.toLocaleDateString(undefined, { weekday: "long", day: "numeric", month: "long", year: days > 300 ? "numeric" : undefined });
  }
  const timeOf = (secs) => new Date(secs * 1000).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });

  function button(label, title, onClick) {
    const b = document.createElement("button");
    b.className = "btn";
    b.textContent = label;
    b.title = title;
    b.onclick = (e) => { e.preventDefault(); e.stopPropagation(); onClick(); };
    return b;
  }

  function render() {
    const q = $("q").value.trim().toLowerCase();
    const shown = entries.filter((e) => !q || e.url.toLowerCase().includes(q) || (e.title || "").toLowerCase().includes(q));
    $("count").textContent = shown.length ? `${shown.length} page${shown.length === 1 ? "" : "s"}` : "";
    const list = $("list");
    list.innerHTML = "";
    if (!shown.length) {
      const p = document.createElement("p");
      p.className = "empty";
      p.textContent = q ? "Nothing matches." : "No history yet.";
      list.appendChild(p);
      return;
    }
    let day = "";
    for (const e of shown.slice(0, 1500)) {
      const label = dayLabel(e.last);
      if (label !== day) {
        day = label;
        const h = document.createElement("div");
        h.className = "day";
        h.textContent = label;
        list.appendChild(h);
      }
      const row = document.createElement("div");
      row.className = "entry";
      const time = document.createElement("div");
      time.className = "time";
      time.textContent = timeOf(e.last);
      const info = document.createElement("div");
      info.className = "info";
      const a = document.createElement("a");
      a.className = "title";
      a.href = e.url;
      a.textContent = e.title || e.url;
      a.title = e.title || e.url;
      const url = document.createElement("div");
      url.className = "url";
      url.textContent = e.url + (e.visits > 1 ? `  ·  ${e.visits} visits` : "");
      url.title = e.url;
      info.append(a, url);
      const acts = document.createElement("div");
      acts.className = "acts";
      acts.append(button("Remove", "Remove this page from history", () => {
        entries = entries.filter((x) => x.url !== e.url);
        render();
        post({ cmd: "remove_history_entry", url: e.url });
      }));
      if (e.site) {
        acts.append(button("Forget site", "Remove every page from " + e.site, () => {
          if (!confirm("Remove every page from " + e.site + " from history?")) return;
          post({ cmd: "forget_history_site", site: e.site });
        }));
      }
      row.append(time, info, acts);
      list.appendChild(row);
    }
  }

  window.hbHistory = { render(list) { entries = list; render(); } };
  $("q").addEventListener("input", render);
  $("clear").onclick = () => {
    const range = $("time-range").value;
    const labels = {
      hour: "browsing history from the last hour",
      day: "browsing history from the last 24 hours",
      week: "browsing history from the last 7 days",
      all: "all browsing history"
    };
    if (confirm("Clear " + (labels[range] || "browsing history") + "?")) {
      post({ cmd: "clear_history", range });
    }
  };
  render();
</script>
"#;

/// `http://hen.downloads/`: rendered in the page from data the browser pushes
/// (`hbDownloads.render`), so it updates live without reloading; every action is a message.
pub fn downloads_page(data: &InternalData) -> String {
    let initial = serde_json::json!({
        "downloads": data.downloads.iter().rev().collect::<Vec<_>>(),
        "open_when_done": [],
    });
    let body = DOWNLOADS_BODY.replace("__INITIAL__", &initial.to_string().replace("</", r"<\/"));
    page("Downloads", &data.theme, &body, None)
}

const DOWNLOADS_BODY: &str = r#"
<style>
  .head { display: flex; align-items: center; gap: 10px; margin-bottom: 14px; }
  .head h1 { margin: 0; flex: 1; }
  .head input { flex: 0 1 260px; }
  .dl { display: flex; align-items: center; gap: 12px; padding: 10px; border-bottom: 1px solid var(--border-color); }
  .dl:hover { background: var(--hover-bg); }
  .dl .info { flex: 1; min-width: 0; }
  .dl .name { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; font-size: 14px; }
  .dl.done .name { cursor: pointer; }
  .dl.done .name:hover { text-decoration: underline; }
  .dl .url { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; color: var(--subtext-color); font-size: 12px; margin-top: 2px; }
  .dl .status { color: var(--subtext-color); font-size: 12px; margin-top: 2px; }
  .dl.failed .status { color: #c0392b; }
  .acts { display: flex; gap: 6px; flex-wrap: wrap; justify-content: flex-end; flex: none; max-width: 50%; }
  button.btn { background: transparent; cursor: pointer; font: inherit; font-size: 12px; }
  button.btn.on { background: var(--hover-bg); }
</style>
<div class="head">
  <h1>Downloads</h1>
  <input id="filter" placeholder="Filter downloads" spellcheck="false">
  <button class="btn" id="clear">Clear list</button>
</div>
<div id="list"></div>
<script>
  const post = (m) => window.ipc.postMessage(JSON.stringify(m));
  const $ = (id) => document.getElementById(id);
  const size = (n) => n >= 1 << 30 ? (n / (1 << 30)).toFixed(2) + " GB" : n >= 1 << 20 ? (n / (1 << 20)).toFixed(1) + " MB" : Math.round(n / 1024) + " KB";
  const speed = (n) => n >= 1 << 20 ? (n / (1 << 20)).toFixed(1) + " MB/s" : n >= 1 << 10 ? Math.round(n / 1024) + " KB/s" : Math.round(n) + " B/s";
  const fileName = (d) => (d.path || d.url || "download").split(/[\\/]/).pop();
  const rates = new Map();
  let state = __INITIAL__;

  function rate(d) {
    const now = performance.now(), r = rates.get(d.id);
    if (!r) { rates.set(d.id, { bytes: d.received, t: now, bps: 0 }); return 0; }
    const dt = (now - r.t) / 1000;
    if (dt >= 0.5 && d.received >= r.bytes) {
      const inst = (d.received - r.bytes) / dt;
      r.bps = r.bps ? r.bps * 0.6 + inst * 0.4 : inst;
      r.bytes = d.received; r.t = now;
    }
    return now - r.t < 2500 ? r.bps : 0;
  }

  function status(d, openLater) {
    const progress = d.total ? size(d.received) + " of " + size(d.total) : size(d.received);
    switch (d.state) {
      case "completed": return "Done • " + size(d.received || d.total);
      case "cancelled": return "Cancelled";
      case "interrupted": return d.can_resume ? "Failed • can resume" : "Failed";
      case "paused": return "Paused • " + progress + (openLater ? " • opens when done" : "");
      default: return progress + " • " + speed(rate(d)) + (openLater ? " • opens when done" : "");
    }
  }

  function button(label, msg, extra) {
    const b = document.createElement("button");
    b.className = "btn" + (extra ? " " + extra : "");
    b.textContent = label;
    b.onclick = (e) => { e.stopPropagation(); post(msg); };
    return b;
  }

  function render() {
    const q = $("filter").value.trim().toLowerCase();
    const list = $("list");
    list.innerHTML = "";
    const openLater = new Set(state.open_when_done || []);
    const items = (state.downloads || []).filter((d) => !q || fileName(d).toLowerCase().includes(q) || (d.url || "").toLowerCase().includes(q));
    if (!items.length) {
      const p = document.createElement("p");
      p.className = "empty";
      p.textContent = q ? "Nothing matches." : "No downloads in this session yet.";
      list.appendChild(p);
      return;
    }
    for (const d of items) {
      const running = d.state === "in_progress", paused = d.state === "paused", done = d.state === "completed";
      const failed = d.state === "cancelled" || d.state === "interrupted";
      const row = document.createElement("div");
      row.className = "dl" + (done ? " done" : "") + (failed ? " failed" : "");
      const info = document.createElement("div");
      info.className = "info";
      const name = document.createElement("div");
      name.className = "name";
      name.textContent = fileName(d);
      name.title = d.path || "";
      if (done) name.onclick = () => post({ cmd: "open_download", id: d.id });
      const url = document.createElement("div");
      url.className = "url";
      url.textContent = d.url;
      url.title = d.url;
      const st = document.createElement("div");
      st.className = "status";
      st.textContent = status(d, openLater.has(d.id));
      info.append(name, url, st);
      if (running || paused) {
        const bar = document.createElement("div");
        bar.className = "bar";
        const fill = document.createElement("div");
        fill.style.width = (d.total ? Math.min(100, (d.received / d.total) * 100) : 30) + "%";
        bar.appendChild(fill);
        info.appendChild(bar);
      }
      const acts = document.createElement("div");
      acts.className = "acts";
      if (done) {
        acts.append(button("Open", { cmd: "open_download", id: d.id }), button("Show in folder", { cmd: "show_download", id: d.id }));
      }
      if (running || paused) {
        acts.append(
          button(running ? "Pause" : "Resume", { cmd: "pause_download", id: d.id }),
          button("Open when done", { cmd: "toggle_open_when_done", id: d.id }, openLater.has(d.id) ? "on" : ""),
          button("Cancel", { cmd: "cancel_download", id: d.id }),
        );
      }
      if (failed) acts.append(button(d.state === "interrupted" && d.can_resume ? "Resume" : "Re-download", { cmd: "retry_download", id: d.id }));
      if (done) acts.append(button("Re-download", { cmd: "retry_download", id: d.id }));
      acts.append(button("Copy URL", { cmd: "copy_download_url", id: d.id }));
      if (!running && !paused) acts.append(button("Remove", { cmd: "remove_download", id: d.id }));
      row.append(info, acts);
      list.appendChild(row);
    }
  }

  window.hbDownloads = { render(s) { state = s; render(); } };
  $("filter").addEventListener("input", render);
  $("clear").onclick = () => post({ cmd: "clear_downloads" });
  render();
  // Keeps the speed readout honest between updates.
  setInterval(() => { if ((state.downloads || []).some((d) => d.state === "in_progress")) render(); }, 1000);
</script>
"#;

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
    fn history_page_embeds_entries() {
        let now = history::now();
        let data = InternalData {
            history: vec![
                history::Entry { url: "https://github.com/".into(), title: "GitHub".into(), visits: 1, last: now - 86_400 },
                history::Entry { url: "https://doc.rust-lang.org/".into(), title: "Rust </script> docs".into(), visits: 3, last: now },
            ],
            ..Default::default()
        };
        let json = history_json(&data.history);
        // Newest first, each with its site (for "forget site").
        assert!(json.find("doc.rust-lang.org").unwrap() < json.find("github.com").unwrap());
        assert!(json.contains(r#""site":"github.com""#));
        // A "</script>" in a title can't end the embedded data early.
        let html = history_page(&data, "rust");
        assert!(!html.contains("Rust </script>") && html.contains("hbHistory"));
        assert!(html.contains(r#"$("q").value = "rust";"#));
    }

    #[test]
    fn downloads_page_states() {
        let mut data = InternalData::default();
        assert!(downloads_page(&data).contains("No downloads"));
        data.downloads.push(Download { id: 7, url: "https://x/a.zip".into(), path: r"C:\d\a.zip".into(), received: 512 << 10, total: 1 << 20, state: "in_progress".into(), can_resume: false });
        let html = downloads_page(&data);
        // Rendered by the page from the embedded data; it never reloads itself.
        assert!(html.contains(r#""url":"https://x/a.zip""#) && html.contains("hbDownloads"));
        assert!(!html.contains("http-equiv=\"refresh\""));
        // A "</script>" in a URL can't end the embedded data early.
        data.downloads[0].url = "https://x/</script>.zip".into();
        assert!(!downloads_page(&data).contains("x/</script>"));
    }

    /// Writes sample History and Downloads pages to `$HEN_PREVIEW_DIR` for a visual check.
    #[test]
    #[ignore]
    fn preview_pages() {
        let Some(dir) = std::env::var_os("HEN_PREVIEW_DIR") else { return };
        let now = history::now();
        let mut data = InternalData::default();
        data.history = vec![
            history::Entry { url: "https://doc.rust-lang.org/std/collections/struct.HashMap.html".into(), title: "HashMap in std::collections - Rust".into(), visits: 4, last: now - 600 },
            history::Entry { url: "https://github.com/despale-code/chickenwm".into(), title: "despale-code/chickenwm".into(), visits: 2, last: now - 3_600 },
            history::Entry { url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".into(), title: "A video with a rather long title that goes on and on and on".into(), visits: 1, last: now - 90_000 },
        ];
        data.downloads = vec![
            Download { id: 1, url: "https://example.com/files/a-very-long-file-name-for-testing-the-overflow-behaviour-of-the-list.zip".into(), path: r"C:\Downloads\a-very-long-file-name-for-testing-the-overflow-behaviour-of-the-list.zip".into(), received: 3 << 20, total: 3 << 20, state: "completed".into(), can_resume: false },
            Download { id: 2, url: "https://example.com/big.iso".into(), path: r"C:\Downloads\big.iso".into(), received: 400 << 20, total: 1 << 30, state: "in_progress".into(), can_resume: true },
            Download { id: 3, url: "https://example.com/x.exe".into(), path: r"C:\Downloads\x.exe".into(), received: 1 << 20, total: 5 << 20, state: "cancelled".into(), can_resume: false },
        ];
        let dir = std::path::PathBuf::from(dir);
        std::fs::write(dir.join("history.html"), history_page(&data, "")).unwrap();
        std::fs::write(dir.join("downloads.html"), downloads_page(&data)).unwrap();
    }
}
