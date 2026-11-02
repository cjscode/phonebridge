//! `cargo xtask <command>`
//!
//!   doctor          check the Mac has what the build needs (Xcode CLT, Rust target, tauri-cli)
//!   fetch           download + verify scrcpy/adb into src-tauri/ (also done by the build)
//!   dist            fetch, `cargo tauri build --bundles app,dmg`, then verify-bundle
//!   verify-bundle   check the built .app: files, exec bits, architectures, code signatures

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const TARGET: &str = "aarch64-apple-darwin";
const APP: &str = "PhoneBridge.app";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let target = flag(&args, "--target").unwrap_or_else(|| TARGET.to_string());
    let result = match args.first().map(String::as_str) {
        Some("doctor") => doctor(),
        Some("fetch") => fetch_cmd(&target),
        Some("dist") => dist(&target),
        Some("verify-bundle") => verify_bundle(
            args.get(1)
                .filter(|a| !a.starts_with("--"))
                .map(PathBuf::from),
            &target,
        ),
        _ => {
            eprintln!(
                "usage: cargo xtask <doctor|fetch|dist|verify-bundle [path/to/PhoneBridge.app]> [--target TRIPLE]"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\nerror: {e}");
            ExitCode::FAILURE
        }
    }
}

type R = Result<(), String>;

fn flag(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

fn workspace() -> PathBuf {
    fetch::workspace_root(Path::new(env!("CARGO_MANIFEST_DIR"))).expect("workspace root")
}

fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("running {cmd:?}: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("{cmd:?} failed:\n{text}"))
    }
}

fn doctor() -> R {
    let mut ok = true;
    let mut check = |name: &str, res: Result<String, String>, fix: &str| match res {
        Ok(v) => println!("  ok   {name}: {}", v.lines().next().unwrap_or("").trim()),
        Err(_) => {
            ok = false;
            println!("  FAIL {name}\n       fix: {fix}");
        }
    };
    println!("PhoneBridge build doctor");
    check(
        "Xcode Command Line Tools",
        run(Command::new("xcode-select").arg("-p")),
        "xcode-select --install",
    );
    check(
        "clang",
        run(Command::new("xcrun").args(["--find", "clang"])),
        "xcode-select --install",
    );
    check(
        "macOS SDK",
        run(Command::new("xcrun").args(["--sdk", "macosx", "--show-sdk-path"])),
        "sudo rm -rf /Library/Developer/CommandLineTools && xcode-select --install",
    );
    check(
        "curl",
        run(Command::new("curl").arg("--version")),
        "curl ships with macOS",
    );
    check(
        "rustc",
        run(Command::new("rustc").arg("--version")),
        "install rustup from https://rustup.rs",
    );
    check(
        "Rust target aarch64-apple-darwin",
        run(Command::new("rustup").args(["target", "list", "--installed"])).and_then(|s| {
            if s.contains(TARGET) {
                Ok(TARGET.into())
            } else {
                Err(s)
            }
        }),
        "rustup target add aarch64-apple-darwin (rust-toolchain.toml does this automatically)",
    );
    check(
        "tauri-cli",
        run(Command::new("cargo").args(["tauri", "--version"])),
        "cargo install tauri-cli --version 2.12.4 --locked",
    );
    for (name, path) in [
        ("Homebrew adb", "/opt/homebrew/bin/adb"),
        ("Homebrew scrcpy", "/opt/homebrew/bin/scrcpy"),
    ] {
        if Path::new(path).exists() {
            println!("  note {name} at {path} (PhoneBridge never uses it)");
        }
    }
    if ok {
        Ok(())
    } else {
        Err("fix the items marked FAIL, then run again".into())
    }
}

fn fetch_cmd(target: &str) -> R {
    let ws = workspace();
    let go_licenses = fetch::cache_dir(&ws).join("licenses").join("go");
    let placed = fetch::bundle::place(&ws, &ws.join("src-tauri"), target, Some(&go_licenses))
        .map_err(|e| e.to_string())?;
    for p in [&placed.adb, &placed.scrcpy, &placed.scrcpy_server] {
        let sha = fetch::sha256_file(p).map_err(|e| e.to_string())?;
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        println!(
            "{:>10} B  sha256 {sha}  {}",
            size,
            p.strip_prefix(&ws).unwrap_or(p).display()
        );
    }
    Ok(())
}

fn dist(target: &str) -> R {
    if !cfg!(target_os = "macos") {
        return Err("dist builds the macOS app and must run on a Mac".into());
    }
    doctor()?;
    fetch_cmd(target)?;
    let ws = workspace();
    println!("\n== cargo tauri build ==");
    let status = Command::new("cargo")
        .current_dir(ws.join("src-tauri"))
        .args(["tauri", "build", "--target", target, "--bundles", "app,dmg"])
        .status()
        .map_err(|e| format!("running cargo tauri build: {e}"))?;
    if !status.success() {
        return Err(format!("cargo tauri build failed ({status})"));
    }
    verify_bundle(None, target)?;
    let bundle = ws.join("target").join(target).join("release/bundle");
    println!(
        "\nBuilt:\n  {}\n  {}",
        bundle.join("macos").join(APP).display(),
        bundle.join("dmg").display()
    );
    Ok(())
}

fn verify_bundle(app: Option<PathBuf>, target: &str) -> R {
    let app = app.unwrap_or_else(|| {
        workspace()
            .join("target")
            .join(target)
            .join("release/bundle/macos")
            .join(APP)
    });
    println!("\n== verifying {} ==", app.display());
    let contents = app.join("Contents");
    let mut problems = vec![];

    let exes = ["MacOS/phonebridge", "MacOS/adb", "MacOS/scrcpy"];
    let data = [
        "Resources/scrcpy-server",
        "Resources/scrcpy-icons/scrcpy.png",
        "Resources/licenses/scrcpy-LICENSE.txt",
        "Resources/licenses/platform-tools-NOTICE.txt",
        "Resources/licenses/go/MODULES.txt",
    ];
    for rel in exes.iter().chain(data.iter()) {
        let p = contents.join(rel);
        match std::fs::metadata(&p) {
            Ok(m) => {
                use std::os::unix::fs::PermissionsExt;
                let exec = m.permissions().mode() & 0o111 != 0;
                let want_exec = exes.contains(rel);
                if want_exec && !exec {
                    problems.push(format!("{rel} is not executable"));
                }
                println!(
                    "  ok   {rel} ({} B{})",
                    m.len(),
                    if exec { ", executable" } else { "" }
                );
            }
            Err(_) => problems.push(format!("missing {rel}")),
        }
    }

    if cfg!(target_os = "macos") {
        for rel in exes {
            let p = contents.join(rel);
            match run(Command::new("lipo").arg("-archs").arg(&p)) {
                Ok(a) => println!("  arch {rel}: {}", a.trim()),
                Err(e) => problems.push(e),
            }
            match run(Command::new("codesign")
                .args(["-dv", "--verbose=2"])
                .arg(&p))
            {
                Ok(info) => {
                    let sig = info
                        .lines()
                        .find(|l| l.starts_with("Signature="))
                        .unwrap_or("Signature=?");
                    let flags = info
                        .lines()
                        .find(|l| l.contains("flags="))
                        .unwrap_or("")
                        .trim();
                    println!("  sign {rel}: {sig} {flags}");
                }
                Err(e) => problems.push(format!("{rel} is not signed: {e}")),
            }
        }
        match run(Command::new("codesign")
            .args(["--verify", "--deep", "--strict", "--verbose=2"])
            .arg(&app))
        {
            Ok(_) => println!("  ok   codesign --verify --deep --strict"),
            Err(e) => problems.push(e),
        }
        // Informational: an ad-hoc app is expected to be rejected by Gatekeeper.
        if let Err(e) = run(Command::new("spctl")
            .args(["--assess", "--type", "execute", "-vv"])
            .arg(&app))
        {
            println!(
                "  note Gatekeeper assessment (expected to fail until notarized): {}",
                e.lines().last().unwrap_or("").trim()
            );
        }
    } else {
        println!("  (signature and architecture checks only run on macOS)");
    }

    if problems.is_empty() {
        println!("bundle OK");
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}
