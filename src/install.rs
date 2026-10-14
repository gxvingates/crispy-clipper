//! Self-install (copy to %LOCALAPPDATA%\Programs, Start menu + desktop shortcuts),
//! start-with-Windows, and uninstall. No installer program needed.

use crate::config;
use std::path::{Path, PathBuf};
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::*;
use windows::core::{GUID, HSTRING, Interface, PCWSTR, w};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Crispy Clipper");
pub const EXE_NAME: &str = "CrispyClipper.exe";

pub fn install_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
    base.join("Programs").join("Crispy Clipper")
}

pub fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}

pub fn is_installed_copy() -> bool {
    let exe = current_exe();
    exe.parent().is_some_and(|d| same_path(d, &install_dir()))
}

/// Running from a cargo build folder (development) — never offer to install.
pub fn is_dev_build() -> bool {
    current_exe().components().any(|c| c.as_os_str().eq_ignore_ascii_case("target"))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase().trim_end_matches('\\') == b.to_string_lossy().to_lowercase().trim_end_matches('\\')
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s.map(PathBuf::from)
    }
}

fn shortcut_paths() -> Vec<PathBuf> {
    [known_folder(&FOLDERID_Programs), known_folder(&FOLDERID_Desktop)]
        .into_iter()
        .flatten()
        .map(|d| d.join("Crispy Clipper.lnk"))
        .collect()
}

fn create_shortcut(lnk: &Path, target: &Path) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(target.as_os_str()))?;
        link.SetWorkingDirectory(&HSTRING::from(target.parent().unwrap().as_os_str()))?;
        link.SetDescription(w!("Crispy Clipper - local game clipping"))?;
        link.SetIconLocation(&HSTRING::from(target.as_os_str()), 0)?;
        link.cast::<IPersistFile>()?.Save(&HSTRING::from(lnk.as_os_str()), true)
    }
}

/// Copies this exe (and a bundled ffmpeg.exe, if any) to the install folder and adds shortcuts.
pub fn install() -> Result<PathBuf, String> {
    let dir = install_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let target = dir.join(EXE_NAME);
    std::fs::copy(current_exe(), &target).map_err(|e| format!("copy failed: {e}"))?;
    let ff = config::exe_dir().join("ffmpeg.exe");
    if ff.exists() {
        let _ = std::fs::copy(&ff, crate::ffmpeg::managed_path());
    }
    for lnk in shortcut_paths() {
        let _ = create_shortcut(&lnk, &target);
    }
    Ok(target)
}

pub fn set_autostart(on: bool) {
    unsafe {
        if on {
            let cmd = format!("\"{}\" --background", current_exe().display());
            let wide: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ.0,
                Some(wide.as_ptr() as _), (wide.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
    }
}

/// Removes shortcuts, autostart, settings and the program folder. Clips are kept.
pub fn uninstall() {
    set_autostart(false);
    for lnk in shortcut_paths() {
        let _ = std::fs::remove_file(lnk);
    }
    // The running exe (and the library's WebView files) can't be deleted while we run;
    // let cmd remove the folders a moment after we exit.
    let script = format!(
        "timeout /t 3 /nobreak >nul & rmdir /s /q \"{}\" & rmdir /s /q \"{}\"",
        config::data_dir().display(),
        install_dir().display()
    );
    use std::os::windows::process::CommandExt;
    // raw_arg: cmd needs the quotes exactly as written.
    let _ = std::process::Command::new("cmd").arg("/c").raw_arg(&script).creation_flags_no_window().spawn();
}

trait NoWindow {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

impl NoWindow for std::process::Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(crate::ffmpeg::CREATE_NO_WINDOW)
    }
}
