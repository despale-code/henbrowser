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

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Fragment-less URL for matching/dedup, or `None` for pages that shouldn't be remembered.
fn canonical(url: &str) -> Option<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
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

    /// Best matches for what has been typed so far.
    pub fn query(&self, text: &str, limit: usize) -> Vec<Entry> {
        let needle = text.trim().to_lowercase();
        let needle = needle.split_once("://").map_or(needle.as_str(), |(_, r)| r);
        let needle = needle.strip_prefix("www.").unwrap_or(needle);
        if needle.is_empty() {
            return Vec::new();
        }
        let words: Vec<&str> = needle.split_whitespace().collect();
        let t = now();
        let mut scored: Vec<(u64, &Entry)> = self
            .entries
            .iter()
            .filter_map(|e| {
                let key = bare(&e.url);
                let title = e.title.to_lowercase();
                let base = if key.starts_with(needle) {
                    1000
                } else if key.contains(needle) {
                    600
                } else if words.iter().all(|w| key.contains(w) || title.contains(w)) {
                    300
                } else {
                    return None;
                };
                let recency = 100u64.saturating_sub(t.saturating_sub(e.last) / 86_400);
                // Shorter URLs first among equals: the site's front page before a deep link.
                let short = 50u64.saturating_sub(key.len() as u64 / 4);
                Some((base + u64::from(e.visits.min(30)) * 5 + recency + short, e))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().take(limit).map(|(_, e)| e.clone()).collect()
    }
}
