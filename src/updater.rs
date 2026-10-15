//! Updates through GitHub Releases: check the latest release of this repo, download its
//! CrispyClipper.exe, verify it against the SHA-256 GitHub publishes for the asset, and swap
//! it in place (a running exe can be renamed on Windows, just not overwritten).

use crate::config;
use crate::ffmpeg::CREATE_NO_WINDOW;
use crate::install;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

const LATEST: &str = "https://api.github.com/repos/gxvingates/crispy-clipper/releases/latest";
const ASSET: &str = "CrispyClipper.exe";

#[derive(Clone, Debug, serde::Serialize)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub page: String,
    url: String,
    sha256: Option<String>,
}

pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn parse_version(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split(['.', '-']).map(|p| p.parse().unwrap_or(0)).collect()
}

/// True if `a` is a newer version than `b` ("0.10.0" > "0.9.3").
pub fn newer(a: &str, b: &str) -> bool {
    let (a, b) = (parse_version(a), parse_version(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

fn curl() -> Command {
    let mut c = Command::new("curl.exe");
    c.creation_flags(CREATE_NO_WINDOW).args(["-fsSL", "--retry", "2", "-A", "CrispyClipper"]);
    c
}

/// The latest release on GitHub, if it's newer than this build.
pub fn check() -> Result<Option<Release>, String> {
    let out = curl()
        .args(["-H", "Accept: application/vnd.github+json", LATEST])
        .output()
        .map_err(|e| format!("couldn't run curl.exe: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let j: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let version = j["tag_name"].as_str().unwrap_or_default().trim_start_matches('v').to_string();
    if version.is_empty() || !newer(&version, current()) {
        return Ok(None);
    }
    let asset = j["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(ASSET)))
        .ok_or("the release has no CrispyClipper.exe")?;
    Ok(Some(Release {
        version,
        notes: j["body"].as_str().unwrap_or_default().to_string(),
        page: j["html_url"].as_str().unwrap_or_default().to_string(),
        url: asset["browser_download_url"].as_str().unwrap_or_default().to_string(),
        sha256: asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase),
    }))
}

/// Downloads and verifies `rel`, swaps it in for the running exe and returns its path.
/// The caller restarts the app from that path.
pub fn install(rel: &Release) -> Result<PathBuf, String> {
    let dir = config::data_dir().join("update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let part = dir.join("CrispyClipper.exe.part");
    let out = curl().arg("-o").arg(&part).arg(&rel.url).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("download failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let bytes = std::fs::read(&part).map_err(|e| e.to_string())?;
    if bytes.len() < 100_000 || &bytes[..2] != b"MZ" {
        return Err("the download isn't a valid program".into());
    }
    if let Some(want) = &rel.sha256 {
        let got: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        if &got != want {
            let _ = std::fs::remove_file(&part);
            return Err("the download didn't match GitHub's checksum".into());
        }
    }
    let exe = install::current_exe();
    let old = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("couldn't replace the app: {e}"))?;
    if let Err(e) = std::fs::rename(&part, &exe) {
        let _ = std::fs::rename(&old, &exe); // put the working version back
        return Err(format!("couldn't replace the app: {e}"));
    }
    Ok(exe)
}

/// Removes the previous version left behind by an update.
pub fn cleanup() {
    let _ = std::fs::remove_file(install::current_exe().with_extension("exe.old"));
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_order() {
        assert!(super::newer("0.2.0", "0.1.9"));
        assert!(super::newer("v0.10.0", "0.9.3"));
        assert!(super::newer("1.0.0", "0.99.99"));
        assert!(!super::newer("0.2.0", "0.2.0"));
        assert!(!super::newer("0.1.0", "0.2.0"));
    }
}
