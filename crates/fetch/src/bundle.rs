//! Placing the pinned binaries where Tauri bundles them. Shared by
//! `src-tauri/build.rs` and `cargo xtask fetch`.
//!
//!   src-tauri/binaries/adb-<triple>, scrcpy-<triple>  -> Contents/MacOS/ (externalBin, signed)
//!   src-tauri/resources/scrcpy-server, scrcpy-icons/   -> Contents/Resources/
//!   src-tauri/resources/licenses/                      -> Contents/Resources/licenses/

use std::fs;
use std::path::{Path, PathBuf};

use crate::{Pins, Result, cache_dir, extract_once, fetch, install_file};

pub struct Placed {
    pub adb: PathBuf,
    pub scrcpy: PathBuf,
    pub scrcpy_server: PathBuf,
}

/// Download (cached, verified) and install scrcpy + adb for `target` into `tauri_dir`.
/// Non-macOS targets get stub executables so the code can be compiled and tested.
pub fn place(
    workspace: &Path,
    tauri_dir: &Path,
    target: &str,
    go_licenses: Option<&Path>,
) -> Result<Placed> {
    let bins = tauri_dir.join("binaries");
    let res = tauri_dir.join("resources");
    let licenses = res.join("licenses");
    fs::create_dir_all(&bins)?;
    fs::create_dir_all(res.join("scrcpy-icons"))?;
    fs::create_dir_all(&licenses)?;
    let placed = Placed {
        adb: bins.join(format!("adb-{target}")),
        scrcpy: bins.join(format!("scrcpy-{target}")),
        scrcpy_server: res.join("scrcpy-server"),
    };

    if !target.contains("apple-darwin") {
        for (path, name) in [(&placed.adb, "adb"), (&placed.scrcpy, "scrcpy")] {
            if !path.is_file() {
                fs::write(
                    path,
                    format!(
                        "#!/bin/sh\necho 'PhoneBridge: {name} is only bundled for macOS builds' >&2\nexit 1\n"
                    ),
                )?;
                install_file(path, path, true)?;
            }
        }
        for f in [
            "scrcpy-server",
            "scrcpy-icons/scrcpy.png",
            "scrcpy-icons/disconnected.png",
        ] {
            let p = res.join(f);
            if !p.exists() {
                fs::write(p, b"")?;
            }
        }
        copy_notices(workspace, &licenses, go_licenses)?;
        return Ok(placed);
    }

    let cache = cache_dir(workspace);
    let pins = Pins::load(&workspace.join("deps.toml"))?;

    // scrcpy: the official static build for this architecture.
    let archive = pins
        .scrcpy
        .archives
        .get(target)
        .ok_or_else(|| crate::Error(format!("deps.toml has no scrcpy build for {target}")))?;
    let file = fetch(archive, &cache)?;
    let dir = extract_once(
        &file,
        &cache,
        &format!("scrcpy-{}-{target}", pins.scrcpy.version),
    )?;
    let root = single_subdir(&dir)?;
    install_file(&root.join("scrcpy"), &placed.scrcpy, true)?;
    install_file(&root.join("scrcpy-server"), &placed.scrcpy_server, false)?;
    install_file(
        &root.join("scrcpy.png"),
        &res.join("scrcpy-icons/scrcpy.png"),
        false,
    )?;
    install_file(
        &root.join("disconnected.png"),
        &res.join("scrcpy-icons/disconnected.png"),
        false,
    )?;
    install_file(
        &root.join("LICENSE"),
        &licenses.join("scrcpy-LICENSE.txt"),
        false,
    )?;

    // adb: from Android platform-tools (a universal binary).
    let pt = fetch(&pins.platform_tools_archive(), &cache)?;
    let pt_dir = extract_once(
        &pt,
        &cache,
        &format!("platform-tools-{}", pins.platform_tools.version),
    )?;
    let pt_root = pt_dir.join("platform-tools");
    install_file(&pt_root.join("adb"), &placed.adb, true)?;
    install_file(
        &pt_root.join("NOTICE.txt"),
        &licenses.join("platform-tools-NOTICE.txt"),
        false,
    )?;

    copy_notices(workspace, &licenses, go_licenses)?;
    Ok(placed)
}

fn copy_notices(workspace: &Path, licenses: &Path, go_licenses: Option<&Path>) -> Result<()> {
    let notices = workspace.join("THIRD_PARTY_NOTICES.md");
    if notices.is_file() {
        install_file(&notices, &licenses.join("THIRD_PARTY_NOTICES.md"), false)?;
    }
    // Left by builds from the libtailscale era (Tailscale is now in the Go module list).
    let _ = fs::remove_file(licenses.join("libtailscale-LICENSE.txt"));
    if let Some(go) = go_licenses.filter(|p| p.is_dir()) {
        let dest = licenses.join("go");
        fs::create_dir_all(&dest)?;
        let mut wanted = std::collections::HashSet::new();
        for entry in fs::read_dir(go)?.flatten() {
            if entry.path().is_file() {
                install_file(&entry.path(), &dest.join(entry.file_name()), false)?;
                wanted.insert(entry.file_name());
            }
        }
        // Keep the folder an exact copy: drop licences of modules no longer linked.
        for entry in fs::read_dir(&dest)?.flatten() {
            if !wanted.contains(&entry.file_name()) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// The scrcpy tarball unpacks into one versioned folder.
fn single_subdir(dir: &Path) -> Result<PathBuf> {
    let subdirs: Vec<PathBuf> = fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    match subdirs.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(crate::Error(format!(
            "unexpected layout in {}",
            dir.display()
        ))),
    }
}
