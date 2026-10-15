//! Capture supervisor: runs FFmpeg (DXGI desktop duplication -> hardware encoder -> MPEG-TS
//! on stdout) only while it's wanted, and turns the replay ring into clips, recordings
//! and screenshots.

use crate::audio::{Mixer, RATE};
use crate::config::Config;
use crate::ffmpeg;
use crate::games::{self, Game};
use crate::ring::{self, Ring};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

pub enum Event {
    ClipSaved(PathBuf),
    RecordingStarted,
    RecordingSaved(PathBuf),
    Screenshot(PathBuf),
    GameDetected(String),
    Failed(String),
}

/// A capture pipeline that passed a self-test on this PC.
#[derive(Clone)]
struct Pipeline {
    output: u32,
    encoder: String,
    filter: Option<String>,
}

pub struct Recorder {
    pub cfg: Config,
    ffmpeg: PathBuf,
    ring: Mutex<Ring>,
    mixer: Mixer,
    game: Mutex<Option<Game>>,
    pub paused: AtomicBool,
    recording: Mutex<Option<PathBuf>>,
    busy_clip: AtomicBool,
    pipeline: Mutex<Option<Pipeline>>,
    wake: (Mutex<bool>, Condvar),
    events: Mutex<mpsc::Sender<Event>>,
    on_event: Box<dyn Fn() + Send + Sync>,
}

fn output_size(idx: u32) -> Option<(u32, u32)> {
    unsafe {
        let f: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let d = f.EnumAdapters1(0).ok()?.EnumOutputs(idx).ok()?.GetDesc().ok()?;
        let r = d.DesktopCoordinates;
        let (w, h) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
        // Rotated outputs still capture in panel orientation.
        Some(if d.Rotation.0 == 2 || d.Rotation.0 == 4 { (h, w) } else { (w, h) })
    }
}

impl Recorder {
    pub fn new(cfg: Config, ffmpeg: PathBuf, events: mpsc::Sender<Event>, on_event: impl Fn() + Send + Sync + 'static) -> Arc<Recorder> {
        let mixer = Mixer::new(&cfg);
        Arc::new(Recorder {
            ring: Mutex::new(Ring::new(Duration::from_secs(cfg.clip_seconds as u64 + 5))),
            cfg,
            ffmpeg,
            mixer,
            game: Mutex::new(None),
            paused: AtomicBool::new(false),
            recording: Mutex::new(None),
            busy_clip: AtomicBool::new(false),
            pipeline: Mutex::new(None),
            wake: (Mutex::new(false), Condvar::new()),
            events: Mutex::new(events),
            on_event: Box::new(on_event),
        })
    }

    fn emit(&self, e: Event) {
        let _ = self.events.lock().unwrap().send(e);
        (self.on_event)();
    }

    fn poke(&self) {
        *self.wake.0.lock().unwrap() = true;
        self.wake.1.notify_one();
    }

    pub fn is_capturing(&self) -> bool {
        self.ring.lock().unwrap().seconds_buffered() > 0
    }

    pub fn is_recording(&self) -> bool {
        self.recording.lock().unwrap().is_some()
    }

    pub fn game_name(&self) -> Option<String> {
        self.game.lock().unwrap().as_ref().map(|g| g.name.clone())
    }

    pub fn game_exe(&self) -> Option<String> {
        self.game.lock().unwrap().as_ref().map(|g| g.exe.clone())
    }

    fn folder(&self) -> PathBuf {
        let name = self.game_name().unwrap_or_else(|| "Desktop".into());
        self.cfg.output_dir().join(name)
    }

    fn wanted(&self) -> bool {
        !self.paused.load(Ordering::SeqCst)
            && (self.cfg.capture_mode == "always" || self.game.lock().unwrap().is_some() || self.is_recording())
    }

    fn output_index(&self) -> u32 {
        if self.cfg.monitor >= 0 {
            return self.cfg.monitor as u32;
        }
        let m = self.game.lock().unwrap().as_ref().map(|g| g.monitor).unwrap_or_else(games::foreground_monitor);
        games::dda_output_index(m).unwrap_or(0)
    }

    fn grab(&self, output: u32, fps: u32) -> String {
        format!("ddagrab=output_idx={output}:framerate={fps}:draw_mouse={}", self.cfg.show_cursor as u8)
    }

    /// Video filter variants to try, best first.
    fn filters(&self, output: u32, encoder: &str) -> Vec<Option<String>> {
        let scale = match (self.cfg.target_height(), output_size(output)) {
            (Some(th), Some((w, h))) if th < h => Some(((w * th / h + 1) & !1, th)),
            _ => None,
        };
        let mut v = Vec::new();
        if ffmpeg::takes_d3d11(encoder) {
            // (FFmpeg 8.1's scale_d3d11 can't allocate textures on ddagrab's device, so
            // downscaling goes through the CPU.)
            if let Some((w, h)) = scale {
                v.push(Some(format!("hwdownload,format=bgra,scale={w}:{h}:flags=bilinear,format=nv12")));
            }
            v.push(None);
        } else {
            let s = scale.map(|(w, h)| format!(",scale={w}:{h}:flags=bilinear")).unwrap_or_default();
            v.push(Some(format!("hwdownload,format=bgra{s},format=nv12")));
        }
        v
    }

    /// Splits the interleaved mixer output into one stereo track each (full mix first),
    /// named like Medal's ("All Audio", "Game", "Mic", "Discord").
    fn audio_maps(&self) -> Vec<String> {
        let tracks = &self.mixer.tracks;
        let mut a: Vec<String> = vec!["-map".into(), "0:v".into()];
        if tracks.len() == 1 {
            a.extend(["-map".into(), "1:a".into()]);
        } else {
            let outs: String = (0..tracks.len()).map(|i| format!("[s{i}]")).collect();
            let mut graph = format!("[1:a]asplit={}{outs}", tracks.len());
            for i in 0..tracks.len() {
                graph += &format!(";[s{i}]pan=stereo|c0=c{}|c1=c{}[a{i}]", 2 * i, 2 * i + 1);
            }
            a.extend(["-filter_complex".into(), graph]);
            for i in 0..tracks.len() {
                a.extend(["-map".into(), format!("[a{i}]")]);
            }
        }
        for (i, name) in tracks.iter().enumerate() {
            a.extend([format!("-metadata:s:a:{i}"), format!("title={name}")]);
            a.extend([format!("-metadata:s:a:{i}"), format!("handler_name={name}")]);
        }
        a
    }

    /// New volumes take effect immediately; nothing else changes.
    pub fn set_volumes(&self, cfg: &Config) {
        self.mixer.set_volumes(cfg);
    }

    fn video_args(&self, p: &Pipeline) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(f) = &p.filter {
            a.extend(["-vf".to_string(), f.clone()]);
        }
        a.extend(ffmpeg::encoder_args(&p.encoder, self.cfg.quality, self.cfg.max_bitrate_mbps, self.cfg.fps));
        a
    }

    /// Finds an encoder + filter that works on this PC (cached until the monitor changes).
    fn pipeline(&self, output: u32) -> Result<Pipeline, String> {
        if let Some(p) = self.pipeline.lock().unwrap().clone().filter(|p| p.output == output) {
            return Ok(p);
        }
        let mut last = String::from("no encoders to try");
        for encoder in ffmpeg::encoder_candidates(&self.cfg) {
            for filter in self.filters(output, &encoder) {
                let p = Pipeline { output, encoder: encoder.clone(), filter };
                let mut args: Vec<String> =
                    ["-f", "lavfi", "-i", &self.grab(output, self.cfg.fps), "-frames:v", "5"].map(String::from).into();
                args.extend(self.video_args(&p));
                args.extend(["-f", "null", "-"].map(String::from));
                match ffmpeg::test(&self.ffmpeg, &args) {
                    Ok(()) => {
                        eprintln!("[recorder] using {} {:?}", p.encoder, p.filter);
                        *self.pipeline.lock().unwrap() = Some(p.clone());
                        return Ok(p);
                    }
                    Err(e) => last = format!("{encoder}: {e}"),
                }
            }
        }
        Err(format!("No working video encoder. Last error: {last}"))
    }

    fn spawn(self: &Arc<Self>, p: &Pipeline) -> std::io::Result<Child> {
        let mut cmd = ffmpeg::command(&self.ffmpeg);
        cmd.args(["-f", "lavfi", "-i", &self.grab(p.output, self.cfg.fps)])
            .args(["-f", "f32le", "-ar", &RATE.to_string(), "-ac", &(2 * self.mixer.tracks.len()).to_string()])
            .args(["-thread_queue_size", "1024", "-probesize", "32", "-analyzeduration", "0", "-i", "pipe:0"])
            .args(self.audio_maps())
            .args(self.video_args(p))
            .args(["-c:a", "aac", "-b:a", "160k"])
            .args(["-mpegts_pmt_start_pid", &ring::PMT_PID.to_string()])
            .args(["-mpegts_start_pid", &ring::VIDEO_PID.to_string()])
            .args(["-flush_packets", "1", "-muxdelay", "0", "-f", "mpegts", "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;

        // Audio timestamps count from here, video from FFmpeg's first captured frame,
        // which comes later (encoder start-up). The gap is measured below and removed on save.
        let started = Instant::now();
        let stdin = child.stdin.take().unwrap();
        let me = self.clone();
        thread::spawn(move || me.mixer.pump(stdin));
        let stderr = child.stderr.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[ffmpeg] {line}");
            }
        });
        let mut out = child.stdout.take().unwrap();
        let me = self.clone();
        thread::spawn(move || {
            let mut buf = vec![0u8; 188 * 512];
            while let Ok(n @ 1..) = out.read(&mut buf) {
                let mut ring = me.ring.lock().unwrap();
                ring.push(&buf[..n]);
                if ring.has_video && ring.av_offset.is_none() {
                    let gap = started.elapsed().as_secs_f64() + AV_LATENCY_FIX;
                    eprintln!("[recorder] audio/video offset {:.0} ms", gap * 1000.0);
                    ring.av_offset = Some(gap);
                }
            }
        });
        Ok(child)
    }

    /// Main loop: tracks the running game and starts/stops FFmpeg as needed.
    pub fn run(self: Arc<Self>) {
        let mut child: Option<(Child, u32)> = None;
        let mut retry_at = Instant::now();
        let mut failures = 0u32;
        loop {
            {
                let (lock, cv) = &self.wake;
                let mut woke = cv.wait_timeout(lock.lock().unwrap(), Duration::from_millis(1000)).unwrap().0;
                *woke = false;
            }

            // Track the game: keep it while its process lives, otherwise look at the foreground app.
            let detected = {
                let mut game = self.game.lock().unwrap();
                if game.as_ref().is_some_and(|g| !games::alive(g.pid)) {
                    *game = None;
                }
                match game.is_none().then(|| games::foreground_game(&self.cfg)).flatten() {
                    Some(g) => {
                        let name = g.name.clone();
                        *game = Some(g);
                        Some(name)
                    }
                    None => None,
                }
            };
            if let Some(name) = detected {
                self.emit(Event::GameDetected(name));
            }

            let want = self.wanted();
            let output = if want { self.output_index() } else { 0 };

            if let Some((c, out)) = &mut child {
                let exited = matches!(c.try_wait(), Ok(Some(_)) | Err(_));
                if exited || !want || *out != output {
                    let _ = c.kill();
                    let _ = c.wait();
                    child = None;
                    self.ring.lock().unwrap().clear();
                    if exited && want {
                        failures += 1;
                        retry_at = Instant::now() + Duration::from_secs((2u64 << failures.min(4)).min(30));
                        *self.pipeline.lock().unwrap() = None; // re-test (e.g. GPU driver changed)
                    }
                }
            }
            if want && child.is_none() && Instant::now() >= retry_at {
                match self.pipeline(output).and_then(|p| self.spawn(&p).map_err(|e| e.to_string())) {
                    Ok(c) => {
                        eprintln!("[recorder] capturing output {output}");
                        child = Some((c, output));
                        failures = 0;
                    }
                    Err(e) => {
                        failures += 1;
                        retry_at = Instant::now() + Duration::from_secs(30);
                        self.emit(Event::Failed(e));
                    }
                }
            }
            if !want && failures > 0 {
                failures = 0;
            }
        }
    }

    pub fn save_clip(self: &Arc<Self>) {
        if self.busy_clip.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        thread::spawn(move || {
            let res = me.cut(me.cfg.clip_seconds);
            me.busy_clip.store(false, Ordering::SeqCst);
            me.emit(match res {
                Ok(p) => Event::ClipSaved(p),
                Err(e) => Event::Failed(e),
            });
        });
    }

    fn cut(&self, secs: u32) -> Result<PathBuf, String> {
        let (data, offset) = {
            let ring = self.ring.lock().unwrap();
            (ring.snapshot(secs), ring.av_offset.unwrap_or(0.0))
        };
        let data = data.ok_or(if self.wanted() {
            "Nothing buffered yet"
        } else {
            "Not capturing: no game detected (switch Capture to \"Always\" in settings to clip the desktop)"
        })?;
        let dir = self.folder();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let stamp = crate::timestamp();
        let ts = dir.join(format!(".clip {stamp}.ts"));
        std::fs::write(&ts, data).map_err(|e| e.to_string())?;
        let mp4 = dir.join(format!("Clip {stamp}.mp4"));
        let res = remux(&self.ffmpeg, &ts, &mp4, &self.mixer.tracks, offset);
        let _ = std::fs::remove_file(&ts);
        res.map(|_| mp4)
    }

    pub fn toggle_recording(self: &Arc<Self>) {
        let mut rec = self.recording.lock().unwrap();
        if let Some(ts) = rec.take() {
            drop(rec);
            let offset = {
                let mut ring = self.ring.lock().unwrap();
                let _ = ring.stop_recording();
                ring.av_offset.unwrap_or(0.0)
            };
            let me = self.clone();
            thread::spawn(move || {
                let mp4 = ts.with_file_name(
                    ts.file_name().unwrap().to_string_lossy().trim_start_matches('.').replace(".ts", ".mp4"),
                );
                let res = remux(&me.ffmpeg, &ts, &mp4, &me.mixer.tracks, offset);
                if res.is_ok() {
                    let _ = std::fs::remove_file(&ts);
                }
                me.emit(match res {
                    Ok(()) => Event::RecordingSaved(mp4),
                    Err(e) => Event::Failed(format!("Recording kept as {}: {e}", ts.display())),
                });
                me.poke();
            });
            return;
        }
        let dir = self.folder();
        let ts = dir.join(format!(".Recording {}.ts", crate::timestamp()));
        let started = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::File::create(&ts))
            .and_then(|f| self.ring.lock().unwrap().start_recording(f));
        match started {
            Ok(()) => {
                *rec = Some(ts);
                drop(rec);
                self.emit(Event::RecordingStarted);
                self.poke(); // start capturing now if idle
            }
            Err(e) => {
                drop(rec);
                self.emit(Event::Failed(format!("Couldn't start recording: {e}")));
            }
        }
    }

    pub fn screenshot(self: &Arc<Self>) {
        let me = self.clone();
        thread::spawn(move || {
            let dir = me.folder();
            let png = dir.join(format!("Screenshot {}.png", crate::timestamp()));
            let res = std::fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|_| {
                let args: Vec<String> = vec![
                    "-f".into(), "lavfi".into(), "-i".into(), me.grab(me.output_index(), 10),
                    "-frames:v".into(), "1".into(), "-vf".into(), "hwdownload,format=bgra".into(),
                    "-y".into(), png.to_string_lossy().into(),
                ];
                ffmpeg::test(&me.ffmpeg, &args)
            });
            me.emit(match res {
                Ok(()) => Event::Screenshot(png),
                Err(e) => Event::Failed(format!("Screenshot failed: {e}")),
            });
        });
    }
}

/// MPEG-TS -> MP4 without re-encoding, naming the audio tracks (TS can't carry the names).
/// Net audio-vs-video latency besides FFmpeg's start-up gap, in seconds: audio waits in the
/// mixer's 40 ms jitter buffer (+~10 ms WASAPI polling); video spends ~2 frames in the encoder.
const AV_LATENCY_FIX: f64 = 0.050 - 0.033;

/// MPEG-TS -> MP4 without re-encoding, naming the audio tracks (TS can't carry the names) and
/// pulling the audio `av_offset` seconds earlier so it lines up with the video.
fn remux(ffmpeg_path: &PathBuf, input: &Path, output: &Path, tracks: &[String], av_offset: f64) -> Result<(), String> {
    let src = input.to_string_lossy().to_string();
    let mut args: Vec<String> = vec!["-i".into(), src.clone()];
    if av_offset > 0.001 {
        args.extend(["-itsoffset".into(), format!("{:.3}", -av_offset), "-i".into(), src]);
        args.extend(["-map".into(), "0:v".into(), "-map".into(), "1:a".into()]);
    } else {
        args.extend(["-map".into(), "0".into()]);
    }
    args.extend(["-c".into(), "copy".into()]);
    for (i, name) in tracks.iter().enumerate() {
        args.extend([format!("-metadata:s:a:{i}"), format!("handler_name={name}")]);
        args.extend([format!("-metadata:s:a:{i}"), format!("title={name}")]);
    }
    // Only the full mix plays by default (what Discord and players use).
    for i in 0..tracks.len() {
        args.extend([format!("-disposition:a:{i}"), if i == 0 { "default".into() } else { "0".into() }]);
    }
    args.extend(["-movflags".into(), "+faststart".into(), "-y".into(), output.to_string_lossy().into()]);
    ffmpeg::test(ffmpeg_path, &args)
}
