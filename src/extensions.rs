//! Browser extensions (Proton Pass, ...).
//!
//! Each subfolder of `~/.config/chickenwm/chickenbrowser/extensions/` is an unpacked Chromium
//! extension that WebView2 installs into the browser profile. WebView2 runs extensions (service
//! workers, content scripts) but has no toolbar UI, so chickenbrowser draws the buttons itself and
//! opens each extension's `action.default_popup` page in a panel.

use crate::UserEvent;
use serde::Serialize;
use std::path::{Path, PathBuf};
use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    take_pwstr, BrowserExtensionRemoveCompletedHandler, ProfileGetBrowserExtensionsCompletedHandler,
};
use windows_core::{Interface, PWSTR};

/// Proton Pass's Chrome Web Store id.
pub const PROTON_PASS_ID: &str = "ghmbeldphafepmbegfdlkpapadhbakde";

/// Store public keys (manifest `key`) for extensions whose web services address them by their
/// store id. An unpacked copy without `key` gets a path-derived id instead, so e.g.
/// account.proton.me can never hand the sign-in session back to Proton Pass
/// (`externally_connectable` messages go to `PROTON_PASS_ID`). The key is the public half of the
/// store signing key, taken from the published CRX header.
const STORE_KEYS: &[(&str, &str, &str)] = &[(
    "Proton Pass",
    PROTON_PASS_ID,
    "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAiZt8pKnvrDkvOuKzjfj1TtrPc0lwlUc/pvAOAoLmtQ9i5IdVisA7m4zqLKFONapyI3FWPsThpGRteYpLSgIf0LY8E2TGCHRDobYMLagS7OAbVySapvYCqj4LBQ7EOSYNVOEOUgyTAWqDB4udB6vgQXkRV6f2n2hAsAvrkutW1U4Unovg1zksp24bOTwyOxM5rJ6vbqGVzuhiblqaOpinmN+TQ53+8xCmVduWrQCMO3I+/hOPmw6S9nPMFj1Wsc+qIsNMCLT8kEGRGa51gw7ZK7TaYQ1MGB/O+lbUjcL5bjYYvcYXA7bL3Ol8vhSLN5yhhxox9w19O/+bwoyib4OAHQIDAQAB",
)];

fn read_manifest(dir: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

/// Gives known store extensions their store `key` so they install under their real id. Must run
/// before the extensions folder is handed to WebView2. Returns how many manifests were patched.
pub fn ensure_store_keys(ext_root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(ext_root) else {
        return 0;
    };
    let mut patched = 0;
    for dir in entries.flatten().map(|e| e.path()) {
        let Some(mut manifest) = read_manifest(&dir) else {
            continue;
        };
        if manifest.get("key").is_some() {
            continue;
        }
        let raw = manifest["name"].as_str().unwrap_or_default().to_owned();
        let mut names = vec![raw.clone()];
        names.extend(localized(&dir, &manifest, &raw));
        let Some((_, id, key)) = STORE_KEYS
            .iter()
            .find(|(name, _, _)| names.iter().any(|n| n.contains(name)))
        else {
            continue;
        };
        manifest["key"] = serde_json::Value::String((*key).to_owned());
        match serde_json::to_string_pretty(&manifest)
            .map_err(std::io::Error::other)
            .and_then(|text| std::fs::write(dir.join("manifest.json"), text))
        {
            Ok(()) => {
                tracing::info!("set store key on {} (id {id})", dir.display());
                patched += 1;
            }
            Err(e) => tracing::warn!("could not write store key to {}: {e}", dir.display()),
        }
    }
    patched
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtensionInfo {
    pub id: String,
    pub name: String,
    /// Popup page path inside the extension (`action.default_popup`).
    #[serde(skip)]
    pub popup: Option<String>,
    /// Toolbar icon as a `data:` URI.
    pub icon: Option<String>,
}

struct ManifestInfo {
    dir: PathBuf,
    names: Vec<String>,
    popup: Option<String>,
    icon: Option<String>,
    /// Id the extension must install under, when its manifest carries a known store key.
    store_id: Option<&'static str>,
}

/// Maps installed extension id -> install path, from the profile's `Secure Preferences`.
///
/// WebView2 also reports Edge's built-in component extensions (Microsoft Clipboard, Edge PDF
/// Viewer, ...) which live under the WebView2 runtime folder; only ids whose path is one of our
/// extension folders are ours.
fn installed_paths(profile: &Path) -> Vec<(String, PathBuf)> {
    let file = profile.join("EBWebView").join("Default").join("Secure Preferences");
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let Ok(prefs) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    prefs["extensions"]["settings"]
        .as_object()
        .map(|settings| {
            settings
                .iter()
                .filter_map(|(id, s)| Some((id.clone(), PathBuf::from(s["path"].as_str()?))))
                .collect()
        })
        .unwrap_or_default()
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase();
    norm(a) == norm(b)
}

/// Resolves `__MSG_key__` names via `_locales/<default_locale or en>/messages.json`.
fn localized(dir: &Path, manifest: &serde_json::Value, value: &str) -> Option<String> {
    let key = value.strip_prefix("__MSG_")?.strip_suffix("__")?;
    let locale = manifest["default_locale"].as_str().unwrap_or("en");
    let text = std::fs::read_to_string(dir.join("_locales").join(locale).join("messages.json")).ok()?;
    let messages: serde_json::Value = serde_json::from_str(&text).ok()?;
    messages
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v["message"].as_str().map(str::to_owned))
}

/// Picks the largest icon up to 64px from `action.default_icon` or `icons`.
fn icon_data_uri(dir: &Path, manifest: &serde_json::Value) -> Option<String> {
    let icons = manifest["action"]["default_icon"]
        .as_object()
        .or_else(|| manifest["icons"].as_object())?;
    let (_, rel) = icons
        .iter()
        .filter_map(|(size, path)| Some((size.parse::<u32>().ok()?, path.as_str()?)))
        .filter(|(size, _)| *size <= 64)
        .max_by_key(|(size, _)| *size)?;
    let bytes = std::fs::read(dir.join(rel.trim_start_matches('/'))).ok()?;
    let mime = if rel.ends_with(".svg") { "image/svg+xml" } else { "image/png" };
    Some(format!("data:{mime};base64,{}", base64(&bytes)))
}

fn read_manifests(ext_root: &Path) -> Vec<ManifestInfo> {
    let Ok(entries) = std::fs::read_dir(ext_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let manifest = read_manifest(&dir)?;
            let raw = manifest["name"].as_str().unwrap_or_default().to_owned();
            let mut names = vec![raw.clone()];
            names.extend(localized(&dir, &manifest, &raw));
            let store_id = manifest["key"].as_str().and_then(|k| {
                STORE_KEYS.iter().find(|(_, _, key)| *key == k).map(|(_, id, _)| *id)
            });
            Some(ManifestInfo {
                names,
                store_id,
                popup: manifest["action"]["default_popup"]
                    .as_str()
                    .map(|p| p.trim_start_matches('/').to_owned()),
                icon: icon_data_uri(&dir, &manifest),
                dir,
            })
        })
        .collect()
}

/// Asks the profile which extensions are installed, then reports ours (with popup/icon from
/// their manifests) as `UserEvent::Extensions`. Extensions that don't come from `ext_root`
/// (Edge's built-in components) are left out.
pub fn query_installed(
    controller: &ICoreWebView2Controller,
    ext_root: PathBuf,
    profile_dir: PathBuf,
    proxy: EventLoopProxy<UserEvent>,
) -> windows_core::Result<()> {
    unsafe {
        let profile = controller
            .CoreWebView2()?
            .cast::<ICoreWebView2_13>()?
            .Profile()?
            .cast::<ICoreWebView2Profile7>()?;
        profile.GetBrowserExtensions(&ProfileGetBrowserExtensionsCompletedHandler::create(
            Box::new(move |result, list| {
                result?;
                let Some(list) = list else {
                    return Ok(());
                };
                let manifests = read_manifests(&ext_root);
                let paths = installed_paths(&profile_dir);
                let mut count = 0u32;
                list.Count(&mut count)?;
                let mut out: Vec<ExtensionInfo> = Vec::with_capacity(count as usize);
                for i in 0..count {
                    let ext = list.GetValueAtIndex(i)?;
                    let id = {
                        let mut p = PWSTR::null();
                        ext.Id(&mut p)?;
                        take_pwstr(p)
                    };
                    let name = {
                        let mut p = PWSTR::null();
                        ext.Name(&mut p)?;
                        take_pwstr(p)
                    };
                    let by_path = paths
                        .iter()
                        .find(|(pid, _)| *pid == id)
                        .and_then(|(_, path)| manifests.iter().find(|m| same_path(&m.dir, path)));
                    let Some(manifest) =
                        by_path.or_else(|| manifests.iter().find(|m| m.names.iter().any(|n| n == &name)))
                    else {
                        tracing::debug!("ignoring non-chickenbrowser extension {name} ({id})");
                        continue;
                    };
                    // A copy installed before it had its store key: same folder, old path-derived
                    // id. It would show a duplicate button and hold the old (unreachable) id.
                    if manifest.store_id.is_some_and(|sid| sid != id) {
                        tracing::info!("removing stale install of {name} ({id})");
                        let _ = ext.Remove(&BrowserExtensionRemoveCompletedHandler::create(
                            Box::new(|_| Ok(())),
                        ));
                        continue;
                    }
                    let mut is_enabled = windows_core::BOOL(0);
                    let _ = ext.IsEnabled(&mut is_enabled);
                    if is_enabled.0 == 0 {
                        tracing::warn!("extension {name} ({id}) was disabled; enabling now");
                        let _ = ext.Enable(true, None);
                    }
                    if out.iter().any(|e| e.id == id) {
                        continue;
                    }
                    out.push(ExtensionInfo {
                        id,
                        name,
                        popup: manifest.popup.clone(),
                        icon: manifest.icon.clone(),
                    });
                }
                let _ = proxy.send_event(UserEvent::Extensions(out));
                Ok(())
            }),
        ))?;
    }
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
