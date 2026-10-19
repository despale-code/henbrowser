//! Bookmarks (`bookmarks.json` next to the history). Toggled with the star in the address bar or
//! Ctrl+D, listed on the start page, and ranked above history in the address bar suggestions.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
    /// Unix seconds.
    pub added: u64,
    /// Optional folder name (empty = root / unsorted).
    #[serde(default)]
    pub folder: String,
    /// Saved to the Read Later list from Reader mode.
    #[serde(default)]
    pub read_later: bool,
}

pub struct Bookmarks {
    path: PathBuf,
    items: Vec<Bookmark>,
}

impl Bookmarks {
    pub fn load(path: PathBuf) -> Self {
        let items = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, items }
    }

    fn save(&self) {
        if let Ok(s) = serde_json::to_string_pretty(&self.items) {
            if let Err(e) = std::fs::write(&self.path, s) {
                tracing::warn!("couldn't save bookmarks: {e}");
            }
        }
    }

    pub fn all(&self) -> &[Bookmark] {
        &self.items
    }

    pub fn contains(&self, url: &str) -> bool {
        let Some(url) = crate::history::canonical(url) else { return false };
        self.items.iter().any(|b| b.url == url)
    }

    /// Adds the page, or removes it if it's already bookmarked. Returns whether it is now
    /// bookmarked. Pages that can't be bookmarked (internal pages) return false.
    pub fn toggle(&mut self, url: &str, title: &str) -> bool {
        let Some(url) = crate::history::canonical(url) else { return false };
        if let Some(i) = self.items.iter().position(|b| b.url == url) {
            self.items.remove(i);
            self.save();
            return false;
        }
        self.items.push(Bookmark {
            url,
            title: title.to_owned(),
            added: crate::history::now(),
            folder: String::new(),
            read_later: false,
        });
        self.save();
        true
    }

    /// Saves or updates an entry in the Read Later list.
    pub fn add_read_later(&mut self, url: &str, title: &str) -> bool {
        let Some(url) = crate::history::canonical(url) else { return false };
        if let Some(b) = self.items.iter_mut().find(|b| b.url == url) {
            b.read_later = true;
            if b.title.is_empty() && !title.is_empty() {
                b.title = title.to_owned();
            }
        } else {
            self.items.push(Bookmark {
                url,
                title: title.to_owned(),
                added: crate::history::now(),
                folder: "Read Later".to_owned(),
                read_later: true,
            });
        }
        self.save();
        true
    }

    pub fn set_folder(&mut self, url: &str, folder: &str) {
        let Some(url) = crate::history::canonical(url) else { return };
        if let Some(b) = self.items.iter_mut().find(|b| b.url == url) {
            b.folder = folder.trim().to_owned();
            self.save();
        }
    }

    pub fn remove_url(&mut self, url: &str) {
        let canonical = crate::history::canonical(url).unwrap_or_else(|| url.to_owned());
        let len_before = self.items.len();
        self.items.retain(|b| b.url != canonical && b.url != url);
        if self.items.len() != len_before {
            self.save();
        }
    }

    #[allow(dead_code)]
    pub fn folders(&self) -> Vec<String> {
        let mut set = std::collections::BTreeSet::new();
        for b in &self.items {
            if !b.folder.trim().is_empty() {
                set.insert(b.folder.trim().to_owned());
            }
        }
        set.into_iter().collect()
    }

    /// Imports bookmarks from a Netscape/HTML bookmark export (`<H3>Folder</H3>`, `<A HREF="…">Title</A>`).
    /// Returns the number of newly added bookmarks.
    pub fn import_html(&mut self, html: &str) -> usize {
        let mut added_count = 0usize;
        let mut folder_stack: Vec<String> = Vec::new();
        let mut pending_folder: Option<String> = None;

        for raw_line in html.lines() {
            let line = raw_line.trim();
            let upper = line.to_ascii_uppercase();
            if upper.contains("</DL>") {
                folder_stack.pop();
            }
            if let Some(h3_pos) = upper.find("<H3") {
                if let Some(gt) = line[h3_pos..].find('>') {
                    let rest = &line[h3_pos + gt + 1..];
                    if let Some(end) = rest.to_ascii_uppercase().find("</H3>") {
                        let name = rest[..end].trim().to_owned();
                        if !name.is_empty() {
                            pending_folder = Some(name);
                        }
                    }
                }
            }
            if upper.contains("<DL") {
                if let Some(f) = pending_folder.take() {
                    folder_stack.push(f);
                }
            }
            if let Some(a_pos) = upper.find("<A ") {
                let after_a = &line[a_pos..];
                let after_upper = after_a.to_ascii_uppercase();
                if let Some(href_idx) = after_upper.find("HREF=") {
                    let href_rest = &after_a[href_idx + 5..].trim_start();
                    let quote = href_rest.chars().next().unwrap_or('"');
                    if (quote == '"' || quote == '\'') && href_rest.len() > 1 {
                        let inner = &href_rest[1..];
                        if let Some(q_end) = inner.find(quote) {
                            let raw_url = &inner[..q_end];
                            let after_quote = &inner[q_end + 1..];
                            let title = if let Some(gt) = after_quote.find('>') {
                                let t_rest = &after_quote[gt + 1..];
                                let end_a = t_rest.to_ascii_uppercase().find("</A>").unwrap_or(t_rest.len());
                                t_rest[..end_a].trim().to_owned()
                            } else {
                                String::new()
                            };
                            if let Some(url) = crate::history::canonical(raw_url) {
                                if !self.items.iter().any(|b| b.url == url) {
                                    let folder = folder_stack.last().cloned().unwrap_or_default();
                                    self.items.push(Bookmark {
                                        url,
                                        title,
                                        added: crate::history::now(),
                                        folder,
                                        read_later: false,
                                    });
                                    added_count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        if added_count > 0 {
            self.save();
        }
        added_count
    }

    /// Fills in the title of a bookmark saved before its page reported one.
    pub fn set_title(&mut self, url: &str, title: &str) {
        let Some(url) = crate::history::canonical(url) else { return };
        if let Some(b) = self.items.iter_mut().find(|b| b.url == url && b.title.is_empty() && !title.is_empty()) {
            b.title = title.to_owned();
            self.save();
        }
    }

    /// Bookmarks matching what has been typed, best first.
    pub fn query(&self, text: &str, limit: usize) -> Vec<Bookmark> {
        let Some(needle) = crate::history::Needle::new(text) else { return Vec::new() };
        let mut scored: Vec<(u64, &Bookmark)> = self
            .items
            .iter()
            .filter_map(|b| Some((needle.score(&b.url, &b.title)?, b)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().take(limit).map(|(_, b)| b.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_and_query() {
        let dir = std::env::temp_dir().join(format!("henbrowser-bm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bookmarks.json");
        let mut bm = Bookmarks::load(path.clone());
        assert!(bm.toggle("https://doc.rust-lang.org/std/#x", "Rust std"));
        assert!(bm.contains("https://doc.rust-lang.org/std/"));
        assert!(!bm.toggle("about:blank", "nope"));
        assert_eq!(bm.query("rust", 5).len(), 1);
        assert_eq!(bm.query("doc.rust", 5)[0].title, "Rust std");
        // Persisted.
        assert_eq!(Bookmarks::load(path.clone()).all().len(), 1);
        assert!(!bm.toggle("https://doc.rust-lang.org/std/", ""));
        assert!(Bookmarks::load(path.clone()).all().is_empty());

        // HTML import with folders & Read Later
        let sample_html = r#"
        <!DOCTYPE NETSCAPE-Bookmark-file-1>
        <DL><p>
            <DT><H3>Dev Docs</H3>
            <DL><p>
                <DT><A HREF="https://crates.io/">Crates.io</A>
                <DT><A HREF="https://docs.rs/">Docs.rs</A>
            </DL><p>
        </DL><p>
        "#;
        assert_eq!(bm.import_html(sample_html), 2);
        assert_eq!(bm.folders(), vec!["Dev Docs".to_string()]);
        assert!(bm.add_read_later("https://example.com/article", "Long Article"));
        assert!(bm.all().iter().any(|b| b.read_later && b.folder == "Read Later"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
