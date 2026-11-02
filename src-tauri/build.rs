//! Puts the pinned, SHA-256-verified scrcpy + adb (and scrcpy-server, icons,
//! licence files) where Tauri bundles them, then runs tauri-build.
//!
//!   binaries/adb-<triple>, binaries/scrcpy-<triple>  -> Contents/MacOS/   (externalBin, signed)
//!   resources/scrcpy-server, resources/scrcpy-icons/  -> Contents/Resources/
//!   resources/licenses/                               -> Contents/Resources/licenses/
//!
//! `PHONEBRIDGE_SKIP_FETCH=1` leaves existing files alone (offline rebuilds).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    if let Err(e) = place_bundled_files() {
        eprintln!("\nphonebridge build.rs: {e}\n");
        std::process::exit(1);
    }
    tauri_build::build();
}

fn place_bundled_files() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../deps.toml");
    println!("cargo:rerun-if-changed=../THIRD_PARTY_NOTICES.md");
    println!("cargo:rerun-if-env-changed=PHONEBRIDGE_SKIP_FETCH");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let target = env::var("TARGET")?;
    let workspace = fetch::workspace_root(&manifest)?;
    let bins = manifest.join("binaries");
    let res = manifest.join("resources");
    let licenses = res.join("licenses");
    fs::create_dir_all(&bins)?;
    fs::create_dir_all(res.join("scrcpy-icons"))?;
    fs::create_dir_all(&licenses)?;

    let adb_out = bins.join(format!("adb-{target}"));
    let scrcpy_out = bins.join(format!("scrcpy-{target}"));

    if env::var_os("PHONEBRIDGE_SKIP_FETCH").is_some() && adb_out.is_file() && scrcpy_out.is_file() {
        return Ok(());
    }

    if !target.contains("apple-darwin") {
        // Linux/other: only for compiling and testing the code, not a real app.
        // Install stubs so tauri-build's externalBin check passes.
        for (path, name) in [(&adb_out, "adb"), (&scrcpy_out, "scrcpy")] {
            if !path.is_file() {
                fs::write(path, format!("#!/bin/sh\necho 'PhoneBridge: {name} is only bundled for macOS builds' >&2\nexit 1\n"))?;
                fetch::install_file(path, path, true)?;
            }
        }
        for f in ["scrcpy-server", "scrcpy-icons/scrcpy.png", "scrcpy-icons/disconnected.png"] {
            let p = res.join(f);
            if !p.exists() {
                fs::write(p, b"")?;
            }
        }
        copy_notices(&workspace, &licenses)?;
        return Ok(());
    }

    let cache = fetch::cache_dir(&workspace);
    let pins = fetch::Pins::load(&workspace.join("deps.toml"))?;

    // scrcpy: the official static build for this architecture.
    let archive = pins
        .scrcpy
        .archives
        .get(&target)
        .ok_or_else(|| format!("deps.toml has no scrcpy build for {target}"))?;
    let file = fetch::fetch(archive, &cache)?;
    let dir = fetch::extract_once(&file, &cache, &format!("scrcpy-{}-{target}", pins.scrcpy.version))?;
    let root = single_subdir(&dir)?;
    fetch::install_file(&root.join("scrcpy"), &scrcpy_out, true)?;
    fetch::install_file(&root.join("scrcpy-server"), &res.join("scrcpy-server"), false)?;
    fetch::install_file(&root.join("scrcpy.png"), &res.join("scrcpy-icons/scrcpy.png"), false)?;
    fetch::install_file(&root.join("disconnected.png"), &res.join("scrcpy-icons/disconnected.png"), false)?;
    fetch::install_file(&root.join("LICENSE"), &licenses.join("scrcpy-LICENSE.txt"), false)?;

    // adb: from Android platform-tools (universal binary).
    let pt = fetch::fetch(&pins.platform_tools_archive(), &cache)?;
    let pt_dir = fetch::extract_once(&pt, &cache, &format!("platform-tools-{}", pins.platform_tools.version))?;
    let pt_root = pt_dir.join("platform-tools");
    fetch::install_file(&pt_root.join("adb"), &adb_out, true)?;
    fetch::install_file(&pt_root.join("NOTICE.txt"), &licenses.join("platform-tools-NOTICE.txt"), false)?;

    copy_notices(&workspace, &licenses)?;
    Ok(())
}

fn copy_notices(workspace: &Path, licenses: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let notices = workspace.join("THIRD_PARTY_NOTICES.md");
    if notices.is_file() {
        fetch::install_file(&notices, &licenses.join("THIRD_PARTY_NOTICES.md"), false)?;
    }
    fetch::install_file(
        &workspace.join("crates/libtailscale-sys/vendor/libtailscale/LICENSE"),
        &licenses.join("libtailscale-LICENSE.txt"),
        false,
    )?;
    // Go module licences gathered by libtailscale-sys.
    if let Some(go) = env::var_os("DEP_TAILSCALE_LICENSES").map(PathBuf::from).filter(|p| p.is_dir()) {
        let dest = licenses.join("go");
        fs::create_dir_all(&dest)?;
        for entry in fs::read_dir(go)?.flatten() {
            if entry.path().is_file() {
                fetch::install_file(&entry.path(), &dest.join(entry.file_name()), false)?;
            }
        }
    }
    Ok(())
}

/// The scrcpy tarball unpacks into one versioned folder.
fn single_subdir(dir: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let subdirs: Vec<PathBuf> =
        fs::read_dir(dir)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    match subdirs.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(format!("unexpected layout in {}", dir.display()).into()),
    }
}
