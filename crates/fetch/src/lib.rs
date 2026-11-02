//! Pinned downloads for the build.
//!
//! Everything the build pulls from the internet is listed in `deps.toml` at the
//! workspace root with a SHA-256. This crate downloads with the system `curl`
//! (always present on macOS, honours proxies and the system trust store),
//! verifies the hash in Rust, and unpacks with the system `tar` / `unzip`.
//!
//! It is used by `libtailscale-sys/build.rs`, `src-tauri/build.rs` and `xtask`.

pub mod bundle;

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use sha2::{Digest, Sha256};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error(e.to_string())
    }
}

macro_rules! bail {
    ($($t:tt)*) => { return Err(Error(format!($($t)*))) };
}

/// One downloadable file.
#[derive(Debug, Clone, Deserialize)]
pub struct Archive {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
pub struct Scrcpy {
    pub version: String,
    /// Keyed by Rust target triple.
    pub archives: BTreeMap<String, Archive>,
}

#[derive(Debug, Deserialize)]
pub struct PlatformTools {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
pub struct Go {
    pub version: String,
    /// Keyed by `<goos>-<goarch>` of the *build host*.
    pub archives: BTreeMap<String, Archive>,
}

#[derive(Debug, Deserialize)]
pub struct Libtailscale {
    pub repo: String,
    pub commit: String,
}

/// Parsed `deps.toml`.
#[derive(Debug, Deserialize)]
pub struct Pins {
    pub scrcpy: Scrcpy,
    #[serde(rename = "platform-tools")]
    pub platform_tools: PlatformTools,
    pub go: Go,
    pub libtailscale: Libtailscale,
}

impl Pins {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .map_err(|e| Error(format!("reading {}: {e}", path.display())))?;
        toml::from_str(&text).map_err(|e| Error(format!("parsing {}: {e}", path.display())))
    }

    pub fn platform_tools_archive(&self) -> Archive {
        Archive {
            url: self.platform_tools.url.clone(),
            sha256: self.platform_tools.sha256.clone(),
        }
    }
}

/// Walk up from `start` to the directory holding `deps.toml`.
pub fn workspace_root(start: &Path) -> Result<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join("deps.toml").is_file() {
            return Ok(d.to_path_buf());
        }
        dir = d.parent();
    }
    bail!("could not find deps.toml above {}", start.display())
}

/// Download cache shared by all build scripts.
/// `PHONEBRIDGE_CACHE` overrides; default is `<workspace>/target/phonebridge-cache`.
pub fn cache_dir(workspace: &Path) -> PathBuf {
    match std::env::var_os("PHONEBRIDGE_CACHE") {
        Some(p) => PathBuf::from(p),
        None => workspace.join("target").join("phonebridge-cache"),
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn file_name(url: &str) -> Result<&str> {
    match url.rsplit('/').next() {
        Some(n) if !n.is_empty() => Ok(n),
        _ => bail!("cannot derive a file name from {url}"),
    }
}

/// Return a verified local copy of `archive`, downloading it into `cache` if needed.
///
/// A cached file whose hash doesn't match is deleted and fetched again. With
/// `PHONEBRIDGE_OFFLINE=1` a missing file is an error instead of a download.
pub fn fetch(archive: &Archive, cache: &Path) -> Result<PathBuf> {
    let dest = cache.join("downloads").join(file_name(&archive.url)?);
    let want = archive.sha256.to_ascii_lowercase();

    if dest.is_file() {
        if sha256_file(&dest)? == want {
            return Ok(dest);
        }
        fs::remove_file(&dest)?;
    }

    if std::env::var_os("PHONEBRIDGE_OFFLINE").is_some() {
        bail!(
            "{} is not in the cache ({}) and PHONEBRIDGE_OFFLINE is set",
            archive.url,
            dest.display()
        );
    }

    fs::create_dir_all(dest.parent().unwrap())?;
    let partial = dest.with_extension("partial");
    let status = Command::new("curl")
        .args(["--fail", "--location", "--silent", "--show-error"])
        .args([
            "--retry",
            "4",
            "--retry-delay",
            "2",
            "--connect-timeout",
            "30",
        ])
        .arg("--output")
        .arg(&partial)
        .arg(&archive.url)
        .status()
        .map_err(|e| Error(format!("running curl (is it installed?): {e}")))?;
    if !status.success() {
        let _ = fs::remove_file(&partial);
        bail!("download failed ({status}): {}", archive.url);
    }

    let got = sha256_file(&partial)?;
    if got != want {
        let _ = fs::remove_file(&partial);
        bail!(
            "SHA-256 mismatch for {}\n  expected {want}\n  got      {got}\nRefusing to use it.",
            archive.url
        );
    }
    fs::rename(&partial, &dest)?;
    Ok(dest)
}

/// Unpack a `.tar.gz` into `into` (created if missing).
pub fn untar_gz(archive: &Path, into: &Path) -> Result<()> {
    fs::create_dir_all(into)?;
    run(Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into))
}

/// Unpack selected entries (or everything, if `entries` is empty) of a `.zip`.
pub fn unzip(archive: &Path, into: &Path, entries: &[&str]) -> Result<()> {
    fs::create_dir_all(into)?;
    let mut cmd = Command::new("unzip");
    cmd.args(["-q", "-o"]).arg(archive);
    cmd.args(entries);
    cmd.arg("-d").arg(into);
    run(&mut cmd)
}

/// Unpack `archive` into `<cache>/extracted/<stamp>/` once, and return that directory.
/// A `.done` marker makes repeat builds skip the work.
pub fn extract_once(archive: &Path, cache: &Path, stamp: &str) -> Result<PathBuf> {
    let dir = cache.join("extracted").join(stamp);
    let marker = dir.join(".done");
    if marker.is_file() {
        return Ok(dir);
    }
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    let name = archive.to_string_lossy();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        untar_gz(archive, &dir)?;
    } else if name.ends_with(".zip") {
        unzip(archive, &dir, &[])?;
    } else {
        bail!("don't know how to unpack {name}");
    }
    fs::write(&marker, b"")?;
    Ok(dir)
}

/// Copy `from` to `to` (only when the contents differ, so mtimes stay stable
/// and cargo doesn't rebuild needlessly) and optionally mark it executable.
pub fn install_file(from: &Path, to: &Path, executable: bool) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    let same = to.is_file() && sha256_file(from)? == sha256_file(to)?;
    if !same {
        let tmp = to.with_extension("tmp-install");
        fs::copy(from, &tmp)
            .map_err(|e| Error(format!("copy {} -> {}: {e}", from.display(), to.display())))?;
        fs::rename(&tmp, to)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(to, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd
        .output()
        .map_err(|e| Error(format!("running {cmd:?}: {e}")))?;
    if !out.status.success() {
        bail!(
            "{cmd:?} failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_parse() {
        let root = workspace_root(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let pins = Pins::load(&root.join("deps.toml")).unwrap();
        assert_eq!(pins.scrcpy.version, "5.0.1");
        assert!(pins.scrcpy.archives.contains_key("aarch64-apple-darwin"));
        assert!(pins.go.archives.contains_key("darwin-arm64"));
        for a in pins
            .scrcpy
            .archives
            .values()
            .chain(pins.go.archives.values())
        {
            assert_eq!(a.sha256.len(), 64);
        }
        assert_eq!(pins.libtailscale.commit.len(), 40);
    }

    #[test]
    fn sha256_known_value() {
        let dir = std::env::temp_dir().join(format!("fetch-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("abc");
        fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_bad_hash() {
        let dir = std::env::temp_dir().join(format!("fetch-bad-{}", std::process::id()));
        let src = dir.join("src.bin");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&src, b"hello").unwrap();
        let archive = Archive {
            url: format!("file://{}", src.display()),
            sha256: "0".repeat(64),
        };
        let err = fetch(&archive, &dir.join("cache")).unwrap_err();
        assert!(err.to_string().contains("SHA-256 mismatch"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
