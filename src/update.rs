use semver::Version;
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const REPOSITORY: &str = "RainyPixel/codex-wake";
const API_LATEST_RELEASE: &str =
    "https://api.github.com/repos/RainyPixel/codex-wake/releases/latest";
const CHECKSUMS_ASSET: &str = "SHA256SUMS";
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateReport {
    pub current: String,
    pub latest: String,
    pub update_available: bool,
    pub release_url: String,
}

#[derive(Debug, Clone)]
pub struct AvailableRelease {
    release: GitHubRelease,
    version: Version,
}

pub fn check_for_update() -> Result<(UpdateReport, AvailableRelease), String> {
    let release = fetch_latest_release()?;
    let latest = parse_tag(&release.tag_name)?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|error| format!("parse current version: {error}"))?;
    let report = UpdateReport {
        current: current.to_string(),
        latest: latest.to_string(),
        update_available: latest > current,
        release_url: release.html_url.clone(),
    };
    Ok((
        report,
        AvailableRelease {
            release,
            version: latest,
        },
    ))
}

pub fn install_release(release: &AvailableRelease, executable: &Path) -> Result<(), String> {
    if is_cargo_target_binary(executable) {
        return Err(format!(
            "refusing to self-update a Cargo build artifact at {}; install a release binary first",
            executable.display()
        ));
    }
    let asset_name = platform_asset_name()?;
    let asset = find_asset(&release.release, &asset_name)?;
    let checksums = find_asset(&release.release, CHECKSUMS_ASSET)?;
    let workspace = UpdateDir::new()?;
    let archive_path = workspace.as_ref().join(&asset_name);
    let checksums_path = workspace.as_ref().join(CHECKSUMS_ASSET);
    download(&asset.browser_download_url, &archive_path)?;
    download(&checksums.browser_download_url, &checksums_path)?;
    verify_checksum(&checksums_path, &asset_name, &archive_path)?;
    validate_archive(&archive_path)?;
    extract_binary(&archive_path, workspace.as_ref())?;
    let candidate = workspace.as_ref().join("codex-wake");
    validate_candidate(&candidate, &release.version)?;
    atomic_replace_executable(&candidate, executable)
}

pub fn platform_asset_name() -> Result<String, String> {
    let target = match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        (os, arch) => return Err(format!("no release binary is published for {os}/{arch}")),
    };
    Ok(format!("codex-wake-{target}.tar.gz"))
}

fn fetch_latest_release() -> Result<GitHubRelease, String> {
    let output = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "8",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2026-03-10",
            "--user-agent",
            "codex-wake",
            API_LATEST_RELEASE,
        ])
        .output()
        .map_err(|error| format!("run curl: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "query GitHub Releases: {}",
            bounded_stderr(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("parse GitHub release response: {error}"))
}

fn parse_tag(tag: &str) -> Result<Version, String> {
    let version = tag
        .strip_prefix('v')
        .ok_or_else(|| format!("latest release tag `{tag}` does not start with `v`"))?;
    Version::parse(version).map_err(|error| format!("parse release tag `{tag}`: {error}"))
}

fn find_asset<'a>(release: &'a GitHubRelease, name: &str) -> Result<&'a GitHubAsset, String> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| format!("release {} has no `{name}` asset", release.tag_name))
}

fn download(url: &str, destination: &Path) -> Result<(), String> {
    let output = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "120",
            "--user-agent",
            "codex-wake",
            "--output",
        ])
        .arg(destination)
        .arg(url)
        .output()
        .map_err(|error| format!("run curl: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "download {url}: {}",
            bounded_stderr(&output.stderr)
        ));
    }
    Ok(())
}

fn verify_checksum(manifest: &Path, asset_name: &str, archive: &Path) -> Result<(), String> {
    let manifest =
        fs::read_to_string(manifest).map_err(|error| format!("read checksum manifest: {error}"))?;
    let expected = manifest
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let checksum = fields.next()?;
            let name = fields.next()?.trim_start_matches('*');
            (name == asset_name).then_some(checksum)
        })
        .next()
        .ok_or_else(|| format!("checksum manifest has no entry for `{asset_name}`"))?;
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("invalid SHA-256 checksum for `{asset_name}`"));
    }
    let actual = sha256(archive)?;
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(format!(
            "SHA-256 mismatch for `{asset_name}`: expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

fn sha256(path: &Path) -> Result<String, String> {
    for (command, arguments) in [
        ("sha256sum", vec![path.as_os_str()]),
        (
            "shasum",
            vec![OsStr::new("-a"), OsStr::new("256"), path.as_os_str()],
        ),
    ] {
        let output = match Command::new(command).args(arguments).output() {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("run {command}: {error}")),
        };
        if !output.status.success() {
            return Err(format!(
                "{command} failed: {}",
                bounded_stderr(&output.stderr)
            ));
        }
        let checksum = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned();
        if checksum.len() == 64 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(checksum);
        }
        return Err(format!("could not parse {command} output"));
    }
    Err("neither `sha256sum` nor `shasum` is available".to_owned())
}

fn validate_archive(archive: &Path) -> Result<(), String> {
    let output = Command::new("tar")
        .args(["-tzf"])
        .arg(archive)
        .output()
        .map_err(|error| format!("run tar: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "inspect release archive: {}",
            bounded_stderr(&output.stderr)
        ));
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let entries = listing.lines().collect::<Vec<_>>();
    if entries
        .iter()
        .filter(|entry| **entry == "codex-wake")
        .count()
        != 1
        || entries.iter().any(|entry| {
            entry.starts_with('/') || entry.split('/').any(|component| component == "..")
        })
    {
        return Err("release archive has an unsafe or unexpected layout".to_owned());
    }
    Ok(())
}

fn extract_binary(archive: &Path, destination: &Path) -> Result<(), String> {
    let output = Command::new("tar")
        .args(["-xzf"])
        .arg(archive)
        .arg("-C")
        .arg(destination)
        .arg("codex-wake")
        .output()
        .map_err(|error| format!("run tar: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "extract release binary: {}",
            bounded_stderr(&output.stderr)
        ));
    }
    Ok(())
}

fn validate_candidate(candidate: &Path, expected: &Version) -> Result<(), String> {
    let metadata =
        fs::metadata(candidate).map_err(|error| format!("inspect downloaded binary: {error}"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err("downloaded codex-wake is not executable".to_owned());
    }
    let output = Command::new(candidate)
        .arg("--version")
        .output()
        .map_err(|error| format!("run downloaded codex-wake: {error}"))?;
    let expected_output = format!("codex-wake {expected}");
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != expected_output
    {
        return Err(format!(
            "downloaded binary version does not match release {expected}"
        ));
    }
    Ok(())
}

fn atomic_replace_executable(source: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", destination.display()))?;
    let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}.update.{}.{}",
        destination
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("codex-wake"),
        std::process::id(),
        sequence
    ));
    let result = (|| {
        let mut input = fs::File::open(source)
            .map_err(|error| format!("open downloaded executable: {error}"))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&temporary)
            .map_err(|error| format!("create {}: {error}", temporary.display()))?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|error| format!("read downloaded executable: {error}"))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| format!("write {}: {error}", temporary.display()))?;
        }
        output
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", temporary.display()))?;
        fs::rename(&temporary, destination)
            .map_err(|error| format!("replace {}: {error}", destination.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn is_cargo_target_binary(path: &Path) -> bool {
    let components = path
        .components()
        .map(|component| component.as_os_str())
        .collect::<Vec<_>>();
    components.windows(2).any(|window| {
        window[0] == OsStr::new("target") && matches!(window[1].to_str(), Some("debug" | "release"))
    })
}

fn bounded_stderr(stderr: &[u8]) -> String {
    let start = stderr.len().saturating_sub(1024);
    String::from_utf8_lossy(&stderr[start..]).trim().to_owned()
}

struct UpdateDir(PathBuf);

impl UpdateDir {
    fn new() -> Result<Self, String> {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "codex-wake-update-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)
            .map_err(|error| format!("create update workspace {}: {error}", path.display()))?;
        Ok(Self(path))
    }
}

impl AsRef<Path> for UpdateDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for UpdateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_tags_are_strict_semver() {
        assert_eq!(parse_tag("v1.2.3").unwrap(), Version::new(1, 2, 3));
        assert!(parse_tag("1.2.3").is_err());
        assert!(parse_tag("vlatest").is_err());
    }

    #[test]
    fn cargo_build_artifacts_are_not_self_updated() {
        assert!(is_cargo_target_binary(Path::new(
            "/project/target/debug/codex-wake"
        )));
        assert!(!is_cargo_target_binary(Path::new(
            "/home/user/.local/bin/codex-wake"
        )));
    }
}
