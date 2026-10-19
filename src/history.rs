//! Visited pages, for the address bar's suggestions. Stored as JSON next to the other browser
//! data; only http(s) pages are kept.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ENTRIES: usize = 5000;

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    pub url: String,
    pub title: String,
    pub visits: u32,
    /// Unix seconds of the last visit.
    pub last: u64,
}

pub struct History {
    path: PathBuf,
    entries: Vec<Entry>,
    dirty: bool,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Fragment-less URL for matching/dedup, or `None` for pages that shouldn't be remembered
/// (anything but http(s), and the built-in start page).
pub fn canonical(url: &str) -> Option<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) || url.starts_with("http://hen.") {
        return None;
    }
    Some(url.split('#').next().unwrap_or(url).to_owned())
}

/// URL without scheme and `www.`, lowercased: what typed text is matched against.
fn bare(url: &str) -> String {
    let u = url.split_once("://").map_or(url, |(_, rest)| rest);
    u.strip_prefix("www.").unwrap_or(u).trim_end_matches('/').to_lowercase()
}

impl History {
    pub fn load(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, entries, dirty: false }
    }

    pub fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        if let Ok(s) = serde_json::to_string(&self.entries) {
            if std::fs::write(&self.path, s).is_ok() {
                self.dirty = false;
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.dirty = true;
        self.save_if_dirty();
    }

    pub fn clear_since(&mut self, cutoff: u64) {
        let before = self.entries.len();
        self.entries.retain(|e| e.last < cutoff);
        if self.entries.len() != before {
            self.dirty = true;
            self.save_if_dirty();
        }
    }

    pub fn clear_range(&mut self, range: Option<&str>) {
        match range {
            Some("hour") => self.clear_since(now().saturating_sub(3600)),
            Some("day") => self.clear_since(now().saturating_sub(86400)),
            Some("week") => self.clear_since(now().saturating_sub(7 * 86400)),
            _ => self.clear(),
        }
    }

    pub fn visit(&mut self, url: &str) {
        let Some(url) = canonical(url) else { return };
        self.dirty = true;
        if let Some(e) = self.entries.iter_mut().find(|e| e.url == url) {
            e.visits += 1;
            e.last = now();
            return;
        }
        self.entries.push(Entry { url, title: String::new(), visits: 1, last: now() });
        if self.entries.len() > MAX_ENTRIES {
            // Drop the stalest entry.
            if let Some(i) = self.entries.iter().enumerate().min_by_key(|(_, e)| e.last).map(|(i, _)| i) {
                self.entries.swap_remove(i);
            }
        }
    }

    pub fn set_title(&mut self, url: &str, title: &str) {
        let Some(url) = canonical(url) else { return };
        if let Some(e) = self.entries.iter_mut().find(|e| e.url == url) {
            if e.title != title {
                e.title = title.to_owned();
                self.dirty = true;
            }
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Removes one page from the history.
    pub fn remove(&mut self, url: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.url != url);
        let removed = self.entries.len() != before;
        if removed {
            self.dirty = true;
            self.save_if_dirty();
        }
        removed
    }

    /// Removes every page of `site` (see `settings::site_of`) from the history.
    pub fn forget_site(&mut self, site: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| !crate::settings::url_on_site(&e.url, site));
        let removed = before - self.entries.len();
        if removed > 0 {
            self.dirty = true;
            self.save_if_dirty();
        }
        removed
    }

    /// Best matches for what has been typed so far.
    pub fn query(&self, text: &str, limit: usize) -> Vec<Entry> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return self.top_sites(limit);
        }
        let Some(needle) = Needle::new(trimmed) else { return Vec::new() };
        let t = now();
        let mut scored: Vec<(u64, &Entry)> = self
            .entries
            .iter()
            .filter_map(|e| {
                let base = needle.score(&e.url, &e.title)?;
                let frec = u64::from(chickenwm_core::scoring::frecency_weigh(e.visits, e.last, t)) / 2;
                Some((base + frec, e))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().take(limit).map(|(_, e)| e.clone()).collect()
    }

    /// Most visited sites for the start page: one entry per host (its most visited page),
    /// weighted toward recent use.
    pub fn top_sites(&self, limit: usize) -> Vec<Entry> {
        let t = now();
        let weight = |e: &Entry| u64::from(chickenwm_core::scoring::frecency_weigh(e.visits, e.last, t));
        let mut by_host: Vec<(String, u64, &Entry)> = Vec::new();
        for e in &self.entries {
            let host = bare(&e.url).split('/').next().unwrap_or_default().to_owned();
            let w = weight(e);
            match by_host.iter_mut().find(|(h, _, _)| *h == host) {
                Some(slot) => {
                    // The host's score adds up; its link is its most visited page.
                    if weight(slot.2) < w {
                        slot.2 = e;
                    }
                    slot.1 += w;
                }
                None => by_host.push((host, w, e)),
            }
        }
        by_host.sort_by(|a, b| b.1.cmp(&a.1));
        by_host.into_iter().take(limit).map(|(_, _, e)| e.clone()).collect()
    }
}

/// Typed address-bar text, normalised for matching against URLs and titles.
pub struct Needle {
    text: String,
}

impl Needle {
    pub fn new(text: &str) -> Option<Self> {
        let text = text.trim().to_lowercase();
        let text = text.split_once("://").map_or(text.as_str(), |(_, r)| r);
        let text = text.strip_prefix("www.").unwrap_or(text).to_owned();
        (!text.is_empty()).then_some(Self { text })
    }

    /// Match quality for one page, `None` if it doesn't match. Prefix matches on the address
    /// rank highest; shorter addresses win among equals (the front page before a deep link),
    /// with fallback to shared `fuzzy_score` for non-contiguous word-boundary queries.
    pub fn score(&self, url: &str, title: &str) -> Option<u64> {
        let key = bare(url);
        let title = title.to_lowercase();
        let base = if key.starts_with(&self.text) {
            1000
        } else if key.contains(&self.text) {
            600
        } else if self.text.split_whitespace().all(|w| key.contains(w) || title.contains(w)) {
            300
        } else if self.text.chars().count() >= 3 {
            let fz = chickenwm_core::scoring::fuzzy_score(&self.text, &key)
                .max(chickenwm_core::scoring::fuzzy_score(&self.text, &title))?;
            if fz >= 30 {
                (fz as u64).min(250)
            } else {
                return None;
            }
        } else {
            return None;
        };
        let short = 50u64.saturating_sub(key.len() as u64 / 4);
        Some(base + short)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(entries: &[(&str, &str, u32, u64)]) -> History {
        History {
            path: std::env::temp_dir().join("henbrowser-history-test.json"),
            entries: entries.iter().map(|(u, t, v, l)| Entry { url: u.to_string(), title: t.to_string(), visits: *v, last: *l }).collect(),
            dirty: false,
        }
    }

    #[test]
    fn ranking() {
        let now = now();
        let h = history(&[
            ("https://github.com/rust-lang/rust/issues/1", "Issue 1", 1, now - 40 * 86_400),
            ("https://github.com/", "GitHub", 20, now),
            ("https://docs.github.com/en", "GitHub Docs", 2, now),
            ("https://example.com/github-tips", "Tips", 9, now),
        ]);
        let urls: Vec<String> = h.query("git", 10).into_iter().map(|e| e.url).collect();
        // Prefix matches on the address first, the front page before deep links.
        assert_eq!(urls[0], "https://github.com/");
        assert_eq!(urls[1], "https://github.com/rust-lang/rust/issues/1");
        // Words can match the title.
        assert_eq!(h.query("tips", 10)[0].url, "https://example.com/github-tips");
        assert!(h.query("www.", 10).is_empty() || h.query("   ", 10).is_empty());
        assert!(h.query("zzz", 10).is_empty());
    }

    #[test]
    fn visits_and_forgetting() {
        let mut h = history(&[]);
        h.visit("https://a.example.com/page#frag");
        h.visit("https://a.example.com/page");
        h.visit("http://hen.history/");
        h.visit("about:blank");
        assert_eq!(h.entries().len(), 1);
        assert_eq!(h.entries()[0].visits, 2);
        h.visit("https://b.org/");
        assert_eq!(h.forget_site("example.com"), 1);
        assert_eq!(h.entries().len(), 1);
        let _ = std::fs::remove_file(&h.path);
    }

    #[test]
    fn top_sites_one_per_host() {
        let now = now();
        let h = history(&[
            ("https://news.ycombinator.com/", "HN", 30, now),
            ("https://news.ycombinator.com/item?id=1", "Thread", 2, now),
            ("https://rust-lang.org/", "Rust", 5, now),
        ]);
        let top: Vec<String> = h.top_sites(8).into_iter().map(|e| e.url).collect();
        assert_eq!(top, ["https://news.ycombinator.com/", "https://rust-lang.org/"]);
    }

    #[test]
    fn clear_time_ranges() {
        let now = now();
        let mut h = history(&[
            ("https://example.com/recent", "Recent", 1, now - 300), // 5 mins ago
            ("https://example.com/earlier-today", "Today", 1, now - 7200), // 2 hours ago
            ("https://example.com/this-week", "Week", 1, now - 100_000), // ~1.1 days ago
            ("https://example.com/ancient", "Ancient", 1, now - 1_000_000), // ~11 days ago
        ]);
        assert_eq!(h.entries().len(), 4);

        // Clear last 1 hour: drops "recent", keeps 3
        h.clear_range(Some("hour"));
        assert_eq!(h.entries().len(), 3);
        assert!(!h.entries().iter().any(|e| e.url.ends_with("/recent")));

        // Clear last 24 hours: drops "earlier-today", keeps 2
        h.clear_range(Some("day"));
        assert_eq!(h.entries().len(), 2);
        assert!(!h.entries().iter().any(|e| e.url.ends_with("/earlier-today")));

        // Clear last 7 days: drops "this-week", keeps "ancient"
        h.clear_range(Some("week"));
        assert_eq!(h.entries().len(), 1);
        assert_eq!(h.entries()[0].url, "https://example.com/ancient");

        // Clear all
        h.clear_range(Some("all"));
        assert_eq!(h.entries().len(), 0);
    }
}
