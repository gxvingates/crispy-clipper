//! The clip library window (WebView2). It runs as its own short-lived process, so the
//! background recorder never carries any UI memory.

use crate::config::{self, Config};
use crate::{ffmpeg, icon, install, mp4};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::WindowExtWindows;
use tao::window::{Icon, WindowBuilder};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};
use wry::http::{Request, Response, StatusCode, header};
use wry::{WebContext, WebViewBuilder, WebViewBuilderExtWindows};

const UI: &str = include_str!("ui.html");
const TITLE: &str = "Crispy Clipper";

enum UserEvent {
    Eval(String),
}

pub fn run() {
    unsafe {
        let _ = CreateMutexW(None, true, w!("Local\\CrispyClipperLibrary"));
        if windows::Win32::Foundation::GetLastError() == windows::Win32::Foundation::ERROR_ALREADY_EXISTS {
            if let Ok(h) = FindWindowW(None, &HSTRING::from(TITLE)) {
                let _ = ShowWindow(h, SW_RESTORE);
                let _ = SetForegroundWindow(h);
            }
            return;
        }
    }
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title(TITLE)
        .with_inner_size(LogicalSize::new(1240.0, 800.0))
        .with_min_inner_size(LogicalSize::new(760.0, 520.0))
        .with_window_icon(Icon::from_rgba(icon::rgba(64, true), 64, 64).ok())
        .build(&event_loop)
        .expect("window");
    let hwnd = window.hwnd();

    let mut ctx = WebContext::new(Some(config::data_dir().join("webview")));
    let webview = WebViewBuilder::new_with_web_context(&mut ctx)
        .with_url("http://app.localhost/")
        .with_background_color((17, 17, 21, 255))
        .with_devtools(cfg!(debug_assertions))
        .with_browser_accelerator_keys(false)
        // Lean WebView2: no background services, one renderer, GPU kept for video decoding.
        .with_additional_browser_args(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,SpareRendererForSitePerProcess,             AutofillServerCommunication,MediaRouter,OptimizationHints,Translate              --disable-background-networking --disable-component-update --disable-extensions --disable-sync              --no-pings --no-first-run --renderer-process-limit=1 --in-process-gpu --js-flags=--max-old-space-size=96",
        )
        .with_asynchronous_custom_protocol("app".into(), |_id, req, responder| {
            std::thread::spawn(move || responder.respond(serve(&req)));
        })
        .with_ipc_handler(move |req: Request<String>| handle_ipc(req.body(), &proxy, hwnd))
        .build(&window)
        .expect("WebView2 runtime missing? Install it from https://go.microsoft.com/fwlink/p/?LinkId=2124703");

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Eval(js)) => {
                let _ = webview.evaluate_script(&js);
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => std::process::exit(0),
            _ => {}
        }
    });
}

fn cfg() -> Config {
    Config::load().unwrap_or_else(|_| Config::parse("").unwrap())
}

// ---------- local "server" for the UI, clips and thumbnails ----------

fn query_param(req: &Request<Vec<u8>>, key: &str) -> Option<String> {
    let q = req.uri().query()?;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 3 <= b.len())
            .then(|| std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match (hex, b[i]) {
            (Some(v), _) => {
                out.push(v);
                i += 3;
            }
            (None, b'+') => {
                out.push(b' ');
                i += 1;
            }
            (None, c) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Only files inside the clips folder are served.
fn allowed(path: &Path) -> bool {
    let root = cfg().output_dir();
    match (path.canonicalize(), root.canonicalize()) {
        (Ok(p), Ok(r)) => p.starts_with(r),
        _ => false,
    }
}

fn respond(status: StatusCode, ctype: &str, body: Vec<u8>) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, ctype)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Cow::Owned(body))
        .unwrap()
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("mp4") => "video/mp4",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
}

fn serve(req: &Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    match req.uri().path() {
        "/" => respond(StatusCode::OK, "text/html; charset=utf-8", UI.as_bytes().to_vec()),
        "/media" => match query_param(req, "path").map(PathBuf::from) {
            Some(p) if allowed(&p) => serve_file(req, &p),
            _ => respond(StatusCode::FORBIDDEN, "text/plain", b"forbidden".to_vec()),
        },
        "/thumb" => match query_param(req, "path").map(PathBuf::from) {
            Some(p) if allowed(&p) => match thumbnail(&p) {
                // The URL carries the clip's mtime, so it's safe to cache forever.
                Some(t) => {
                    let mut r = serve_file(req, &t);
                    r.headers_mut().insert(header::CACHE_CONTROL, "max-age=31536000".parse().unwrap());
                    r
                }
                None => respond(StatusCode::NOT_FOUND, "text/plain", Vec::new()),
            },
            _ => respond(StatusCode::FORBIDDEN, "text/plain", b"forbidden".to_vec()),
        },
        _ => respond(StatusCode::NOT_FOUND, "text/plain", Vec::new()),
    }
}

/// Serves a file with HTTP Range support so the video player can seek.
fn serve_file(req: &Request<Vec<u8>>, path: &Path) -> Response<Cow<'static, [u8]>> {
    let Ok(mut f) = File::open(path) else {
        return respond(StatusCode::NOT_FOUND, "text/plain", Vec::new());
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| {
            let (a, b) = v.split_once('-')?;
            let start: u64 = a.parse().ok()?;
            let end: u64 = b.parse().unwrap_or(len.saturating_sub(1));
            Some((start, end.min(len.saturating_sub(1))))
        });
    let Some((start, end)) = range.or_else(|| (len > 8 << 20).then_some((0, len - 1))) else {
        let mut body = Vec::with_capacity(len as usize);
        let _ = f.read_to_end(&mut body);
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime(path))
            .header(header::ACCEPT_RANGES, "bytes")
            .body(Cow::Owned(body))
            .unwrap();
    };
    if start >= len {
        return respond(StatusCode::RANGE_NOT_SATISFIABLE, "text/plain", Vec::new());
    }
    let end = end.min(start + (4 << 20) - 1); // 4 MB per request keeps memory flat
    let mut body = vec![0u8; (end - start + 1) as usize];
    let _ = f.seek(SeekFrom::Start(start)).and_then(|_| f.read_exact(&mut body));
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_TYPE, mime(path))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
        .header(header::CONTENT_LENGTH, body.len())
        .body(Cow::Owned(body))
        .unwrap()
}

/// At most 3 thumbnail FFmpeg processes at once.
static SLOTS: (Mutex<u32>, Condvar) = (Mutex::new(0), Condvar::new());

fn thumbnail(path: &Path) -> Option<PathBuf> {
    let meta = path.metadata().ok()?;
    let mtime = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let key = format!("{}|{}|{mtime}", path.display(), meta.len());
    let hash = key.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    let dir = config::data_dir().join("thumbs");
    let out = dir.join(format!("{hash:016x}.jpg"));
    if out.exists() {
        return Some(out);
    }
    let _ = std::fs::create_dir_all(&dir);
    let ff = ffmpeg::locate(&cfg())?;
    {
        let mut n = SLOTS.0.lock().unwrap();
        while *n >= 3 {
            n = SLOTS.1.wait(n).unwrap();
        }
        *n += 1;
    }
    let is_video = mime(path) == "video/mp4";
    let mut cmd = ffmpeg::command(&ff);
    if is_video {
        cmd.args(["-ss", "1"]);
    }
    let ok = cmd
        .arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "5", "-y"])
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    *SLOTS.0.lock().unwrap() -= 1;
    SLOTS.1.notify_one();
    // Clips shorter than 1s: retry from the first frame.
    if !ok && is_video {
        let _ = ffmpeg::command(&ff)
            .arg("-i")
            .arg(path)
            .args(["-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "5", "-y"])
            .arg(&out)
            .status();
    }
    out.exists().then_some(out)
}

// ---------- commands from the UI ----------

fn handle_ipc(body: &str, proxy: &EventLoopProxy<UserEvent>, hwnd: isize) {
    let Ok(msg) = serde_json::from_str::<Value>(body) else { return };
    let id = msg["id"].as_u64().unwrap_or(0);
    let cmd = msg["cmd"].as_str().unwrap_or("").to_string();
    let args = msg["args"].clone();
    let reply = {
        let proxy = proxy.clone();
        move |res: Result<Value, String>| {
            let v = match res {
                Ok(v) => json!({ "ok": v }),
                Err(e) => json!({ "error": e }),
            };
            let _ = proxy.send_event(UserEvent::Eval(format!("window.__reply({id}, {v})")));
        }
    };
    // Dialogs need the UI thread; everything else runs in the background.
    match cmd.as_str() {
        "pickFolder" => reply(pick_folder(HWND(hwnd as _)).ok_or_else(|| "cancelled".into()).map(Value::from)),
        "copyFile" => reply(copy_file_to_clipboard(args["path"].as_str().unwrap_or("")).map(|_| Value::Null)),
        _ => {
            std::thread::spawn(move || reply(run_command(&cmd, &args)));
        }
    }
}

fn arg_path(args: &Value) -> Result<PathBuf, String> {
    let p = PathBuf::from(args["path"].as_str().ok_or("missing path")?);
    if allowed(&p) { Ok(p) } else { Err("That file isn't in your clips folder.".into()) }
}

fn run_command(cmd: &str, args: &Value) -> Result<Value, String> {
    match cmd {
        "list" => Ok(list_clips()),
        "status" => Ok(status()),
        "audioDevices" => {
            let (outputs, inputs) = crate::audio::list_devices();
            Ok(json!({ "outputs": outputs, "inputs": inputs }))
        }
        "config" => serde_json::to_value(cfg()).map_err(|e| e.to_string()),
        "defaults" => serde_json::to_value(Config::parse("").unwrap()).map_err(|e| e.to_string()),
        "saveConfig" => {
            let c: Config = serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
            c.save()?;
            if !install::is_dev_build() {
                install::set_autostart(c.start_with_windows);
            }
            if !recorder_running() {
                start_recorder();
            }
            Ok(Value::Null)
        }
        "startRecorder" => {
            start_recorder();
            Ok(Value::Null)
        }
        "trim" => {
            let p = arg_path(args)?;
            let start = args["start"].as_f64().unwrap_or(0.0);
            let end = args["end"].as_f64().ok_or("missing end")?;
            trim(&p, start, end, args["precise"].as_bool().unwrap_or(false), args["replace"].as_bool().unwrap_or(false))
                .map(|p| Value::from(p.to_string_lossy()))
        }
        "compress" => {
            let p = arg_path(args)?;
            compress(&p, args["mb"].as_f64().unwrap_or(10.0)).map(|p| Value::from(p.to_string_lossy()))
        }
        "rename" => {
            let p = arg_path(args)?;
            let name: String = args["name"]
                .as_str()
                .unwrap_or("")
                .chars()
                .filter(|c| !"<>:\"/\\|?*".contains(*c) && !c.is_control())
                .collect();
            let name = name.trim();
            if name.is_empty() {
                return Err("Name can't be empty".into());
            }
            let ext = p.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            let new = p.with_file_name(format!("{name}.{ext}"));
            if new.exists() {
                return Err("A file with that name already exists".into());
            }
            std::fs::rename(&p, &new).map_err(|e| e.to_string())?;
            Ok(Value::from(new.to_string_lossy()))
        }
        "delete" => {
            let paths: Vec<PathBuf> = args["paths"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(PathBuf::from)).collect())
                .unwrap_or_default();
            if paths.iter().any(|p| !allowed(p)) {
                return Err("Can only delete files in your clips folder".into());
            }
            recycle(&paths)?;
            Ok(Value::Null)
        }
        "reveal" => {
            let p = arg_path(args)?;
            let _ = std::process::Command::new("explorer").arg(format!("/select,{}", p.display())).spawn();
            Ok(Value::Null)
        }
        "openExternal" => {
            let p = arg_path(args)?;
            unsafe {
                ShellExecuteW(None, w!("open"), &HSTRING::from(p.as_os_str()), None, None, SW_SHOWNORMAL);
            }
            Ok(Value::Null)
        }
        "openFolder" => {
            let dir = cfg().output_dir();
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::process::Command::new("explorer").arg(dir).spawn();
            Ok(Value::Null)
        }
        "uninstall" => {
            stop_recorder();
            install::uninstall();
            std::process::exit(0);
        }
        _ => Err(format!("unknown command {cmd}")),
    }
}

fn list_clips() -> Value {
    let root = cfg().output_dir();
    let mut items = Vec::new();
    let mut add_dir = |dir: &Path, game: &str| {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !p.is_file() {
                continue;
            }
            let ext = p.extension().map(|x| x.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            let kind = match ext.as_str() {
                "mp4" if name.starts_with("Recording") => "recording",
                "mp4" => "clip",
                "png" | "jpg" => "screenshot",
                _ => continue,
            };
            let meta = e.metadata().ok();
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let info = (ext == "mp4").then(|| mp4::info(&p)).flatten();
            items.push(json!({
                "path": p.to_string_lossy(),
                "name": p.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default(),
                "game": game,
                "kind": kind,
                "size": meta.map(|m| m.len()).unwrap_or(0),
                "mtime": mtime,
                "duration": info.as_ref().map(|i| i.seconds).unwrap_or(0.0),
                "width": info.as_ref().map(|i| i.width).unwrap_or(0),
                "height": info.as_ref().map(|i| i.height).unwrap_or(0),
            }));
        }
    };
    add_dir(&root, "Other");
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() && !name.starts_with('.') {
                add_dir(&e.path(), &name);
            }
        }
    }
    items.sort_by(|a, b| b["mtime"].as_u64().cmp(&a["mtime"].as_u64()));
    json!({ "items": items, "root": root.to_string_lossy() })
}

fn recorder_running() -> bool {
    unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, w!("Local\\CrispyClipperRecorder")).is_ok() }
}

fn start_recorder() {
    let _ = std::process::Command::new(install::current_exe()).arg("--background").spawn();
}

fn stop_recorder() {
    let _ = std::process::Command::new("taskkill")
        .args(["/f", "/im", install::current_exe().file_name().unwrap().to_str().unwrap_or(install::EXE_NAME)])
        .args(["/fi", &format!("PID ne {}", std::process::id())])
        .creation_flags_hidden()
        .status();
}

trait Hidden {
    fn creation_flags_hidden(&mut self) -> &mut Self;
}
impl Hidden for std::process::Command {
    fn creation_flags_hidden(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(ffmpeg::CREATE_NO_WINDOW)
    }
}

fn status() -> Value {
    let c = cfg();
    json!({
        "running": recorder_running(),
        "ffmpeg": ffmpeg::locate(&c).map(|p| p.to_string_lossy().to_string()),
        "installed": install::is_installed_copy(),
        "version": env!("CARGO_PKG_VERSION"),
        "settingsPath": config::config_path().to_string_lossy(),
        "logPath": config::data_dir().join("log.txt").to_string_lossy(),
    })
}

fn unique(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
    (2..).map(|i| path.with_file_name(format!("{stem} {i}.{ext}"))).find(|p| !p.exists()).unwrap()
}

/// Tries each encoder until one works on this PC (hardware first).
fn encode(input: &Path, pre: &[String], post: &[String], video: impl Fn(&str) -> Vec<String>, out: &Path) -> Result<(), String> {
    let c = cfg();
    let ff = ffmpeg::locate(&c).ok_or("FFmpeg not found")?;
    let mut last = String::new();
    for enc in ffmpeg::encoder_candidates(&c) {
        let mut args: Vec<String> = pre.to_vec();
        args.extend(["-i".into(), input.to_string_lossy().into()]);
        args.extend(post.iter().cloned());
        args.extend(video(&enc));
        args.extend(["-c:a", "aac", "-b:a", "160k", "-movflags", "+faststart", "-y"].map(String::from));
        args.push(out.to_string_lossy().into());
        match ffmpeg::test(&ff, &args) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    let _ = std::fs::remove_file(out);
    Err(last)
}

fn trim(path: &Path, start: f64, end: f64, precise: bool, replace: bool) -> Result<PathBuf, String> {
    if end <= start {
        return Err("The end must be after the start".into());
    }
    let c = cfg();
    let ff = ffmpeg::locate(&c).ok_or("FFmpeg not found")?;
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    let out = if replace {
        path.with_file_name(format!(".{stem}.trim.mp4"))
    } else {
        unique(path.with_file_name(format!("{stem} (trimmed).mp4")))
    };
    let (ss, t) = (format!("{start:.3}"), format!("{:.3}", end - start));
    if precise {
        let pre = vec!["-ss".to_string(), ss];
        let post = vec!["-t".to_string(), t, "-map".into(), "0".into(), "-pix_fmt".into(), "yuv420p".into()];
        encode(path, &pre, &post, |enc| ffmpeg::encoder_args(enc, c.quality.min(20), c.max_bitrate_mbps, c.fps), &out)?;
    } else {
        let args: Vec<String> = vec![
            "-ss".into(), ss, "-i".into(), path.to_string_lossy().into(), "-t".into(), t, "-map".into(), "0".into(),
            "-c".into(), "copy".into(), "-avoid_negative_ts".into(), "make_zero".into(), "-movflags".into(),
            "+faststart".into(), "-y".into(), out.to_string_lossy().into(),
        ];
        ffmpeg::test(&ff, &args)?;
    }
    if replace {
        recycle(&[path.to_path_buf()])?; // the untrimmed original goes to the Recycle Bin
        std::fs::rename(&out, path).map_err(|e| e.to_string())?;
        return Ok(path.to_path_buf());
    }
    Ok(out)
}

/// Re-encodes a copy that fits under `mb` megabytes (Discord's free limit is 10 MB).
fn compress(path: &Path, mb: f64) -> Result<PathBuf, String> {
    let info = mp4::info(path).ok_or("Couldn't read the clip")?;
    if info.seconds <= 0.0 {
        return Err("Couldn't read the clip length".into());
    }
    let audio_kbps = 128.0;
    let video_kbps = ((mb * 8.0 * 1000.0 * 0.92) / info.seconds - audio_kbps).floor();
    if video_kbps < 300.0 {
        return Err(format!("{:.0}s is too long to fit in {mb} MB. Trim it first.", info.seconds));
    }
    let height = if video_kbps < 2500.0 { 720 } else if video_kbps < 8000.0 { 1080 } else { 1440 };
    let mut post = vec!["-map".to_string(), "0".into()];
    if info.height > height {
        post.extend(["-vf".into(), format!("scale=-2:{height}:flags=bicubic,format=yuv420p")]);
    } else {
        post.extend(["-pix_fmt".into(), "yuv420p".into()]);
    }
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    let out = unique(path.with_file_name(format!("{stem} ({mb:.0}MB).mp4")));
    let pre = vec!["-hwaccel".to_string(), "auto".into()];
    encode(path, &pre, &post, |enc| {
        let rate = format!("{video_kbps}k");
        let mut a = vec!["-c:v".to_string(), enc.to_string()];
        if enc.ends_with("_nvenc") {
            a.extend(["-preset", "p6", "-tune", "hq", "-rc", "cbr", "-spatial-aq", "1"].map(String::from));
        } else if enc == "libx264" {
            a.extend(["-preset", "medium"].map(String::from));
        }
        a.extend(["-b:v".into(), rate.clone(), "-maxrate".into(), rate, "-bufsize".into(), format!("{video_kbps}k")]);
        a
    }, &out)?;
    Ok(out)
}

fn recycle(paths: &[PathBuf]) -> Result<(), String> {
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        from.extend(p.as_os_str().encode_wide_vec());
        from.push(0);
    }
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r == 0 { Ok(()) } else { Err(format!("Couldn't delete (error {r})")) }
}

trait WideVec {
    fn encode_wide_vec(&self) -> Vec<u16>;
}
impl WideVec for std::ffi::OsStr {
    fn encode_wide_vec(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().collect()
    }
}

fn copy_file_to_clipboard(path: &str) -> Result<(), String> {
    let p = PathBuf::from(path);
    if !allowed(&p) {
        return Err("not in clips folder".into());
    }
    let mut wide = p.as_os_str().encode_wide_vec();
    wide.extend([0, 0]);
    let header = size_of::<DROPFILES>();
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, header + wide.len() * 2).map_err(|e| e.to_string())?;
        let ptr = GlobalLock(mem) as *mut u8;
        let df = DROPFILES { pFiles: header as u32, fWide: true.into(), ..Default::default() };
        std::ptr::write_unaligned(ptr as *mut DROPFILES, df);
        std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, ptr.add(header), wide.len() * 2);
        let _ = GlobalUnlock(mem);
        OpenClipboard(None).map_err(|e| e.to_string())?;
        let _ = EmptyClipboard();
        let res = SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(mem.0)));
        let _ = CloseClipboard();
        res.map(|_| ()).map_err(|e| e.to_string())
    }
}

fn pick_folder(owner: HWND) -> Option<String> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dlg.SetOptions(dlg.GetOptions().ok()? | FOS_PICKFOLDERS).ok()?;
        dlg.Show(Some(owner)).ok()?;
        let item = dlg.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as _));
        s
    }
}
