//! Game detection without a game database: the foreground app counts as a game when it
//! covers its whole monitor (fullscreen or borderless) and isn't on the ignore list, or
//! when it's on the user's game list.

use crate::config::Config;
use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow};
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PWSTR;

#[derive(Clone, Debug)]
pub struct Game {
    pub pid: u32,
    pub exe: String,
    /// Folder-safe display name.
    pub name: String,
    pub monitor: HMONITOR,
}

unsafe impl Send for Game {}

/// Install folders of the big game stores: anything launched from here is a game, even windowed.
const GAME_LIBRARIES: &[&str] = &[
    r"\steamapps\common\", r"\epic games\", r"\riot games\", r"\xboxgames\",
    r"\windowsapps\microsoft.minecraft", r"\gog galaxy\games\", r"\ubisoft game launcher\games\",
    r"\ea games\", r"\origin games\", r"\battle.net\games\", r"\rockstar games\",
];

/// Full lower-case exe path.
fn exe_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        ok.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(path.rsplit('\\').next()?.to_ascii_lowercase())
    }
}

pub fn alive(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code).is_ok();
        let _ = CloseHandle(h);
        ok && code == 259 // STILL_ACTIVE
    }
}

pub fn monitor_rect(m: HMONITOR) -> RECT {
    let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let _ = GetMonitorInfoW(m, &mut mi);
    }
    mi.rcMonitor
}

pub fn foreground_monitor() -> HMONITOR {
    unsafe { MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY) }
}

fn clean_name(title: &str, exe: &str) -> String {
    let stem = exe.trim_end_matches(".exe");
    let t: String = title
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { ' ' } else { c })
        .collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.trim_matches(|c: char| c == '.' || c == ' ');
    if t.is_empty() || t.len() > 48 { stem.to_string() } else { t.to_string() }
}

pub fn foreground_game(cfg: &Config) -> Option<Game> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() || hwnd == GetShellWindow() || hwnd == GetDesktopWindow() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 || pid == std::process::id() {
            return None;
        }
        let path = exe_path(pid)?;
        let exe = path.rsplit('\\').next()?.to_string();
        if cfg.ignored_apps.iter().any(|a| a.eq_ignore_ascii_case(&exe)) {
            return None;
        }
        let listed = cfg.game_apps.iter().any(|a| a.eq_ignore_ascii_case(&exe))
            || GAME_LIBRARIES.iter().any(|d| path.contains(d)) && big_enough(hwnd);
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
        if !listed && !covers_monitor(hwnd, monitor) {
            return None;
        }
        let mut title = [0u16; 256];
        let n = GetWindowTextW(hwnd, &mut title);
        let name = clean_name(&String::from_utf16_lossy(&title[..n as usize]), &exe);
        Some(Game { pid, exe, name, monitor })
    }
}

/// Skips splash screens and small helper windows of store-installed games.
unsafe fn big_enough(hwnd: HWND) -> bool {
    let mut r = RECT::default();
    unsafe { GetClientRect(hwnd, &mut r) }.is_ok() && r.right - r.left >= 800 && r.bottom - r.top >= 500
}

unsafe fn covers_monitor(hwnd: HWND, monitor: HMONITOR) -> bool {
    let mut r = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
        return false;
    }
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
    if style & WS_CAPTION.0 == WS_CAPTION.0 {
        return false; // maximized normal window, not a borderless game
    }
    let m = monitor_rect(monitor);
    r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
}

/// Index of `monitor` among the primary GPU's outputs, matching FFmpeg ddagrab's `output_idx`.
pub fn dda_output_index(monitor: HMONITOR) -> Option<u32> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let mut i = 0;
        while let Ok(output) = adapter.EnumOutputs(i) {
            if output.GetDesc().ok()?.Monitor == monitor {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}
