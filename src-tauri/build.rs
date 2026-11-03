//! Puts the pinned, SHA-256-verified scrcpy + adb (and scrcpy-server, icons,
//! licence files) where Tauri bundles them, then runs tauri-build.
//! See `crates/fetch/src/bundle.rs` for the layout.
//!
//! `PHONEBRIDGE_SKIP_FETCH=1` leaves existing files alone (offline rebuilds).

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=../deps.toml");
    println!("cargo:rerun-if-changed=../THIRD_PARTY_NOTICES.md");
    println!("cargo:rerun-if-env-changed=PHONEBRIDGE_SKIP_FETCH");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let target = env::var("TARGET").unwrap();
    let skip = env::var_os("PHONEBRIDGE_SKIP_FETCH").is_some()
        && manifest.join(format!("binaries/adb-{target}")).is_file()
        && manifest.join(format!("binaries/scrcpy-{target}")).is_file();
    if !skip {
        let go_licenses = env::var_os("DEP_OVERLAY_LICENSES").map(PathBuf::from);
        let result = fetch::workspace_root(&manifest)
            .and_then(|ws| fetch::bundle::place(&ws, &manifest, &target, go_licenses.as_deref()));
        if let Err(e) = result {
            eprintln!("\nphonebridge build.rs: {e}\n");
            std::process::exit(1);
        }
    }
    tauri_build::build();
}
