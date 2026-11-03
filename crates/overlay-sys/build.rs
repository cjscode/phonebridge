//! Builds `liboverlay.a` from `go/` (PhoneBridge's C wrapper around NetBird's
//! `client/embed` and Tailscale's `tsnet`) with `go build -buildmode=c-archive`.
//! Both live in one archive because a program can only contain one Go runtime.
//!
//! Go is a *build-time only* dependency, and you don't install it: the toolchain
//! pinned in `deps.toml` is downloaded (SHA-256 checked) into the build cache.
//! Go modules are verified against `go/go.sum`.
//!
//! Overrides:
//! - `PHONEBRIDGE_OVERLAY_A=/path/liboverlay.a`  link a prebuilt archive, skip Go entirely
//! - `PHONEBRIDGE_GO=/path/to/go`                use this `go` binary instead of downloading one
//! - `PHONEBRIDGE_MACOS_MIN=13.0`                deployment target (Go 1.27 needs >= 13.0)

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if let Err(e) = run() {
        // cargo shows build-script stderr on failure; make the cause obvious.
        eprintln!("\noverlay-sys build failed:\n{e}\n");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let target = env::var("TARGET")?;
    let host = env::var("HOST")?;
    let vendor = manifest.join("go");

    for f in [
        "common.go",
        "netbird.go",
        "tailscale.go",
        "go.mod",
        "go.sum",
    ] {
        println!("cargo:rerun-if-changed={}", vendor.join(f).display());
    }
    for v in [
        "PHONEBRIDGE_OVERLAY_A",
        "PHONEBRIDGE_GO",
        "PHONEBRIDGE_MACOS_MIN",
        "PHONEBRIDGE_CACHE",
    ] {
        println!("cargo:rerun-if-env-changed={v}");
    }

    let (goos, goarch) = go_platform(&target)?;
    let is_darwin = goos == "darwin";

    if let Some(prebuilt) = env::var_os("PHONEBRIDGE_OVERLAY_A") {
        let prebuilt = PathBuf::from(prebuilt);
        fs::copy(&prebuilt, out_dir.join("liboverlay.a"))
            .map_err(|e| format!("copying {}: {e}", prebuilt.display()))?;
    } else {
        let workspace = fetch::workspace_root(&manifest)?;
        let cache = fetch::cache_dir(&workspace);
        let pins = fetch::Pins::load(&workspace.join("deps.toml"))?;
        let go = go_binary(&pins, &host, &cache)?;
        build_archive(&go, &vendor, &out_dir, &cache, goos, goarch, is_darwin)?;
        collect_licenses(&go, &vendor, &cache, goos, goarch);
        // Exposed to dependents' build scripts as DEP_OVERLAY_LICENSES.
        println!(
            "cargo:licenses={}",
            cache.join("licenses").join("go").display()
        );
    }

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=overlay");
    if is_darwin {
        // From the `go:cgo_import_dynamic` directives in the darwin dependency
        // tree: CoreFoundation and Security (crypto/x509 etc.) and libresolv.
        for fw in ["CoreFoundation", "Security"] {
            println!("cargo:rustc-link-lib=framework={fw}");
        }
        println!("cargo:rustc-link-lib=resolv");
    } else {
        println!("cargo:rustc-link-lib=pthread");
        println!("cargo:rustc-link-lib=dl");
        println!("cargo:rustc-link-lib=resolv");
    }
    Ok(())
}

/// Rust target triple -> (GOOS, GOARCH).
fn go_platform(target: &str) -> Result<(&'static str, &'static str), String> {
    let goos = if target.contains("apple-darwin") {
        "darwin"
    } else if target.contains("linux") {
        "linux"
    } else {
        return Err(format!("unsupported target {target} (macOS or Linux only)"));
    };
    let goarch = if target.starts_with("aarch64") {
        "arm64"
    } else if target.starts_with("x86_64") {
        "amd64"
    } else {
        return Err(format!("unsupported architecture in {target}"));
    };
    Ok((goos, goarch))
}

fn go_binary(
    pins: &fetch::Pins,
    host: &str,
    cache: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(go) = env::var_os("PHONEBRIDGE_GO") {
        return Ok(PathBuf::from(go));
    }
    let (os, arch) = go_platform(host)?;
    let key = format!("{os}-{arch}");
    let archive = pins
        .go
        .archives
        .get(&key)
        .ok_or_else(|| format!("deps.toml has no Go archive for build host {key}"))?;
    let file = fetch::fetch(archive, cache)?;
    let dir = fetch::extract_once(&file, cache, &format!("go-{}-{key}", pins.go.version))?;
    let go = dir.join("go").join("bin").join("go");
    if !go.is_file() {
        return Err(format!("Go archive did not contain {}", go.display()).into());
    }
    Ok(go)
}

fn go_env(
    cmd: &mut Command,
    cache: &Path,
    goos: &str,
    goarch: &str,
    is_darwin: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    cmd.env("GOTOOLCHAIN", "local")
        .env("GOFLAGS", "-mod=readonly")
        .env("GOPATH", cache.join("gopath"))
        .env("GOMODCACHE", cache.join("gomodcache"))
        .env("GOCACHE", cache.join("gocache"))
        .env("CGO_ENABLED", "1")
        .env("GOOS", goos)
        .env("GOARCH", goarch)
        .env_remove("GOROOT");

    if is_darwin {
        let min = env::var("PHONEBRIDGE_MACOS_MIN").unwrap_or_else(|_| "13.0".into());
        let clang = xcrun_clang()?;
        // Calling clang by its full path (instead of the /usr/bin/clang shim)
        // skips xcrun's SDK lookup, so the system headers (stdlib.h, pthread.h)
        // aren't found. Pass the SDK explicitly.
        let sdk = macos_sdk()?;
        let arch = if goarch == "arm64" { "arm64" } else { "x86_64" };
        let flags = format!("-arch {arch} -mmacosx-version-min={min} -isysroot {sdk}");
        cmd.env("CC", clang)
            .env("SDKROOT", &sdk)
            .env("CGO_CFLAGS", format!("-O2 {flags}"))
            .env("CGO_LDFLAGS", &flags)
            .env("MACOSX_DEPLOYMENT_TARGET", &min);
    }
    Ok(())
}

/// Path of the macOS SDK (`SDKROOT` if set, else `xcrun --show-sdk-path`).
fn macos_sdk() -> Result<String, Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=SDKROOT");
    if let Ok(sdk) = env::var("SDKROOT")
        && Path::new(&sdk).is_dir()
    {
        return Ok(sdk);
    }
    let out = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .map_err(|e| format!("running xcrun --show-sdk-path: {e}"))?;
    let sdk = String::from_utf8(out.stdout)?.trim().to_string();
    if !out.status.success() || sdk.is_empty() || !Path::new(&sdk).is_dir() {
        return Err(format!(
            "Couldn't find the macOS SDK. Reinstall the Xcode Command Line Tools:\n  \
             sudo rm -rf /Library/Developer/CommandLineTools && xcode-select --install\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    if sdk.contains(' ') {
        // CGO_CFLAGS is split on spaces; a quoted path isn't supported there.
        return Err(format!(
            "macOS SDK path contains a space, which cgo flags can't handle: {sdk}"
        )
        .into());
    }
    Ok(sdk)
}

fn xcrun_clang() -> Result<String, Box<dyn std::error::Error>> {
    let out = Command::new("xcrun")
        .args(["--sdk", "macosx", "--find", "clang"])
        .output()
        .map_err(|e| {
            format!(
                "running xcrun (install Xcode Command Line Tools: `xcode-select --install`): {e}"
            )
        })?;
    if !out.status.success() {
        return Err(format!(
            "xcrun could not find clang. Install the Xcode Command Line Tools with `xcode-select --install`.\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

fn build_archive(
    go: &Path,
    vendor: &Path,
    out_dir: &Path,
    cache: &Path,
    goos: &str,
    goarch: &str,
    is_darwin: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::new(go);
    cmd.current_dir(vendor)
        .args([
            "build",
            "-buildmode=c-archive",
            "-trimpath",
            "-ldflags=-s -w -buildid=",
        ])
        .arg("-o")
        .arg(out_dir.join("liboverlay.a"))
        .arg(".");
    go_env(&mut cmd, cache, goos, goarch, is_darwin)?;
    let out = cmd
        .output()
        .map_err(|e| format!("running {}: {e}", go.display()))?;
    if !out.status.success() {
        return Err(format!(
            "`go build -buildmode=c-archive` failed ({}):\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(())
}

/// Copy LICENSE files of the Go standard library and every Go module linked into
/// liboverlay to `<cache>/licenses/go/`, which the app bundles. Best effort:
/// a failure here warns instead of failing the build.
fn collect_licenses(go: &Path, vendor: &Path, cache: &Path, goos: &str, goarch: &str) {
    let dest = cache.join("licenses").join("go");
    let result = (|| -> Result<usize, Box<dyn std::error::Error>> {
        let mut cmd = Command::new(go);
        cmd.current_dir(vendor).args([
            "list",
            "-deps",
            "-f",
            "{{with .Module}}{{.Path}}\t{{.Version}}\t{{.Dir}}{{end}}",
            ".",
        ]);
        // Licences don't depend on cgo flags; skip the compiler lookup.
        go_env(&mut cmd, cache, goos, goarch, false)?;
        cmd.env("CGO_ENABLED", "1");
        let out = cmd.output()?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned().into());
        }
        let mut modules = BTreeMap::new();
        for line in String::from_utf8(out.stdout)?.lines() {
            let mut parts = line.split('\t');
            if let (Some(path), Some(ver), Some(dir)) = (parts.next(), parts.next(), parts.next())
                && !dir.is_empty()
            {
                modules.insert(path.to_string(), (ver.to_string(), PathBuf::from(dir)));
            }
        }
        if dest.exists() {
            fs::remove_dir_all(&dest)?;
        }
        fs::create_dir_all(&dest)?;

        // Go itself.
        let goroot = go.parent().and_then(Path::parent).map(Path::to_path_buf);
        if let Some(lic) = goroot.map(|r| r.join("LICENSE")).filter(|p| p.is_file()) {
            fs::copy(lic, dest.join("go-stdlib.LICENSE.txt"))?;
        }

        let mut index = String::from(
            "Go modules statically linked into PhoneBridge (via NetBird and Tailscale):\n\n",
        );
        for (path, (ver, dir)) in &modules {
            let ver = if ver.is_empty() {
                "(main module)"
            } else {
                ver.as_str()
            };
            index.push_str(&format!("{path} {ver}\n"));
            let safe = path.replace('/', "_");
            for entry in fs::read_dir(dir)?.flatten() {
                if !entry.path().is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let upper = name.to_ascii_uppercase();
                if upper.starts_with("LICENSE")
                    || upper.starts_with("COPYING")
                    || upper.starts_with("NOTICE")
                    || upper == "PATENTS"
                {
                    fs::copy(entry.path(), dest.join(format!("{safe}.{name}.txt")))?;
                }
            }
        }
        fs::write(dest.join("MODULES.txt"), index)?;
        Ok(modules.len())
    })();
    if let Err(e) = result {
        println!("cargo:warning=could not collect Go module licenses: {e}");
    }
}
