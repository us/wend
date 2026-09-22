//! `wend update`: replace this binary with the newest GitHub Release.
//!
//! Standalone by design: it needs no index, no store, and no new dependency.
//! HTTP and unpacking go through `curl` and `tar`, which also keeps the
//! feature working on every release target (macOS arm64/x64, Linux
//! gnu/musl, Windows x64) without platform crates.
//!
//! Asset contract (see `.github/workflows/release.yml`): unix targets ship
//! `wend-<target>.tar.gz` holding a `wend` binary, Windows ships
//! `wend-<target>.zip` holding `wend.exe`, each with a `.sha256` sidecar.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The repository whose Releases carry the binaries.
const RELEASES_LATEST: &str = "https://api.github.com/repos/us/wend/releases/latest";

/// Release target triple plus the archive extension that target ships.
fn release_target() -> Result<(&'static str, &'static str), String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok(("aarch64-apple-darwin", "tar.gz")),
        ("macos", "x86_64") => Ok(("x86_64-apple-darwin", "tar.gz")),
        ("linux", "x86_64") => Ok(("x86_64-unknown-linux-gnu", "tar.gz")),
        ("windows", "x86_64") => Ok(("x86_64-pc-windows-msvc", "zip")),
        (os, arch) => Err(format!(
            "no prebuilt binary for {os}-{arch}; install from source instead: \
             cargo install --path crates/wend-cli"
        )),
    }
}

/// True when `latest` is strictly newer than `current`. Both are dot-separated
/// numbers (`1.2.10` beats `1.2.9`); a non-version never counts as newer.
fn is_newer_version(latest: &str, current: &str) -> bool {
    let parse = |v: &str| {
        v.split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    };
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// `wend update` installs the newest release over this binary;
/// `wend update --check` only reports what the newest release is.
pub fn run(args: &[String]) -> anyhow::Result<()> {
    let check_only = args.iter().any(|a| a == "--check");
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!(
            "usage:\n\
             \x20 wend update           install the newest release over this binary\n\
             \x20 wend update --check   only report what the newest release is\n"
        );
        return Ok(());
    }
    let current = env!("CARGO_PKG_VERSION");
    let (target, ext) = release_target().map_err(|e| anyhow::anyhow!("{e}"))?;
    let asset = format!("wend-{target}.{ext}");
    let latest = fetch_latest(&asset)
        .map_err(|detail| anyhow::anyhow!("wend update: cannot reach GitHub Releases: {detail}"))?;
    if !is_newer_version(&latest.version, current) {
        println!("wend {current} is the newest release");
        return Ok(());
    }
    if check_only {
        println!(
            "update available: {current} -> {}: run `wend update` to install it",
            latest.version
        );
        return Ok(());
    }
    install(&latest, current)
}

/// What the newest release is: its version and where its asset lives.
struct Release {
    version: String,
    asset_url: String,
    checksum_url: Option<String>,
}

/// Read the newest release off the GitHub API with curl, which keeps this
/// updater free of HTTP dependencies. curl honours the proxy environment, so
/// a blackholed network fails here rather than hanging.
fn fetch_latest(asset: &str) -> Result<Release, String> {
    let body = curl_api(RELEASES_LATEST)?;
    let json: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| format!("the releases API answered what is not JSON: {e}"))?;
    fetch_latest_parsing(&json, asset)
}

/// The version and the asset URL out of one releases API answer, so the
/// parsing is testable without the network.
fn fetch_latest_parsing(json: &serde_json::Value, asset: &str) -> Result<Release, String> {
    let tag = json
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "the releases API answered without a tag_name".to_string())?;
    let version = tag.trim_start_matches('v').to_string();
    if !is_newer_version(&version, "0") && version != "0" && version.parse::<u64>().is_err() {
        // Reject non-versions ("nightly", ...); accept anything shaped like one.
        let shaped = !version.is_empty()
            && version
                .split('.')
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
        if !shaped {
            return Err(format!(
                "the newest release is tagged {tag:?}, which is no version"
            ));
        }
    }
    let assets = json
        .get("assets")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let url_of = |name: &str| {
        assets.iter().find_map(|a| {
            if a.get("name").and_then(|n| n.as_str()) == Some(name) {
                a.get("browser_download_url")
                    .and_then(|u| u.as_str())
                    .map(str::to_string)
            } else {
                None
            }
        })
    };
    let Some(asset_url) = url_of(asset) else {
        return Err(format!("the newest release carries no {asset}"));
    };
    Ok(Release {
        version,
        asset_url,
        checksum_url: url_of(&format!("{asset}.sha256")),
    })
}

/// GET a URL to memory. Anything curl reports becomes text the person can act on.
///
/// An API token rides along when one is in the environment: anonymous calls share
/// 60 requests an hour per IP, while a token gets 5000. Nothing is stored
/// anywhere; the token only travels in this request.
fn curl_api(url: &str) -> Result<Vec<u8>, String> {
    let mut command = Command::new("curl");
    command.args([
        "-fsSL",
        "--max-time",
        "25",
        "-H",
        "Accept: application/vnd.github+json",
        url,
    ]);
    if let Some(token) = github_token() {
        command.args(["-H", &format!("Authorization: Bearer {token}")]);
    }
    let out = command
        .output()
        .map_err(|e| format!("curl is not installed or cannot run: {e}"))?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("curl exited with {}", out.status)
        } else {
            detail
        });
    }
    Ok(out.stdout)
}

/// A GitHub API token from the environment, when the person has one.
fn github_token() -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|token| token.trim().to_string())
        .find(|token| !token.is_empty())
}

/// Download a URL to a file.
fn download(url: &str, dest: &Path) -> Result<(), String> {
    // `file://` URLs only appear in tests; real assets are https.
    if let Some(path) = url.strip_prefix("file://") {
        std::fs::copy(path, dest).map_err(|e| format!("cannot copy test fixture: {e}"))?;
        return Ok(());
    }
    let status = Command::new("curl")
        .args(["-fsSL", "--max-time", "120", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|e| format!("curl is not installed or cannot run: {e}"))?;
    if !status.success() {
        return Err(format!("curl exited with {status} downloading the release"));
    }
    Ok(())
}

/// Fetch the archive, check it, and swap it over this binary.
fn install(latest: &Release, current: &str) -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("wend update: cannot find this binary: {e}"))?;
    install_to(latest, current, &exe)
}

/// The same swap onto an explicit path, so tests drive the whole chain without
/// touching the test binary they are running inside of.
fn install_to(latest: &Release, current: &str, exe: &Path) -> anyhow::Result<()> {
    // Counted, because one process can install twice: two tests in one binary share
    // a pid, and a shared work directory would have them unpacking over each other.
    static INSTALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = INSTALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let work: PathBuf =
        std::env::temp_dir().join(format!("wend-update-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&work)?;
    let cleanup = || std::fs::remove_dir_all(&work).ok();
    let failed = |message: String| -> anyhow::Error {
        cleanup();
        anyhow::anyhow!("{message}")
    };

    let archive_name = latest
        .asset_url
        .rsplit('/')
        .next()
        .unwrap_or("wend-release");
    let archive = work.join(archive_name);
    if let Err(detail) = download(&latest.asset_url, &archive) {
        return Err(failed(format!(
            "wend update: the download failed: {detail}"
        )));
    }
    if let Some(url) = &latest.checksum_url {
        let checksum_file = work.join(format!("{archive_name}.sha256"));
        match download(url, &checksum_file) {
            Ok(()) => {
                if let Err(detail) = check_sha256(&archive, &checksum_file) {
                    return Err(failed(format!(
                        "wend update: refusing: the checksum does not match: {detail}"
                    )));
                }
            }
            Err(detail) => {
                eprintln!("wend update: warning: no checksum to check against: {detail}");
            }
        }
    }
    // `tar` unpacks both formats: bsdtar reads .tar.gz and .zip alike, and it
    // ships with Windows 10+, so this stays dependency-free on every target.
    let tar = Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .current_dir(&work)
        .status()
        .map_err(|e| failed(format!("wend update: cannot unpack the release: {e}")))?;
    if !tar.success() {
        return Err(failed(format!(
            "wend update: cannot unpack the release: tar exited {tar}"
        )));
    }
    let bin_name = exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "wend".to_string());
    let fresh = work.join(&bin_name);
    if !fresh.is_file() {
        // Fallback to the plain unix name when the exe path has none.
        let alt = work.join("wend");
        if !alt.is_file() {
            return Err(failed(format!(
                "wend update: the release holds no {bin_name} binary"
            )));
        }
        std::fs::rename(&alt, &fresh).map_err(|e| failed(format!("wend update: {e}")))?;
    }
    let reported = Command::new(&fresh)
        .arg("--version")
        .output()
        .map_err(|e| {
            failed(format!(
                "wend update: the downloaded binary does not run: {e}"
            ))
        })?;
    let reported = String::from_utf8_lossy(&reported.stdout).trim().to_string();
    if reported != format!("wend {}", latest.version) {
        return Err(failed(format!(
            "wend update: refusing: the download reports {reported:?}, want {:?}",
            format!("wend {}", latest.version)
        )));
    }
    swap_over(&fresh, exe).map_err(|e| failed(format!("wend update: {e}")))?;
    cleanup();
    let mut out = std::io::stdout();
    writeln!(
        out,
        "updated wend {current} -> {} at {}",
        latest.version,
        exe.display()
    )?;
    Ok(())
}

#[cfg(unix)]
fn swap_over(fresh: &Path, exe: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let staged = exe.with_extension("new");
    std::fs::copy(fresh, &staged).map_err(|e| format!("cannot stage the new binary: {e}"))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("cannot chmod the new binary: {e}"))?;
    // One atomic rename: a running process keeps serving from its old image.
    std::fs::rename(&staged, exe).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("cannot replace {}: {e}", exe.display())
    })
}

#[cfg(windows)]
fn swap_over(fresh: &Path, exe: &Path) -> Result<(), String> {
    // Windows locks the running image, so an in-place rename fails. Stage the
    // new binary next to the old one and let the person swap it after exiting.
    let staged = exe.with_extension("new.exe");
    std::fs::copy(fresh, &staged).map_err(|e| format!("cannot stage the new binary: {e}"))?;
    match std::fs::rename(&staged, exe) {
        Ok(()) => Ok(()),
        Err(_) => Err(format!(
            "cannot replace the running binary on Windows: the new release is staged at {}; \
             exit wend and rename it over {}",
            staged.display(),
            exe.display()
        )),
    }
}

/// Compare the archive against its `.sha256` file.
fn check_sha256(archive: &Path, checksum_file: &Path) -> Result<(), String> {
    let text =
        std::fs::read_to_string(checksum_file).map_err(|e| format!("cannot read it: {e}"))?;
    // Both sidecar shapes occur in the wild: `<hash>  <name>` (shasum) and a
    // bare `<HASH>` line (PowerShell Get-FileHash). First whitespace-separated
    // token is the hash either way.
    let want = text
        .split_whitespace()
        .next()
        .ok_or_else(|| "the file is empty".to_string())?;
    let got = sha256_of(archive)?;
    if got.eq_ignore_ascii_case(want) {
        Ok(())
    } else {
        Err("the hash differs".to_string())
    }
}

#[cfg(unix)]
fn sha256_of(archive: &Path) -> Result<String, String> {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(archive)
        .output()
        .map_err(|e| format!("shasum is not installed: {e}"))?;
    if !out.status.success() {
        return Err("shasum failed".to_string());
    }
    let got = String::from_utf8_lossy(&out.stdout);
    Ok(got.split_whitespace().next().unwrap_or("").to_string())
}

#[cfg(windows)]
fn sha256_of(archive: &Path) -> Result<String, String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command"])
        .arg(format!(
            "(Get-FileHash '{}' -Algorithm SHA256).Hash",
            archive.display()
        ))
        .output()
        .map_err(|e| format!("powershell is not available: {e}"))?;
    if !out.status.success() {
        return Err("Get-FileHash failed".to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::{fetch_latest_parsing, install_to, is_newer_version, Release};

    #[test]
    fn newer_compares_segment_by_segment() {
        assert!(is_newer_version("0.2.0", "0.1.0"));
        assert!(is_newer_version("0.1.10", "0.1.9"));
        assert!(!is_newer_version("0.1.0", "0.1.0"));
        assert!(!is_newer_version("0.1.0", "0.2.0"));
        assert!(!is_newer_version("nightly", "0.1.0"));
    }

    #[test]
    fn the_release_json_yields_a_version_and_an_asset_url() {
        let body = serde_json::json!({
            "tag_name": "v0.2.0",
            "assets": [
                {"name": "wend-aarch64-apple-darwin.tar.gz", "browser_download_url": "https://example.invalid/wend.tgz"},
                {"name": "wend-aarch64-apple-darwin.tar.gz.sha256", "browser_download_url": "https://example.invalid/wend.tgz.sha256"},
            ],
        });
        let release = fetch_latest_parsing(&body, "wend-aarch64-apple-darwin.tar.gz")
            .expect("the fixture is well formed");
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.asset_url, "https://example.invalid/wend.tgz");
        assert!(release.checksum_url.is_some());
    }

    #[test]
    fn a_release_without_the_asset_is_refused() {
        let body = serde_json::json!({"tag_name": "v0.2.0", "assets": []});
        assert!(fetch_latest_parsing(&body, "wend-aarch64-apple-darwin.tar.gz").is_err());
    }

    #[test]
    fn a_non_version_tag_is_refused() {
        let body = serde_json::json!({"tag_name": "nightly", "assets": []});
        assert!(fetch_latest_parsing(&body, "wend-aarch64-apple-darwin.tar.gz").is_err());
    }

    /// A stand-in release archive: a script answering `--version` the way the
    /// real binary does, packed the way the release workflow packs it.
    #[cfg(unix)]
    fn fake_release(dir: &std::path::Path, reported: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("wend");
        std::fs::write(&script, format!("#!/bin/sh\necho \"wend {reported}\"\n")).expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let archive = dir.join("wend-test.tar.gz");
        let status = std::process::Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("wend")
            .current_dir(dir)
            .status()
            .expect("tar");
        assert!(status.success());
        format!("file://{}", archive.display())
    }

    #[test]
    #[cfg(unix)]
    fn install_swaps_the_binary_after_verifying_its_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pack = dir.path().join("pack");
        std::fs::create_dir(&pack).expect("pack dir");
        let exe = dir.path().join("wend");
        std::fs::write(&exe, "old binary").expect("old");
        let latest = Release {
            version: "9.9.9".to_string(),
            asset_url: fake_release(&pack, "9.9.9"),
            checksum_url: None,
        };
        install_to(&latest, "0.0.0", &exe).expect("install");
        let out = std::process::Command::new(&exe)
            .arg("--version")
            .output()
            .expect("run the swapped binary");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "wend 9.9.9",
            "the file at the old path is the new release now"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_download_reporting_the_wrong_version_leaves_the_binary_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pack = dir.path().join("pack");
        std::fs::create_dir(&pack).expect("pack dir");
        let exe = dir.path().join("wend");
        std::fs::write(&exe, "old binary").expect("old");
        let latest = Release {
            version: "9.9.9".to_string(),
            asset_url: fake_release(&pack, "0.0.0"),
            checksum_url: None,
        };
        assert!(install_to(&latest, "0.0.0", &exe).is_err());
        assert_eq!(
            std::fs::read(&exe).expect("read"),
            b"old binary",
            "a refused download must not touch the binary in hand"
        );
    }
}
