#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod ffmpeg;
mod games;
mod hotkey;
mod icon;
mod install;
mod library;
mod mp4;
mod notify;
mod recorder;
mod ring;

use config::Config;
use recorder::{Event, Recorder};
use std::os::windows::io::AsRawHandle;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LPARAM, WPARAM};
use windows::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentThreadId, INFINITE, OpenEventW, SetEvent, WaitForSingleObject};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

const WM_EVENT: u32 = WM_APP + 1;
const WM_MENU: u32 = WM_APP + 2;
const WM_RELOAD: u32 = WM_APP + 3;
const WM_OPEN: u32 = WM_APP + 4;

const MENU: [&str; 6] = ["open", "clip", "record", "shot", "pause", "quit"];

pub fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

pub fn message_box(text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(None, &HSTRING::from(text), w!("Crispy Clipper"), style) }
}

/// Single instance for the background recorder. With `wait`, waits for a restarting predecessor.
fn take_instance_lock(wait: bool) -> bool {
    for _ in 0..if wait { 50 } else { 1 } {
        unsafe {
            if let Ok(h) = CreateMutexW(None, true, w!("Local\\CrispyClipperRecorder")) {
                if GetLastError() != ERROR_ALREADY_EXISTS {
                    return true; // handle stays open for the life of the process
                }
                let _ = CloseHandle(h);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// In release builds there's no console, so send our log lines to a file.
fn log_to_file() {
    if cfg!(debug_assertions) {
        return;
    }
    let path = config::data_dir().join("log.txt");
    let _ = std::fs::create_dir_all(config::data_dir());
    let big = std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false);
    let file = std::fs::OpenOptions::new().create(true).append(!big).write(true).truncate(big).open(&path);
    if let Ok(f) = file {
        unsafe {
            let _ = SetStdHandle(STD_ERROR_HANDLE, HANDLE(f.as_raw_handle()));
        }
        std::mem::forget(f);
    }
}

fn open_library() {
    let _ = std::process::Command::new(install::current_exe()).arg("--library").spawn();
}

fn settings_mtime() -> Option<SystemTime> {
    std::fs::metadata(config::config_path()).and_then(|m| m.modified()).ok()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let has = |a: &str| args.iter().any(|x| x == a);
    // Real pixel sizes for monitor detection, downscaling and the popup on scaled displays.
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    if has("--library") {
        let view = args.iter().position(|a| a == "--view").and_then(|i| args.get(i + 1)).cloned();
        return library::run(view);
    }
    if has("--install") {
        // Silent install (no prompt), then launch the installed copy.
        match install::install() {
            Ok(target) => {
                let _ = std::process::Command::new(target).spawn();
            }
            Err(e) => {
                message_box(&format!("Install failed: {e}"), MB_ICONERROR);
            }
        }
        return;
    }
    // `--clip` / `--record` / `--screenshot`: trigger a running recorder (Stream Deck, macros).
    for (flag, name) in [("--clip", w!("Local\\CrispyClipperClip")), ("--record", w!("Local\\CrispyClipperRecord")), ("--screenshot", w!("Local\\CrispyClipperShot"))] {
        if has(flag) {
            unsafe {
                if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, name) {
                    let _ = SetEvent(ev);
                }
            }
            return;
        }
    }
    let background = has("--background");
    log_to_file();
    // Release builds abort on panic: get the reason into the log first.
    std::panic::set_hook(Box::new(|info| eprintln!("[panic] {} {info}", timestamp())));

    if !take_instance_lock(has("--replace")) {
        if !background {
            open_library(); // already running: double-clicking the app opens the library
        }
        return;
    }

    // First launch of a downloaded copy: offer to install.
    let portable_flag = config::data_dir().join("portable");
    if !background && !install::is_installed_copy() && !install::is_dev_build() && !portable_flag.exists() {
        let r = message_box(
            "Install Crispy Clipper?\n\nIt will be added to your Start menu and desktop and start with Windows \
             (you can turn that off in Settings).\n\nChoose No to just run it from here.",
            MB_YESNOCANCEL | MB_ICONQUESTION,
        );
        if r == IDCANCEL {
            return;
        }
        if r == IDYES {
            match install::install() {
                Ok(target) => {
                    // Hand over to the installed copy (it waits for our lock to go away).
                    let _ = std::process::Command::new(target).arg("--replace").spawn();
                    return;
                }
                Err(e) => {
                    message_box(&format!("Install failed: {e}"), MB_ICONERROR);
                }
            }
        } else {
            let _ = std::fs::create_dir_all(config::data_dir());
            let _ = std::fs::write(&portable_flag, "");
        }
    }

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            message_box(&format!("Your settings file has an error:\n{e}\n\nFix it or delete it to reset."), MB_ICONERROR);
            return;
        }
    };

    // Keep our own FFmpeg current (the user already agreed to download it once).
    if ffmpeg::managed_outdated() {
        match ffmpeg::download() {
            Ok(_) => eprintln!("[app] updated ffmpeg to build {}", ffmpeg::MANAGED_VERSION),
            Err(e) => eprintln!("[app] ffmpeg update failed: {e}"),
        }
    }
    // Prefer our own tested FFmpeg build; any other copy is only a fallback.
    let declined = config::data_dir().join("ffmpeg-declined");
    let fallback = ffmpeg::locate(&cfg);
    let ffmpeg_path = if ffmpeg::managed_path().exists() || install::is_dev_build() && fallback.is_some() {
        fallback.unwrap()
    } else if declined.exists() && fallback.is_some() {
        fallback.unwrap()
    } else {
        let r = message_box(
            "Crispy Clipper needs to download its video encoder (a minimal FFmpeg build, about 9 MB) \
             from github.com/gxvingates/crispy-clipper.\n\nDownload it now?",
            MB_OKCANCEL | MB_ICONINFORMATION,
        );
        match (r == IDOK).then(ffmpeg::download) {
            Some(Ok(p)) => p,
            Some(Err(e)) if fallback.is_none() => {
                message_box(&format!("Download failed:\n{e}\n\nCheck your internet connection and try again."), MB_ICONERROR);
                return;
            }
            None if fallback.is_none() => return,
            res => {
                if let Some(Err(e)) = res {
                    eprintln!("[app] ffmpeg download failed: {e}");
                } else {
                    let _ = std::fs::write(&declined, "");
                }
                fallback.unwrap()
            }
        }
    };
    eprintln!("[app] {} starting, ffmpeg {}", timestamp(), ffmpeg_path.display());

    if !install::is_dev_build() {
        install::set_autostart(cfg.start_with_windows);
    }

    let tid = unsafe { GetCurrentThreadId() };
    let post = move |msg: u32, wp: usize| unsafe {
        let _ = PostThreadMessageW(tid, msg, WPARAM(wp), LPARAM(0));
    };
    let (tx, rx) = mpsc::channel();
    let rec = Recorder::new(cfg.clone(), ffmpeg_path, tx, move || post(WM_EVENT, 0));
    {
        let rec = rec.clone();
        std::thread::spawn(move || rec.run());
    }

    // Settings saved (the library saves as you change things): volumes apply live, like
    // Medal's sliders; anything else restarts the recorder with the new settings.
    {
        let (rec, mut current) = (rec.clone(), cfg.clone());
        std::thread::spawn(move || {
            let mut seen = settings_mtime();
            loop {
                std::thread::sleep(Duration::from_millis(500));
                if settings_mtime() == seen {
                    continue;
                }
                std::thread::sleep(Duration::from_millis(150)); // let the write finish
                seen = settings_mtime();
                match Config::load() {
                    Ok(new) if new == current => {}
                    Ok(new) if current.only_volumes_differ(&new) => {
                        rec.set_volumes(&new);
                        current = new;
                        eprintln!("[app] volumes updated");
                    }
                    Ok(_) => {
                        post(WM_RELOAD, 0);
                        break;
                    }
                    Err(e) => eprintln!("[app] settings not reloaded: {e}"),
                }
            }
        });
    }

    // Named events for `--clip` etc.: each wakes a thread that posts the matching hotkey id.
    for (id, name) in [(1usize, w!("Local\\CrispyClipperClip")), (2, w!("Local\\CrispyClipperRecord")), (3, w!("Local\\CrispyClipperShot"))] {
        if let Ok(ev) = unsafe { CreateEventW(None, false, false, name) } {
            let ev = ev.0 as usize;
            std::thread::spawn(move || loop {
                unsafe {
                    WaitForSingleObject(HANDLE(ev as _), INFINITE);
                    let _ = PostThreadMessageW(tid, WM_HOTKEY, WPARAM(id), LPARAM(0));
                }
            });
        }
    }

    let hotkeys = [(1, &cfg.clip_hotkey), (2, &cfg.record_hotkey), (3, &cfg.screenshot_hotkey)];
    let mut bad = Vec::new();
    for (id, key) in hotkeys {
        if key.trim().is_empty() {
            continue;
        }
        match hotkey::parse(key) {
            Some((mods, vk)) if unsafe { RegisterHotKey(None, id, mods, vk) }.is_ok() => {}
            Some(_) => bad.push(format!("{key} (used by another app)")),
            None => bad.push(format!("{key} (not a valid key)")),
        }
    }
    if !bad.is_empty() {
        message_box(&format!("Couldn't set hotkeys: {}\nChange them in Settings.", bad.join(", ")), MB_ICONWARNING);
    }

    let menu = Menu::new();
    let item = |id: &str, text: String| MenuItem::with_id(id, text, true, None);
    let open = item("open", "Open Crispy Clipper".into());
    let clip = item("clip", format!("Save last {}s\t{}", cfg.clip_seconds, cfg.clip_hotkey));
    let record = item("record", format!("Start recording\t{}", cfg.record_hotkey));
    let shot = item("shot", format!("Screenshot\t{}", cfg.screenshot_hotkey));
    let pause = CheckMenuItem::with_id("pause", "Pause capturing", true, false, None);
    let quit = item("quit", "Quit".into());
    let sep = PredefinedMenuItem::separator();
    menu.append_items(&[&open, &sep, &clip, &record, &shot, &pause, &PredefinedMenuItem::separator(), &quit]).unwrap();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Some(i) = MENU.iter().position(|m| *m == e.id.0) {
            post(WM_MENU, i);
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            post(WM_OPEN, 0);
        }
    }));
    let icon = |on: bool| Icon::from_rgba(icon::rgba(32, on), 32, 32).unwrap();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Crispy Clipper")
        .with_icon(icon(false))
        .build()
        .expect("tray icon");
    let toast = notify::Toast::new();
    let notify = |text: &str, ok: bool, sound: bool| {
        if cfg.notify_overlay {
            if let Some(t) = &toast {
                t.show(text, ok);
            }
        }
        if cfg.notify_sound && sound {
            unsafe {
                let _ = MessageBeep(if ok { MB_OK } else { MB_ICONHAND });
            }
        }
    };

    if !background {
        open_library();
    }
    unsafe { SetTimer(None, 0, 2000, None) }; // refreshes the tray status
    let mut shown_encoder_error = false;
    let mut last_capturing = None;
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        match msg.message {
            WM_HOTKEY => match msg.wParam.0 {
                1 => rec.save_clip(),
                2 => rec.toggle_recording(),
                3 => rec.screenshot(),
                _ => {}
            },
            WM_MENU => match MENU[msg.wParam.0] {
                "open" => open_library(),
                "clip" => rec.save_clip(),
                "record" => rec.toggle_recording(),
                "shot" => rec.screenshot(),
                "pause" => rec.paused.store(pause.is_checked(), std::sync::atomic::Ordering::SeqCst),
                _ => break,
            },
            WM_OPEN => open_library(),
            WM_EVENT => {
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        Event::ClipSaved(p) => {
                            eprintln!("[clip] {}", p.display());
                            notify(&format!("Clip saved · last {}s", cfg.clip_seconds), true, true);
                        }
                        Event::RecordingStarted => notify("Recording started", true, true),
                        Event::RecordingSaved(p) => {
                            eprintln!("[recording] {}", p.display());
                            notify("Recording saved", true, true);
                        }
                        Event::Screenshot(p) => {
                            eprintln!("[screenshot] {}", p.display());
                            notify("Screenshot saved", true, true);
                        }
                        Event::GameDetected(name) => {
                            eprintln!("[game] {name} ({})", rec.game_exe().unwrap_or_default());
                            notify(&format!("{name} · {} to clip", cfg.clip_hotkey), true, false);
                        }
                        Event::Failed(e) => {
                            eprintln!("[error] {e}");
                            if e.starts_with("No working video encoder") {
                                if !shown_encoder_error {
                                    shown_encoder_error = true;
                                    message_box(&format!("Crispy Clipper can't record on this PC.\n\n{e}"), MB_ICONERROR);
                                }
                            } else {
                                notify(e.lines().next().unwrap_or(&e), false, true);
                            }
                        }
                    }
                }
                last_capturing = None; // refresh tray
                unsafe { let _ = PostThreadMessageW(tid, WM_TIMER, WPARAM(0), LPARAM(0)); }
            }
            WM_TIMER if msg.hwnd.is_invalid() => {
                let capturing = rec.is_capturing();
                let recording = rec.is_recording();
                let _ = record.set_text(format!(
                    "{} recording\t{}",
                    if recording { "Stop" } else { "Start" },
                    cfg.record_hotkey
                ));
                if last_capturing != Some((capturing, recording)) {
                    last_capturing = Some((capturing, recording));
                    let _ = tray.set_icon(Some(icon(capturing)));
                    let state = match (recording, capturing, rec.game_name()) {
                        (true, _, _) => "Recording".to_string(),
                        (_, true, Some(g)) => format!("Capturing {g}"),
                        (_, true, None) => "Capturing desktop".to_string(),
                        _ if rec.paused.load(std::sync::atomic::Ordering::SeqCst) => "Paused".to_string(),
                        _ => "Waiting for a game".to_string(),
                    };
                    let _ = tray.set_tooltip(Some(format!("Crispy Clipper: {state}")));
                }
            }
            WM_RELOAD => {
                let _ = std::process::Command::new(install::current_exe()).args(["--background", "--replace"]).spawn();
                break;
            }
            _ => unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
    }
    // FFmpeg exits on its own once our pipes close.
    let _ = PCWSTR::null();
    std::process::exit(0);
}
