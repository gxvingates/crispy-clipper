//! WASAPI capture (output loopback, per-game loopback, inputs) mixed into a clock-driven
//! 48 kHz stereo f32 stream for FFmpeg's stdin. Capture threads only hold audio devices
//! while a mixer is pumping, so nothing runs when the recorder is idle.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

pub const RATE: u32 = 48_000;
const CH: usize = 2;
// Drop audio older than this so a stalled source can't build up lag.
const MAX_QUEUE: usize = RATE as usize * CH / 5; // 200 ms
const PREBUFFER: usize = RATE as usize * CH / 25; // 40 ms

#[derive(Clone, PartialEq)]
enum Kind {
    DefaultOutput,
    DefaultInput,
    Game,
    /// A specific endpoint; `render` = output device (captured via loopback).
    Device { id: String, render: bool },
}

impl Kind {
    fn name(&self) -> &str {
        match self {
            Kind::DefaultOutput => "default output",
            Kind::DefaultInput => "default input",
            Kind::Game => "game",
            Kind::Device { id, .. } => id,
        }
    }
}

#[derive(serde::Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

/// Active (output, input) devices.
pub fn list_devices() -> (Vec<DeviceInfo>, Vec<DeviceInfo>) {
    let _ = wasapi::initialize_mta();
    let list = |dir: Direction| -> Vec<DeviceInfo> {
        let Ok(e) = DeviceEnumerator::new() else { return Vec::new() };
        let Ok(c) = e.get_device_collection(&dir) else { return Vec::new() };
        (&c).into_iter()
            .flatten()
            .filter_map(|d| Some(DeviceInfo { id: d.get_id().ok()?, name: d.get_friendlyname().ok()? }))
            .collect()
    };
    (list(Direction::Render), list(Direction::Capture))
}

/// Turns configured sources into capture targets ("all-outputs" becomes one per output device).
fn expand(sources: &[crate::config::AudioSource]) -> Vec<(Kind, f32)> {
    let (outputs, inputs) = list_devices();
    let mut v: Vec<(Kind, f32)> = Vec::new();
    for s in sources {
        let kinds = match s.id.as_str() {
            "all-outputs" => outputs.iter().map(|d| Kind::Device { id: d.id.clone(), render: true }).collect(),
            "default-output" => vec![Kind::DefaultOutput],
            "default-input" => vec![Kind::DefaultInput],
            "game" => vec![Kind::Game],
            id => vec![Kind::Device { id: id.to_string(), render: !inputs.iter().any(|d| d.id == id) }],
        };
        for k in kinds {
            // The same device listed twice would double its volume.
            if !v.iter().any(|(e, _)| *e == k) {
                v.push((k, s.volume));
            }
        }
    }
    v
}

struct Source {
    queue: Mutex<VecDeque<f32>>,
    volume: f32,
}

pub struct Mixer {
    sources: Vec<Arc<Source>>,
    active: Arc<AtomicU64>,
    /// Process whose audio is captured in "game" mode (0 = none).
    pub game_pid: Arc<AtomicU32>,
}

impl Mixer {
    pub fn new(config: &[crate::config::AudioSource]) -> Mixer {
        let active = Arc::new(AtomicU64::new(0));
        let game_pid = Arc::new(AtomicU32::new(0));
        let mut sources = Vec::new();
        for (kind, volume) in expand(config) {
            eprintln!("[audio] source {} (volume {volume})", kind.name());
            let src = Arc::new(Source { queue: Mutex::new(VecDeque::new()), volume });
            let (s, a, p) = (src.clone(), active.clone(), game_pid.clone());
            thread::spawn(move || capture_forever(kind, s, a, p));
            sources.push(src);
        }
        Mixer { sources, active, game_pid }
    }

    /// Writes mixed audio to `out` in real time until a write fails (FFmpeg exited).
    ///
    /// FFmpeg stops reading its audio pipe for up to ~0.6 s while it interleaves streams,
    /// so mixing never writes to the pipe directly: a writer thread drains a queue instead
    /// (the same decoupling Medal's recorder does with its AudioFrameQueue).
    pub fn pump(&self, out: impl Write + Send + 'static) {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let backlog = Arc::new(AtomicU64::new(0));
        let writer_backlog = backlog.clone();
        thread::spawn(move || {
            let mut out = out;
            for chunk in rx {
                writer_backlog.fetch_sub(chunk.len() as u64, Ordering::SeqCst);
                if out.write_all(&chunk).is_err() {
                    break; // dropping rx makes the mixer's next send fail
                }
            }
        });
        self.mix(|bytes| {
            // More than 10 s queued means FFmpeg has stopped reading for good.
            if backlog.load(Ordering::SeqCst) > (RATE as usize * CH * 4 * 10) as u64 {
                return false;
            }
            backlog.fetch_add(bytes.len() as u64, Ordering::SeqCst);
            tx.send(bytes.to_vec()).is_ok()
        });
    }

    fn mix(&self, mut emit: impl FnMut(&[u8]) -> bool) {
        for s in &self.sources {
            s.queue.lock().unwrap().clear();
        }
        // Session id, so a dying pump can't switch off its successor's capture.
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        self.active.store(id, Ordering::SeqCst);
        let start = Instant::now();
        let mut written: u64 = 0;
        let mut buf: Vec<f32> = Vec::new();
        let mut bytes: Vec<u8> = Vec::new();
        let mut primed = vec![false; self.sources.len()];
        loop {
            thread::sleep(Duration::from_millis(10));
            let due = (start.elapsed().as_secs_f64() * RATE as f64) as u64;
            let n = (due - written) as usize * CH;
            buf.clear();
            buf.resize(n, 0.0);
            for (s, primed) in self.sources.iter().zip(primed.iter_mut()) {
                let mut q = s.queue.lock().unwrap();
                // Keep ~40 ms in hand so capture-thread jitter never leaves gaps.
                if !*primed {
                    if q.len() < n + PREBUFFER {
                        continue;
                    }
                    *primed = true;
                }
                if q.len() > n + PREBUFFER + MAX_QUEUE {
                    let excess = q.len() - n - PREBUFFER;
                    q.drain(..excess);
                }
                if q.len() < n {
                    *primed = false; // ran dry (source went quiet): re-buffer before resuming
                }
                let take = n.min(q.len());
                for (o, v) in buf.iter_mut().zip(q.drain(..take)) {
                    *o += v * s.volume;
                }
            }
            bytes.clear();
            bytes.extend(buf.iter().flat_map(|v| v.clamp(-1.0, 1.0).to_le_bytes()));
            if !emit(&bytes) {
                break;
            }
            written = due;
        }
        let _ = self.active.compare_exchange(id, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

fn capture_forever(kind: Kind, src: Arc<Source>, active: Arc<AtomicU64>, game_pid: Arc<AtomicU32>) {
    let _ = wasapi::initialize_mta();
    loop {
        if active.load(Ordering::SeqCst) == 0 {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        if let Err(e) = capture(kind.clone(), &src, &active, &game_pid) {
            eprintln!("[audio] {}: {e}; retrying", kind.name());
            thread::sleep(Duration::from_secs(5)); // e.g. device unplugged
        }
        src.queue.lock().unwrap().clear();
    }
}

/// Captures until capture is deactivated or the device/target changes.
fn capture(
    kind: Kind,
    src: &Source,
    active: &AtomicU64,
    game_pid: &AtomicU32,
) -> Result<(), Box<dyn std::error::Error>> {
    let fmt = WaveFormat::new(32, 32, &SampleType::Float, RATE as usize, CH, None);
    let mode = StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: 2_000_000 };
    let enumerator = DeviceEnumerator::new()?;
    let pid = game_pid.load(Ordering::SeqCst);
    let default_dir = match &kind {
        Kind::DefaultOutput => Some(Direction::Render),
        Kind::DefaultInput => Some(Direction::Capture),
        _ => None,
    };
    let (mut client, device_id) = match &kind {
        Kind::Game => {
            if pid == 0 {
                thread::sleep(Duration::from_millis(250));
                return Ok(());
            }
            (wasapi::AudioClient::new_application_loopback_client(pid, true)?, String::new())
        }
        Kind::Device { id, .. } => (enumerator.get_device(id)?.get_iaudioclient()?, id.clone()),
        _ => {
            let device = enumerator.get_default_device(default_dir.as_ref().unwrap())?;
            (device.get_iaudioclient()?, device.get_id()?)
        }
    };
    // Direction::Capture on a render device (or a process client) = loopback.
    client.initialize_client(&fmt, &Direction::Capture, &mode)?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;

    let mut raw: VecDeque<u8> = VecDeque::new();
    let mut last_check = Instant::now();
    while active.load(Ordering::SeqCst) != 0 {
        thread::sleep(Duration::from_millis(5));
        // Each GetBuffer returns one ~10 ms packet: drain all that are waiting.
        while capture.get_next_packet_size()?.unwrap_or(0) > 0 {
            let before = raw.len();
            let info = capture.read_from_device_to_deque(&mut raw)?;
            if info.flags.silent {
                // The buffer contents are undefined for silent packets.
                raw.range_mut(before..).for_each(|b| *b = 0);
            }
        }
        if raw.len() >= 4 {
            let mut q = src.queue.lock().unwrap();
            let samples = raw.len() / 4;
            let (a, b) = raw.as_slices();
            let mut it = a.iter().chain(b.iter()).copied();
            for _ in 0..samples {
                let v = [it.next().unwrap(), it.next().unwrap(), it.next().unwrap(), it.next().unwrap()];
                q.push_back(f32::from_le_bytes(v));
            }
            raw.drain(..samples * 4);
            if q.len() > MAX_QUEUE * 2 {
                let excess = q.len() - MAX_QUEUE * 2;
                q.drain(..excess);
            }
        }
        if last_check.elapsed() > Duration::from_secs(2) {
            last_check = Instant::now();
            let changed = match (&kind, &default_dir) {
                (Kind::Game, _) => game_pid.load(Ordering::SeqCst) != pid,
                (_, Some(dir)) => enumerator.get_default_device(dir)?.get_id()? != device_id,
                _ => false,
            };
            if changed {
                break;
            }
        }
    }
    let _ = client.stop_stream();
    Ok(())
}
