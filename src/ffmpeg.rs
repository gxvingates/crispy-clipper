//! Finding (or fetching) FFmpeg, and the encoder settings used for capture and editing.

use crate::config::{self, Config};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Minimal FFmpeg built by this repo's CI (only what Crispy Clipper uses).
pub const DOWNLOAD_URL: &str =
    "https://github.com/gxvingates/crispy-clipper/releases/download/ffmpeg-min-1/ffmpeg.exe";

pub fn managed_path() -> PathBuf {
    config::data_dir().join("ffmpeg.exe")
}

/// The app's own download, then ffmpeg.exe next to the app, then a custom path, then PATH.
pub fn locate(cfg: &Config) -> Option<PathBuf> {
    let custom = (cfg.ffmpeg != "ffmpeg").then(|| PathBuf::from(&cfg.ffmpeg));
    let on_path = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p).map(|d| d.join("ffmpeg.exe")).find(|f| f.exists())
    });
    [Some(managed_path()), Some(config::exe_dir().join("ffmpeg.exe")), custom, on_path]
        .into_iter()
        .flatten()
        .find(|p| p.exists())
}

/// Downloads the minimal FFmpeg with Windows' built-in curl.
pub fn download() -> Result<PathBuf, String> {
    let dest = managed_path();
    let part = dest.with_extension("part");
    std::fs::create_dir_all(config::data_dir()).map_err(|e| e.to_string())?;
    let out = Command::new("curl.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .args(["-fsSL", "--retry", "2", "-o"])
        .arg(&part)
        .arg(DOWNLOAD_URL)
        .output()
        .map_err(|e| format!("couldn't run curl.exe: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&part);
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
    Ok(dest)
}

pub fn command(path: &PathBuf) -> Command {
    let mut c = Command::new(path);
    c.creation_flags(CREATE_NO_WINDOW)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .stdin(Stdio::null());
    c
}

pub fn has_filter(path: &PathBuf, name: &str) -> bool {
    command(path)
        .arg("-filters")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().nth(1) == Some(name)))
        .unwrap_or(false)
}

/// Hardware encoders in preference order, then the CPU fallback.
pub fn encoder_candidates(cfg: &Config) -> Vec<String> {
    if cfg.encoder != "auto" {
        return vec![cfg.encoder.clone()];
    }
    let mut v = Vec::new();
    for codec in [cfg.codec.as_str(), "h264"] {
        for hw in ["nvenc", "amf", "qsv", "mf"] {
            let e = format!("{codec}_{hw}");
            if !v.contains(&e) && !(codec == "av1" && hw == "mf") {
                v.push(e);
            }
        }
    }
    v.push("libx264".into());
    v
}

/// Encoders that accept DDA's D3D11 frames directly.
pub fn takes_d3d11(encoder: &str) -> bool {
    encoder.ends_with("_nvenc") || encoder.ends_with("_amf")
}

/// Quality-targeted (VBR) settings with a bitrate ceiling.
pub fn encoder_args(encoder: &str, quality: u32, max_mbps: u32, gop: u32) -> Vec<String> {
    let q = quality.to_string();
    let max = format!("{max_mbps}M");
    let buf = format!("{}M", max_mbps * 2);
    let avg = format!("{}M", (max_mbps * 3 / 5).max(1));
    let mut a: Vec<String> = vec!["-c:v".into(), encoder.into()];
    let extra: Vec<&str> = if encoder.ends_with("_nvenc") {
        vec!["-preset", "p5", "-tune", "hq", "-rc", "vbr", "-cq", &q, "-b:v", "0", "-maxrate", &max,
             "-bufsize", &buf, "-spatial-aq", "1", "-bf", "0"]
    } else if encoder.ends_with("_amf") {
        vec!["-usage", "transcoding", "-quality", "balanced", "-rc", "vbr_peak", "-b:v", &avg, "-maxrate", &max,
             "-bufsize", &buf]
    } else if encoder.ends_with("_qsv") {
        vec!["-preset", "medium", "-global_quality", &q]
    } else if encoder.ends_with("_mf") {
        vec!["-hw_encoding", "1", "-b:v", &avg]
    } else {
        vec!["-preset", "veryfast", "-crf", &q, "-maxrate", &max, "-bufsize", &buf]
    };
    a.extend(extra.into_iter().map(String::from));
    a.extend(["-g".into(), gop.to_string()]);
    a
}

/// Runs `args` as a short self-test; Ok if FFmpeg succeeds.
pub fn test(path: &PathBuf, args: &[String]) -> Result<(), String> {
    let out = command(path).args(args).stdout(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("").to_string())
    }
}
