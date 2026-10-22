//! GitHub repository update checker for Hen Browser.
//!
//! Checks the `despale-code/henbrowser` repository for new releases and main-branch commits.
//! Utilizes GitHub Releases API and the rate-limit-free Git smart HTTP endpoint
//! (`info/refs?service=git-upload-pack`) as fallback.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

pub const REPO_OWNER: &str = "despale-code";
pub const REPO_NAME: &str = "henbrowser";
pub const REPO_URL: &str = "https://github.com/despale-code/henbrowser";
pub const SMART_GIT_URL: &str = "https://github.com/despale-code/henbrowser.git/info/refs?service=git-upload-pack";
pub const RELEASES_API_URL: &str = "https://api.github.com/repos/despale-code/henbrowser/releases/latest";
pub const COMMITS_API_URL: &str = "https://api.github.com/repos/despale-code/henbrowser/commits/main";
pub const UPDATE_SCRIPT_URL: &str = "https://raw.githubusercontent.com/despale-code/chickenwm/main/docs/update.ps1";

const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UpdateInfo {
    pub available: bool,
    pub current_version: String,
    pub current_commit: Option<String>,
    pub latest_version: String,
    pub latest_commit: Option<String>,
    pub release_title: String,
    pub release_notes: String,
    pub published_at: Option<String>,
    pub html_url: String,
}

/// Reads the currently installed commit SHA for henbrowser from ChickenWM's `installed.json`,
/// or from the local `.git` repository if running in a development workspace.
pub fn get_installed_commit() -> Option<String> {
    // 1. Try %LOCALAPPDATA%\Programs\ChickenWM\installed.json
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let path = PathBuf::from(local_app_data)
            .join("Programs")
            .join("ChickenWM")
            .join("installed.json");
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(sha) = val["commits"]["henbrowser"].as_str() {
                    let sha = sha.trim();
                    if sha.len() >= 7 {
                        return Some(sha.to_string());
                    }
                }
            }
        }
    }

    // 2. Try git rev-parse HEAD if running in a dev checkout
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;

    let mut git_cmd = Command::new("git");
    git_cmd.args(["rev-parse", "HEAD"]);
    #[cfg(windows)]
    git_cmd.creation_flags(CREATE_NO_WINDOW);

    if let Ok(out) = git_cmd.output() {
        if out.status.success() {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(sha);
            }
        }
    }

    None
}

/// Parses semantic version parts (major, minor, patch) from strings like "0.1.1", "v0.1.2".
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(|c| c == 'v' || c == 'V');
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").split('-').next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Extracts the 40-character SHA of `refs/heads/main` from Git's smart HTTP `info/refs` response.
pub fn parse_git_refs_main(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(idx) = line.find("refs/heads/main") {
            let before = line[..idx].trim();
            if let Some(token) = before.split_whitespace().last() {
                let s = if token.len() >= 40 {
                    &token[token.len() - 40..]
                } else {
                    token
                };
                if s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(s.to_ascii_lowercase());
                }
            }
        }
    }
    None
}

/// Runs curl.exe with timeout and custom headers to fetch text from a URL.
fn fetch_text(url: &str) -> Result<String, String> {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;

    let mut curl_cmd = Command::new("curl.exe");
    curl_cmd
        .args([
            "-sSL",
            "--fail",
            "--max-time",
            "10",
            "-H",
            "User-Agent: HenBrowser",
            "-H",
            "Accept: application/vnd.github+json",
        ])
        .arg(url);

    #[cfg(windows)]
    curl_cmd.creation_flags(CREATE_NO_WINDOW);

    let out = curl_cmd
        .output()
        .map_err(|e| format!("Failed to execute curl: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "Request failed with exit code {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }

    String::from_utf8(out.stdout).map_err(|e| format!("Invalid UTF-8 response: {e}"))
}

/// Checks the repository for updates.
pub fn check_for_updates() -> Result<UpdateInfo, String> {
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let current_commit = get_installed_commit();

    // 1. Attempt GitHub Releases API
    if let Ok(json_str) = fetch_text(RELEASES_API_URL) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json_str) {
            if let Some(tag) = val["tag_name"].as_str() {
                let remote_ver = tag.trim_start_matches(|c| c == 'v' || c == 'V').to_string();
                let available = match (parse_version(&remote_ver), parse_version(&current_version)) {
                    (Some(r), Some(c)) => r > c,
                    _ => remote_ver != current_version,
                };
                let release_title = val["name"].as_str().unwrap_or(tag).to_string();
                let release_notes = val["body"].as_str().unwrap_or("").to_string();
                let published_at = val["published_at"].as_str().map(|s| s.to_string());
                let html_url = val["html_url"].as_str().unwrap_or(REPO_URL).to_string();

                return Ok(UpdateInfo {
                    available,
                    current_version,
                    current_commit,
                    latest_version: remote_ver,
                    latest_commit: None,
                    release_title,
                    release_notes,
                    published_at,
                    html_url,
                });
            }
        }
    }

    // 2. Fallback: Query GitHub Commits API or Git info/refs
    let mut remote_sha = None;
    let mut commit_title = String::new();
    let mut commit_notes = String::new();
    let mut commit_date = None;
    let mut commit_url = format!("{REPO_URL}/commits/main");

    if let Ok(json_str) = fetch_text(COMMITS_API_URL) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json_str) {
            if let Some(sha) = val["sha"].as_str() {
                remote_sha = Some(sha.to_string());
                let full_msg = val["commit"]["message"].as_str().unwrap_or("").to_string();
                let first_line = full_msg.lines().next().unwrap_or("").to_string();
                commit_title = first_line;
                commit_notes = full_msg;
                commit_date = val["commit"]["author"]["date"].as_str().map(|s| s.to_string());
                if let Some(url) = val["html_url"].as_str() {
                    commit_url = url.to_string();
                }
            }
        }
    }

    // 3. Fallback to smart git endpoint if Commits API hit rate-limits
    if remote_sha.is_none() {
        if let Ok(refs_text) = fetch_text(SMART_GIT_URL) {
            if let Some(sha) = parse_git_refs_main(&refs_text) {
                remote_sha = Some(sha);
                commit_title = "Latest commit on main branch".to_string();
            }
        }
    }

    let Some(sha) = remote_sha else {
        return Err("Could not reach GitHub repository or parse update metadata.".to_string());
    };

    let short_sha = sha[..7.min(sha.len())].to_string();
    let available = if let Some(ref curr) = current_commit {
        !curr.eq_ignore_ascii_case(&sha)
    } else {
        false
    };

    let latest_version = format!("{current_version}+{short_sha}");
    let release_title = if commit_title.is_empty() {
        format!("Hen Browser build {short_sha}")
    } else {
        commit_title
    };

    Ok(UpdateInfo {
        available,
        current_version,
        current_commit,
        latest_version,
        latest_commit: Some(sha),
        release_title,
        release_notes: commit_notes,
        published_at: commit_date,
        html_url: commit_url,
    })
}

/// Spawns the ChickenWM updater in a new visible console window.
pub fn update_now() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass"])
        .env("CHICKENWM_UPDATE", "1")
        .args(["-NoExit", "-Command", &format!("irm '{UPDATE_SCRIPT_URL}' | iex")])
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Couldn't launch updater: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parsing() {
        assert_eq!(parse_version("0.1.1"), Some((0, 1, 1)));
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("V1.0.5-beta"), Some((1, 0, 5)));
        assert_eq!(parse_version("bad"), None);
        assert!(parse_version("v0.2.0") > parse_version("v0.1.1"));
        assert!(parse_version("0.1.2") > parse_version("0.1.1"));
        assert!(parse_version("0.1.1") == parse_version("0.1.1"));
    }

    #[test]
    fn parse_smart_git_refs() {
        let refs = "001e# service=git-upload-pack\n\
                    00000159b9a88fa286fbc17ea1c8366f363574ac55020ee6 HEAD\n\
                    003db9a88fa286fbc17ea1c8366f363574ac55020ee6 refs/heads/main\n\
                    0000";
        assert_eq!(
            parse_git_refs_main(refs),
            Some("b9a88fa286fbc17ea1c8366f363574ac55020ee6".to_string())
        );
    }
}
