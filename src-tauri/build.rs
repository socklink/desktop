use std::path::Path;
use std::process::Command;

fn ensure_resources() {
    let dll_path = Path::new("resources/obs-virtualcam-module64.dll");
    let is_valid = dll_path.metadata().map(|m| m.len() > 1000).unwrap_or(false);
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // On non-Windows platforms (e.g. Linux), the DLL is not needed at runtime.
    // An empty placeholder satisfies tauri-build's bundle resource check without downloading 140MB.
    if target_os != "windows" {
        if !dll_path.exists() {
            if let Some(parent) = dll_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(dll_path, b"");
        }
        return;
    }

    if is_valid {
        return;
    }

    println!("cargo:warning=[socklink] obs-virtualcam-module64.dll is missing. Attempting automatic setup...");

    // 1. Try invoking setup-resources.js via node
    if let Ok(status) = Command::new("node").arg("../scripts/setup-resources.js").status() {
        if status.success() && dll_path.metadata().map(|m| m.len() > 1000).unwrap_or(false) {
            return;
        }
    }

    // 2. Fallback: Direct download via curl and extraction via tar (built-in on Windows 10/11)
    if let Some(parent) = dll_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let url = "https://github.com/obsproject/obs-studio/releases/download/30.1.2/OBS-Studio-30.1.2.zip";
    let temp_zip = "resources/obs-temp.zip";
    let dll_zip_path = "data/obs-plugins/win-dshow/obs-virtualcam-module64.dll";

    println!("cargo:warning=[socklink] Downloading OBS Studio to extract virtualcam module...");
    let curl_status = Command::new("curl")
        .args(&["-L", url, "-o", temp_zip])
        .status();

    if let Ok(status) = curl_status {
        if status.success() {
            let _ = Command::new("tar")
                .args(&["-xf", "obs-temp.zip", dll_zip_path])
                .current_dir("resources")
                .status();

            let extracted = Path::new("resources/data/obs-plugins/win-dshow/obs-virtualcam-module64.dll");
            if extracted.exists() {
                let _ = std::fs::rename(extracted, dll_path);
                println!("cargo:warning=[socklink] Successfully installed obs-virtualcam-module64.dll!");
            }
            let _ = std::fs::remove_dir_all("resources/data");
        }
    }
    let _ = std::fs::remove_file(temp_zip);
}

fn main() {
    ensure_resources();
    tauri_build::build();
}
