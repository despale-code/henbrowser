//! Installing extensions into `~/.config/chickenwm/henbrowser/extensions/`: from a Chrome
//! Web Store page or id (the `.crx` is downloaded by the browser itself), a `.crx` or `.zip`
//! file, or an unpacked folder.
//!
//! A `.crx` carries the store's public key. It is written into the unpacked manifest as `key`, so
//! the extension installs under its real store id, which web services (sign-in pages using
//! `externally_connectable`, for example) address it by. A copy unpacked without `key` would get
//! an id derived from its folder path instead.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

/// Chromium's extension id for a public key: the first 16 bytes of its SHA-256, each nibble
/// written as a letter `a`..`p`.
pub fn id_from_key(der: &[u8]) -> String {
    id_from_hash(&Sha256::digest(der)[..16])
}

fn id_from_hash(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|b| [b >> 4, b & 0xf])
        .map(|n| (b'a' + n) as char)
        .collect()
}

/// The id a manifest's `key` pins the extension to.
pub fn manifest_id(manifest: &serde_json::Value) -> Option<String> {
    let key: String = manifest["key"].as_str()?.chars().filter(|c| !c.is_whitespace()).collect();
    Some(id_from_key(&base64_decode(&key)?))
}

pub fn is_extension_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

/// The extension id in a Chrome Web Store detail URL
/// (`chromewebstore.google.com/detail/<slug>/<id>` or the old `chrome.google.com/webstore/...`).
pub fn store_id_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (host, path) = rest.split_once('/')?;
    let store = host.eq_ignore_ascii_case("chromewebstore.google.com")
        || (host.eq_ignore_ascii_case("chrome.google.com") && path.starts_with("webstore/"));
    if !store {
        return None;
    }
    path.split(['/', '?', '#']).find(|seg| is_extension_id(seg)).map(str::to_owned)
}

/// Where Chrome itself fetches an extension's `.crx` (it redirects to the file).
pub fn crx_url(id: &str) -> String {
    format!(
        "https://clients2.google.com/service/update2/crx?response=redirect&prodversion=131.0.0.0\
         &acceptformat=crx2,crx3&x=id%3D{id}%26uc"
    )
}

/// What the user typed or picked: a Web Store URL or id, or a `.crx` / `.zip` / folder path.
/// Fetches (store) and unpacks it off the UI thread; `done` gets the installed folder.
pub fn install_input_async(input: String, ext_root: PathBuf, done: impl FnOnce(Result<PathBuf, String>) + Send + 'static) {
    std::thread::spawn(move || {
        let input = input.trim().trim_matches('"').to_owned();
        let id = store_id_from_url(&input).or_else(|| is_extension_id(&input).then(|| input.clone()));
        let result = match id {
            Some(id) => download_crx(&id, &ext_root).and_then(|bytes| install_bytes(&bytes, &ext_root)),
            None if input.is_empty() => Err(anyhow::anyhow!("paste a Chrome Web Store link, an extension id or a file path")),
            None => install_path(Path::new(&input), &ext_root),
        };
        done(result.map_err(|e| format!("{e:#}")));
    });
}

/// Fetches the store's `.crx` for `id` (WinINet follows the redirect to the package).
fn download_crx(id: &str, ext_root: &Path) -> Result<Vec<u8>> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::Urlmon::URLDownloadToFileW;
    let tmp = ext_root.join(format!(".download-{id}.crx"));
    let hr = unsafe { URLDownloadToFileW(None, &HSTRING::from(crx_url(id)), &HSTRING::from(tmp.as_os_str()), 0, None) };
    hr.ok().with_context(|| format!("couldn't download extension {id} from the Chrome Web Store"))?;
    let bytes = std::fs::read(&tmp);
    let _ = std::fs::remove_file(&tmp);
    let bytes = bytes?;
    if !bytes.starts_with(b"Cr24") {
        bail!("the Chrome Web Store didn't return an extension package for {id}");
    }
    Ok(bytes)
}

/// Installs a `.crx`, a `.zip` or an unpacked folder (its manifest may sit one folder down) into
/// `ext_root`, replacing an earlier copy of the same extension. Returns the installed folder.
pub fn install_path(src: &Path, ext_root: &Path) -> Result<PathBuf> {
    if src.is_dir() {
        let dir = find_manifest_dir(src).context("no manifest.json in that folder")?;
        let manifest = read_manifest(&dir)?;
        let dest = ext_root.join(folder_name(&manifest, None));
        if same_dir(&dir, &dest) {
            return Ok(dest);
        }
        replace_dir(&dest)?;
        copy_dir(&dir, &dest)?;
        return Ok(dest);
    }
    let bytes = std::fs::read(src).with_context(|| format!("couldn't read {}", src.display()))?;
    install_bytes(&bytes, ext_root)
}

/// Installs a `.crx` or `.zip` given as bytes.
pub fn install_bytes(bytes: &[u8], ext_root: &Path) -> Result<PathBuf> {
    let (key, zip) = if bytes.starts_with(b"Cr24") { parse_crx(bytes)? } else { (None, bytes) };
    let staging = ext_root.join(".installing");
    let _ = std::fs::remove_dir_all(&staging);
    extract_zip(zip, &staging)?;
    let result = (|| {
        let dir = find_manifest_dir(&staging).context("the package has no manifest.json")?;
        let mut manifest = read_manifest(&dir)?;
        if let Some(key) = &key {
            manifest["key"] = serde_json::Value::String(crate::extensions::base64(key));
            std::fs::write(dir.join("manifest.json"), serde_json::to_string_pretty(&manifest)?)?;
        }
        let dest = ext_root.join(folder_name(&manifest, key.as_deref()));
        replace_dir(&dest)?;
        std::fs::rename(&dir, &dest).or_else(|_| copy_dir(&dir, &dest))?;
        Ok(dest)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Startup tidy-up of the extensions folder, so dropping things into it by hand works: `.crx` and
/// `.zip` files are unpacked (and the file removed), and a folder whose manifest sits one level
/// down (an extracted release archive) is moved up.
pub fn normalize_root(ext_root: &Path) {
    let Ok(entries) = std::fs::read_dir(ext_root) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        if path.is_file() && (ext == "crx" || ext == "zip") {
            match install_path(&path, ext_root) {
                Ok(dir) => {
                    tracing::info!("unpacked {} into {}", path.display(), dir.display());
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => tracing::warn!("couldn't unpack {}: {e:#}", path.display()),
            }
        } else if path.is_dir() && !path.join("manifest.json").exists() {
            if path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                continue;
            }
            if let Some(inner) = find_manifest_dir(&path) {
                let tmp = ext_root.join(".flatten");
                let _ = std::fs::remove_dir_all(&tmp);
                if std::fs::rename(&inner, &tmp).is_ok() {
                    let _ = std::fs::remove_dir_all(&path);
                    let _ = std::fs::rename(&tmp, &path);
                    tracing::info!("moved {} up to {}", inner.display(), path.display());
                }
            }
        }
    }
}

fn read_manifest(dir: &Path) -> Result<serde_json::Value> {
    let text = std::fs::read_to_string(dir.join("manifest.json"))?;
    Ok(serde_json::from_str(text.trim_start_matches('\u{feff}'))?)
}

/// The folder holding `manifest.json`: `dir` itself or a folder at most two levels down.
fn find_manifest_dir(dir: &Path) -> Option<PathBuf> {
    if dir.join("manifest.json").is_file() {
        return Some(dir.to_path_buf());
    }
    let subdirs = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .map(|it| it.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
            .unwrap_or_default()
    };
    for sub in subdirs(dir) {
        if sub.join("manifest.json").is_file() {
            return Some(sub);
        }
        if let Some(deeper) = subdirs(&sub).into_iter().find(|d| d.join("manifest.json").is_file()) {
            return Some(deeper);
        }
    }
    None
}

/// The store id when the key gives one, else the extension's name made filename-safe.
fn folder_name(manifest: &serde_json::Value, key: Option<&[u8]>) -> String {
    if let Some(id) = key.map(id_from_key).or_else(|| manifest_id(manifest)) {
        return id;
    }
    let name: String = manifest["name"]
        .as_str()
        .unwrap_or("extension")
        .trim_start_matches("__MSG_")
        .trim_end_matches("__")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let name = name.trim_matches('-').to_owned();
    if name.is_empty() { "extension".to_owned() } else { name }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn replace_dir(dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).with_context(|| format!("couldn't replace {}", dest.display()))?;
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("not a valid zip/crx")?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else { continue };
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut data)?;
        std::fs::write(&out, data)?;
    }
    Ok(())
}

/// Splits a `.crx` (version 2 or 3) into its public key and zip payload.
fn parse_crx(bytes: &[u8]) -> Result<(Option<Vec<u8>>, &[u8])> {
    let u32_at = |at: usize| -> Result<usize> {
        let b = bytes.get(at..at + 4).context("truncated crx")?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    match u32_at(4)? {
        2 => {
            let (key_len, sig_len) = (u32_at(8)?, u32_at(12)?);
            let key = bytes.get(16..16 + key_len).context("truncated crx")?.to_vec();
            let zip = bytes.get(16 + key_len + sig_len..).context("truncated crx")?;
            Ok((Some(key), zip))
        }
        3 => {
            let header_len = u32_at(8)?;
            let header = bytes.get(12..12 + header_len).context("truncated crx")?;
            let zip = bytes.get(12 + header_len..).context("truncated crx")?;
            Ok((crx3_key(header), zip))
        }
        v => bail!("unsupported crx version {v}"),
    }
}

/// The key in a CRX3 header (`CrxFileHeader` protobuf) whose hash is the signed crx id.
fn crx3_key(header: &[u8]) -> Option<Vec<u8>> {
    let mut keys = Vec::new();
    let mut crx_id = None;
    for (field, value) in proto_fields(header) {
        match field {
            // sha256_with_rsa / sha256_with_ecdsa: AsymmetricKeyProof { 1: public_key }
            2 | 3 => {
                if let Some((_, key)) = proto_fields(value).into_iter().find(|(f, _)| *f == 1) {
                    keys.push(key.to_vec());
                }
            }
            // signed_header_data: SignedData { 1: crx_id }
            10000 => {
                crx_id = proto_fields(value).into_iter().find(|(f, _)| *f == 1).map(|(_, id)| id.to_vec());
            }
            _ => {}
        }
    }
    let wanted = crx_id.map(|id| id_from_hash(&id));
    keys.iter()
        .find(|k| wanted.as_deref() == Some(id_from_key(k).as_str()))
        .or(keys.first())
        .cloned()
}

/// Length-delimited fields of a protobuf message (other wire types are skipped).
fn proto_fields(mut buf: &[u8]) -> Vec<(u64, &[u8])> {
    fn varint(buf: &mut &[u8]) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = buf.split_first()?;
            *buf = rest;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    let mut out = Vec::new();
    while !buf.is_empty() {
        let Some(tag) = varint(&mut buf) else { break };
        let skip = match tag & 7 {
            0 => {
                if varint(&mut buf).is_none() {
                    break;
                }
                0
            }
            1 => 8,
            5 => 4,
            2 => {
                let Some(len) = varint(&mut buf).map(|l| l as usize) else { break };
                let Some(value) = buf.get(..len) else { break };
                out.push((tag >> 3, value));
                len
            }
            _ => break,
        };
        let Some(rest) = buf.get(skip..) else { break };
        buf = rest;
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Proton Pass's published store key and id (any store extension would do).
    const KEY: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAiZt8pKnvrDkvOuKzjfj1TtrPc0lwlUc/pvAOAoLmtQ9i5IdVisA7m4zqLKFONapyI3FWPsThpGRteYpLSgIf0LY8E2TGCHRDobYMLagS7OAbVySapvYCqj4LBQ7EOSYNVOEOUgyTAWqDB4udB6vgQXkRV6f2n2hAsAvrkutW1U4Unovg1zksp24bOTwyOxM5rJ6vbqGVzuhiblqaOpinmN+TQ53+8xCmVduWrQCMO3I+/hOPmw6S9nPMFj1Wsc+qIsNMCLT8kEGRGa51gw7ZK7TaYQ1MGB/O+lbUjcL5bjYYvcYXA7bL3Ol8vhSLN5yhhxox9w19O/+bwoyib4OAHQIDAQAB";

    #[test]
    fn key_gives_store_id() {
        let manifest = serde_json::json!({ "key": KEY });
        assert_eq!(manifest_id(&manifest).as_deref(), Some("ghmbeldphafepmbegfdlkpapadhbakde"));
        assert_eq!(crate::extensions::base64(&base64_decode(KEY).unwrap()), KEY);
    }

    #[test]
    fn store_urls() {
        let id = "ghmbeldphafepmbegfdlkpapadhbakde";
        assert_eq!(store_id_from_url(&format!("https://chromewebstore.google.com/detail/proton-pass/{id}?hl=en")).as_deref(), Some(id));
        assert_eq!(store_id_from_url(&format!("https://chrome.google.com/webstore/detail/x/{id}")).as_deref(), Some(id));
        assert_eq!(store_id_from_url(&format!("https://example.com/detail/x/{id}")), None);
        assert!(crx_url(id).contains(id));
    }

    #[test]
    fn crx3_round_trip() {
        // A CRX3 with one RSA proof whose key hashes to the signed id, wrapping a tiny zip.
        let key = base64_decode(KEY).unwrap();
        let hash = Sha256::digest(&key);
        let field = |n: u64, v: &[u8]| -> Vec<u8> {
            let mut out = Vec::new();
            let mut tag = (n << 3) | 2;
            while tag >= 0x80 {
                out.push((tag as u8) | 0x80);
                tag >>= 7;
            }
            out.push(tag as u8);
            out.push(v.len() as u8 | if v.len() >= 0x80 { 0x80 } else { 0 });
            if v.len() >= 0x80 {
                out.push((v.len() >> 7) as u8);
            }
            out.extend_from_slice(v);
            out
        };
        let proof = field(1, &key);
        let mut header = field(2, &proof);
        header.extend(field(10000, &field(1, &hash[..16])));

        let mut zip_bytes = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut zip_bytes));
            w.start_file("manifest.json", zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut w, br#"{"name":"Test","manifest_version":3,"version":"1"}"#).unwrap();
            w.finish().unwrap();
        }
        let mut crx = b"Cr24".to_vec();
        crx.extend(3u32.to_le_bytes());
        crx.extend((header.len() as u32).to_le_bytes());
        crx.extend(&header);
        crx.extend(&zip_bytes);

        let root = std::env::temp_dir().join(format!("henbrowser-ext-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let dir = install_bytes(&crx, &root).unwrap();
        assert_eq!(dir.file_name().unwrap(), "ghmbeldphafepmbegfdlkpapadhbakde");
        let manifest = read_manifest(&dir).unwrap();
        assert_eq!(manifest_id(&manifest).as_deref(), Some("ghmbeldphafepmbegfdlkpapadhbakde"));
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod online {
    /// Fetches and unpacks a real store extension (network; run with `--ignored`).
    #[test]
    #[ignore]
    fn store_download() {
        let id = "ghmbeldphafepmbegfdlkpapadhbakde";
        let root = std::env::temp_dir().join("henbrowser-store-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let bytes = super::download_crx(id, &root).unwrap();
        let dir = super::install_bytes(&bytes, &root).unwrap();
        assert_eq!(dir.file_name().unwrap(), id);
        assert!(dir.join("manifest.json").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }
}
