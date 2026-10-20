//! Copies `WebView2Loader.dll` next to the henbrowser executable.
//!
//! `webview2-com-sys` links the WebView2 loader dynamically but only copies the DLL into its own
//! build directory, so without this the exe fails to start with STATUS_DLL_NOT_FOUND.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon-res.o");
    // The program icon (assets/icon.ico, precompiled into assets/icon-res.o).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64")
    {
        let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        println!("cargo:rustc-link-arg-bins={}", dir.join("assets").join("icon-res.o").display());
    }
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") | Ok("arm64ec") => "x64",
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        other => panic!("unsupported WebView2 target architecture: {other:?}"),
    };

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("Cargo.toml");
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(&manifest)
        .output()
        .expect("failed to run `cargo metadata`");
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("invalid `cargo metadata` output");
    let sys_dir = metadata["packages"]
        .as_array()
        .and_then(|pkgs| pkgs.iter().find(|p| p["name"] == "webview2-com-sys"))
        .and_then(|p| p["manifest_path"].as_str())
        .map(|p| PathBuf::from(p).parent().unwrap().to_path_buf())
        .expect("webview2-com-sys not found in dependency graph");
    let dll = sys_dir.join(arch).join("WebView2Loader.dll");

    // OUT_DIR is target/<profile>/build/henbrowser-<hash>/out; the exe lives in target/<profile>.
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let profile_dir = out_dir.ancestors().nth(3).expect("unexpected OUT_DIR layout");
    let dest = profile_dir.join("WebView2Loader.dll");
    if !dest.exists() {
        std::fs::copy(&dll, &dest).unwrap_or_else(|e| panic!("failed to copy {}: {e}", dll.display()));
    } else {
        let _ = std::fs::copy(&dll, &dest);
    }
}
