//! Gives wmx.exe Windows version information (FileDescription, ProductName, version), so Windows
//! names it in Firewall and Defender dialogs and Task Manager instead of showing "wmx.exe".
//!
//! The names come from the build that ships wmx: Ghostex sets `WMX_FILE_DESCRIPTION=Ghostex
//! Sessions`, `WMX_PRODUCT_NAME=Ghostex` and `WMX_PRODUCT_VERSION` (its own version) in
//! apps/desktop/scripts/build-windows-app.ps1 and build-windows-app-wsl.sh, so no app name is
//! written here. Without them wmx describes itself.

#[path = "build_windows_version.rs"]
mod windows_version;

use std::{env, fs, path::PathBuf};

fn main() {
    for name in ["WMX_FILE_DESCRIPTION", "WMX_PRODUCT_NAME", "WMX_PRODUCT_VERSION"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let read = |name: &str, default: String| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or(default)
    };
    let description = read(
        "WMX_FILE_DESCRIPTION",
        "wmx persistent terminal sessions".to_string(),
    );
    let product = read("WMX_PRODUCT_NAME", "wmx".to_string());
    let version = read(
        "WMX_PRODUCT_VERSION",
        env::var("CARGO_PKG_VERSION").unwrap_or_default(),
    );
    let info = windows_version::VersionInfo {
        file_description: &description,
        product_name: &product,
        company_name: &product,
        internal_name: "wmx",
        original_filename: "wmx.exe",
        version_text: &version,
        version: windows_version::numeric_version(&version),
    };
    let path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("wmx-version.res");
    fs::write(&path, windows_version::version_resource_file(&info))
        .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    println!("cargo:rustc-link-arg-bin=wmx={}", path.display());
}
