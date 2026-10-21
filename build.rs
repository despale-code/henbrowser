//! Links the program icon (`assets/icon.ico`, precompiled into `assets/icon-res.o`) into the exe,
//! so Explorer, the Start Menu and the taskbar show it, and ensures WebView2Loader.dll is copied.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon-res.o");
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    let windows_x64 = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64");
    if windows_x64 {
        let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let src = dir.join("assets").join("icon-res.o");
        if src.exists() {
            let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
            let dest = out_dir.join("icon-res.o");
            let _ = std::fs::copy(&src, &dest);
            println!("cargo:rustc-link-arg-bins={}", dest.display());
        }

        // Copy WebView2Loader.dll from webview2-com-sys build out dir to target directory
        if let Ok(out_dir_str) = std::env::var("OUT_DIR") {
            let out_dir = std::path::PathBuf::from(out_dir_str);
            // OUT_DIR is target/(debug|release)/build/henbrowser-<hash>/out
            if let Some(build_dir) = out_dir.parent().and_then(|p| p.parent()) {
                if let Some(target_dir) = build_dir.parent() {
                    copy_webview2_loader(build_dir, target_dir);
                }
            }
        }
    }
}

fn copy_webview2_loader(build_dir: &std::path::Path, target_dir: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(build_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("webview2-com-sys-") {
                        let dll = path.join("out").join("x64").join("WebView2Loader.dll");
                        if dll.exists() {
                            let dest = target_dir.join("WebView2Loader.dll");
                            let _ = std::fs::copy(&dll, &dest);
                            return;
                        }
                    }
                }
            }
        }
    }
}
