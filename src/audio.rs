//! Audio: the output devices (loopback) and input devices you pick in Settings, each with its own
//! volume. Inputs go through the mic chain: mono fix, auto level, noise suppression (RNNoise),
//! noise gate and push-to-talk. Besides the full mix, every device can get its own track.
//!
//! Everything is mixed into a clock-driven 48 kHz f32 stream (2 channels per track) for
//! FFmpeg's stdin. Devices are only held while a mixer is pumping.

use crate::config::Config;
use std::collections::{HashMap, VecDeque};
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

#[derive(Clone, PartialEq, Debug)]
enum Kind {
    DefaultOutput,
    Output(String),
    /// Microphone: None = Windows default.
    Input(Option<String>),
}

#[derive(serde::Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    /// The audio hardware this endpoint belongs to (e.g. "BEACN Mix Create").
    pub interface: String,
    pub default: bool,
}

/// Active (output, input) devices.
pub fn list_devices() -> (Vec<DeviceInfo>, Vec<DeviceInfo>) {
    let _ = wasapi::initialize_mta();
    let list = |dir: Direction| -> Vec<DeviceInfo> {
        let Ok(e) = DeviceEnumerator::new() else { return Vec::new() };
        let Ok(c) = e.get_device_collection(&dir) else { return Vec::new() };
        let default = e.get_default_device(&dir).and_then(|d| d.get_id()).unwrap_or_default();
        (&c).into_iter()
            .flatten()
            .filter_map(|d| {
                let id = d.get_id().ok()?;
                Some(DeviceInfo {
                    name: d.get_friendlyname().ok()?,
                    interface: d.get_interface_friendlyname().unwrap_or_default(),
                    default: id == default,
                    id,
                })
            })
            .collect()
    };
    (list(Direction::Render), list(Direction::Capture))
}

/// Mic processing, applied as audio arrives.
struct MicChain {
    mono: bool,
    denoise: Option<Box<nnnoiseless::DenoiseState<'static>>>,
    ns_in: Vec<f32>,
    ns_out: VecDeque<f32>,
    gate: Option<f32>,
    gate_gain: f32,
    gate_hold: u32,
    push_to_talk: Option<(Vec<i32>, i32)>,
    ptt_gain: f32,
    auto_level: bool,
    agc_gain: f32,
}

impl MicChain {
    fn new(cfg: &Config) -> MicChain {
        let ptt = cfg
            .push_to_talk
            .then(|| crate::hotkey::parse(&cfg.push_to_talk_key))
            .flatten()
            .map(|(mods, vk)| (crate::hotkey::modifier_keys(mods), vk as i32));
        MicChain {
            mono: cfg.mic_mono || cfg.mic_noise_suppression,
            denoise: cfg.mic_noise_suppression.then(nnnoiseless::DenoiseState::new),
            ns_in: Vec::new(),
            ns_out: VecDeque::new(),
            gate: cfg.mic_noise_gate.then(|| 10f32.powf(cfg.mic_noise_gate_db as f32 / 20.0)),
            gate_gain: 1.0,
            gate_hold: 0,
            push_to_talk: if cfg.push_to_talk { ptt.or(Some((Vec::new(), 0))) } else { None },
            ptt_gain: 1.0,
            auto_level: cfg.mic_auto_level,
            agc_gain: 1.0,
        }
    }

    /// Stereo in, stereo out (output can lag the input by up to 10 ms when denoising).
    fn process(&mut self, stereo: &[f32]) -> Vec<f32> {
        if !self.mono && self.gate.is_none() && self.push_to_talk.is_none() && !self.auto_level {
            return stereo.to_vec();
        }
        let mut mono: Vec<f32> = if self.mono {
            // Many interfaces deliver a mic on the left channel only; averaging would halve it.
            let (mut l, mut r) = (0f32, 0f32);
            for f in stereo.chunks_exact(2) {
                l += f[0] * f[0];
                r += f[1] * f[1];
            }
            if l > r * 100.0 {
                stereo.chunks_exact(2).map(|f| f[0]).collect()
            } else if r > l * 100.0 {
                stereo.chunks_exact(2).map(|f| f[1]).collect()
            } else {
                stereo.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5).collect()
            }
        } else {
            Vec::new()
        };
        if let Some(ns) = &mut self.denoise {
            const N: usize = nnnoiseless::DenoiseState::FRAME_SIZE;
            self.ns_in.extend(mono.iter().map(|v| v * 32768.0));
            let mut out = [0f32; N];
            while self.ns_in.len() >= N {
                ns.process_frame(&mut out, &self.ns_in[..N]);
                self.ns_in.drain(..N);
                self.ns_out.extend(out.iter().map(|v| v / 32768.0));
            }
            mono = self.ns_out.drain(..).collect();
        }
        let mut out: Vec<f32> = if self.mono {
            mono.iter().flat_map(|&v| [v, v]).collect()
        } else {
            stereo.to_vec()
        };
        // Noise gate: opens fast, closes after 200 ms below the threshold.
        if let Some(thr) = self.gate {
            for chunk in out.chunks_mut(2 * 480) {
                let rms = (chunk.iter().map(|v| v * v).sum::<f32>() / chunk.len().max(1) as f32).sqrt();
                if rms >= thr {
                    self.gate_hold = 20;
                } else if self.gate_hold > 0 {
                    self.gate_hold -= 1;
                }
                let target = if self.gate_hold > 0 { 1.0 } else { 0.0 };
                for f in chunk.chunks_exact_mut(2) {
                    let k = if target > self.gate_gain { 0.01 } else { 0.0005 };
                    self.gate_gain += (target - self.gate_gain) * k;
                    f[0] *= self.gate_gain;
                    f[1] *= self.gate_gain;
                }
            }
        }
        // Auto level: bring speech up to about -20 dBFS RMS (max 8x). The gain only moves while
        // someone is talking, so silence and background noise don't get pumped up.
        if self.auto_level {
            const TARGET: f32 = 0.1; // -20 dBFS
            for chunk in out.chunks_mut(2 * 480) {
                let rms = (chunk.iter().map(|v| v * v).sum::<f32>() / chunk.len().max(1) as f32).sqrt();
                if rms > 0.005 {
                    let want = (TARGET / rms).clamp(1.0, 8.0);
                    // Fast to back off (loud), slow to boost.
                    let k = if want < self.agc_gain { 0.3 } else { 0.02 };
                    self.agc_gain += (want - self.agc_gain) * k;
                }
                for v in chunk.iter_mut() {
                    let x = *v * self.agc_gain;
                    // Soft limiter above -1 dBFS-ish so peaks never clip.
                    *v = if x.abs() <= 0.8 { x } else { x.signum() * (0.8 + 0.2 * ((x.abs() - 0.8) / 0.2).tanh()) };
                }
            }
        }
        if let Some((mods, vk)) = &self.push_to_talk {
            let held = *vk != 0 && crate::hotkey::is_down(*vk) && mods.iter().all(|&m| crate::hotkey::is_down(m));
            let target = if held { 1.0 } else { 0.0 };
            for f in out.chunks_exact_mut(2) {
                self.ptt_gain += (target - self.ptt_gain) * 0.005;
                f[0] *= self.ptt_gain;
                f[1] *= self.ptt_gain;
            }
        }
        out
    }
}

struct Source {
    key: String,
    kind: Kind,
    /// Separate track this source also goes to (index into Mixer::tracks), besides the master.
    track: Option<usize>,
    volume: AtomicU32,
    queue: Mutex<VecDeque<f32>>,
    mic: Option<Mutex<MicChain>>,
}

impl Source {
    fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }
}

pub struct Mixer {
    sources: Vec<Arc<Source>>,
    /// Track names; index 0 is the full mix.
    pub tracks: Vec<String>,
    active: Arc<AtomicU64>,
}

fn pct(v: u32) -> f32 {
    v as f32 / 100.0
}

fn volumes(cfg: &Config) -> HashMap<String, f32> {
    let outs = cfg.audio_outputs.iter().map(|d| (format!("out:{}", d.id), pct(d.volume)));
    let ins = cfg.audio_inputs.iter().map(|d| (format!("in:{}", d.id), pct(d.volume)));
    outs.chain(ins).collect()
}

/// Short track name from a device name: "Game (BEACN Mix Create)" -> "Game".
fn track_name(name: &str) -> String {
    let n = name.split(" (").next().unwrap_or(name).trim();
    if n.is_empty() { name.to_string() } else { n.to_string() }
}

impl Mixer {
    pub fn new(cfg: &Config) -> Mixer {
        let mut plan: Vec<(String, Kind, String)> = Vec::new();
        for d in &cfg.audio_outputs {
            let kind = if d.id == "auto" { Kind::DefaultOutput } else { Kind::Output(d.id.clone()) };
            plan.push((format!("out:{}", d.id), kind, track_name(&d.name)));
        }
        for d in &cfg.audio_inputs {
            let kind = Kind::Input((d.id != "auto").then(|| d.id.clone()));
            plan.push((format!("in:{}", d.id), kind, track_name(&d.name)));
        }

        // Track 0 = everything; then one per device (when there's more than one).
        let mut tracks = vec!["All Audio".to_string()];
        let separate = cfg.multiple_audio_tracks && plan.len() > 1;
        let vols = volumes(cfg);
        let sources: Vec<Arc<Source>> = plan
            .into_iter()
            .map(|(key, kind, name)| {
                let track = separate.then(|| {
                    let mut n = name.clone();
                    let mut i = 2;
                    while tracks.contains(&n) {
                        n = format!("{name} {i}");
                        i += 1;
                    }
                    tracks.push(n);
                    tracks.len() - 1
                });
                let mic = matches!(kind, Kind::Input(_)).then(|| Mutex::new(MicChain::new(cfg)));
                let volume = AtomicU32::new(vols.get(&key).copied().unwrap_or(1.0).to_bits());
                eprintln!("[audio] source {key} {kind:?} track {track:?}");
                Arc::new(Source { key, kind, track, volume, queue: Mutex::new(VecDeque::new()), mic })
            })
            .collect();

        let active = Arc::new(AtomicU64::new(0));
        // One thread services every source: ~100 wake-ups/s total.
        let (s, a) = (sources.clone(), active.clone());
        thread::spawn(move || capture_loop(s, a));
        Mixer { sources, tracks, active }
    }

    /// Applies new volumes immediately (no restart, like Medal's sliders).
    pub fn set_volumes(&self, cfg: &Config) {
        let vols = volumes(cfg);
        for s in &self.sources {
            if let Some(v) = vols.get(&s.key) {
                s.volume.store(v.to_bits(), Ordering::Relaxed);
            }
        }
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
        let limit = (RATE as usize * CH * self.tracks.len() * 4 * 10) as u64; // 10 s
        self.mix(|bytes| {
            if backlog.load(Ordering::SeqCst) > limit {
                return false; // FFmpeg has stopped reading for good
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
        let t = self.tracks.len();
        let start = Instant::now();
        let mut written: u64 = 0;
        let mut buf: Vec<f32> = Vec::new();
        let mut bytes: Vec<u8> = Vec::new();
        let mut primed = vec![false; self.sources.len()];
        loop {
            thread::sleep(Duration::from_millis(10));
            let due = (start.elapsed().as_secs_f64() * RATE as f64) as u64;
            let frames = (due - written) as usize;
            let n = frames * CH;
            buf.clear();
            buf.resize(frames * CH * t, 0.0);
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
                let vol = s.volume();
                let take = n.min(q.len());
                for (i, v) in q.drain(..take).enumerate() {
                    let (frame, ch) = (i / CH, i % CH);
                    let v = v * vol;
                    buf[frame * CH * t + ch] += v;
                    if let Some(tr) = s.track {
                        buf[frame * CH * t + tr * CH + ch] += v;
                    }
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

struct Live {
    client: wasapi::AudioClient,
    capture: wasapi::AudioCaptureClient,
    device_id: String,
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.client.stop_stream();
    }
}

struct Target {
    src: Arc<Source>,
    live: Option<Live>,
    retry_at: Instant,
    failures: u32,
    last_error: String,
}

fn capture_loop(sources: Vec<Arc<Source>>, active: Arc<AtomicU64>) {
    let _ = wasapi::initialize_mta();
    let mut targets: Vec<Target> = sources
        .into_iter()
        .map(|src| Target { src, live: None, retry_at: Instant::now(), failures: 0, last_error: String::new() })
        .collect();
    let mut raw: VecDeque<u8> = VecDeque::new();
    let mut last_check = Instant::now();
    let enumerator = loop {
        match DeviceEnumerator::new() {
            Ok(e) => break e,
            Err(_) => thread::sleep(Duration::from_secs(1)),
        }
    };
    loop {
        if active.load(Ordering::SeqCst) == 0 {
            for t in &mut targets {
                t.live = None; // idle: release every device
                t.failures = 0;
                t.retry_at = Instant::now();
            }
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        thread::sleep(Duration::from_millis(10));
        let check = last_check.elapsed() > Duration::from_secs(2);
        if check {
            last_check = Instant::now();
        }
        for t in &mut targets {
            if t.live.is_none() {
                if Instant::now() < t.retry_at {
                    continue;
                }
                match open(&t.src.kind, &enumerator) {
                    Ok(Some(l)) => {
                        eprintln!("[audio] {} capturing", t.src.key);
                        t.live = Some(l);
                        t.failures = 0;
                        t.last_error.clear();
                        t.src.queue.lock().unwrap().clear();
                    }
                    Ok(None) => t.retry_at = Instant::now() + Duration::from_secs(2),
                    Err(e) => {
                        let e = e.to_string();
                        if e != t.last_error {
                            eprintln!("[audio] {}: {e}", t.src.key);
                            t.last_error = e;
                        }
                        t.failures += 1;
                        t.retry_at = Instant::now() + Duration::from_secs((2u64 << t.failures.min(4)).min(30));
                    }
                }
                continue;
            }
            let live = t.live.as_ref().unwrap();
            if let Err(e) = drain(live, &mut raw, &t.src) {
                eprintln!("[audio] {}: {e}", t.src.key);
                t.live = None;
                t.retry_at = Instant::now() + Duration::from_secs(1);
                continue;
            }
            // Reopen when the Windows default device changes.
            if check {
                let changed = match &t.src.kind {
                    Kind::DefaultOutput | Kind::Input(None) => {
                        let dir = if t.src.kind == Kind::DefaultOutput { Direction::Render } else { Direction::Capture };
                        enumerator.get_default_device(&dir).and_then(|d| d.get_id()).map(|id| id != live.device_id).unwrap_or(true)
                    }
                    _ => false,
                };
                if changed {
                    t.live = None;
                }
            }
        }
    }
}

/// Opens a capture stream (loopback for outputs).
fn open(kind: &Kind, enumerator: &DeviceEnumerator) -> Result<Option<Live>, Box<dyn std::error::Error>> {
    let fmt = WaveFormat::new(32, 32, &SampleType::Float, RATE as usize, CH, None);
    let mode = StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: 2_000_000 };
    let (mut client, device_id) = match kind {
        Kind::Output(id) | Kind::Input(Some(id)) => (enumerator.get_device(id)?.get_iaudioclient()?, id.clone()),
        Kind::DefaultOutput | Kind::Input(None) => {
            let dir = if *kind == Kind::DefaultOutput { Direction::Render } else { Direction::Capture };
            let d = enumerator.get_default_device(&dir)?;
            (d.get_iaudioclient()?, d.get_id()?)
        }
    };
    // Direction::Capture on an output device (or a process client) = loopback.
    client.initialize_client(&fmt, &Direction::Capture, &mode)?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    Ok(Some(Live { client, capture, device_id }))
}

/// Moves every waiting packet into the source's queue (through the mic chain for mics).
fn drain(live: &Live, raw: &mut VecDeque<u8>, src: &Source) -> Result<(), Box<dyn std::error::Error>> {
    raw.clear();
    // Each GetBuffer returns one ~10 ms packet: drain all that are waiting.
    while live.capture.get_next_packet_size()?.unwrap_or(0) > 0 {
        let before = raw.len();
        let info = live.capture.read_from_device_to_deque(raw)?;
        if info.flags.silent {
            // The buffer contents are undefined for silent packets.
            raw.range_mut(before..).for_each(|b| *b = 0);
        }
    }
    if raw.len() < 8 {
        return Ok(());
    }
    let (a, b) = raw.as_slices();
    let bytes: Vec<u8> = a.iter().chain(b.iter()).copied().collect();
    let mut samples: Vec<f32> = bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    samples.truncate(samples.len() / 2 * 2);
    if let Some(m) = &src.mic {
        samples = m.lock().unwrap().process(&samples);
    }
    let mut q = src.queue.lock().unwrap();
    q.extend(samples);
    if q.len() > MAX_QUEUE * 2 {
        let excess = q.len() - MAX_QUEUE * 2;
        q.drain(..excess);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt()
    }

    #[test]
    fn auto_level_raises_quiet_voice_without_clipping() {
        let cfg = Config::parse("").unwrap();
        let mut chain = MicChain::new(&cfg);
        // 4 s of a left-only "voice" at about -34 dBFS RMS (like a quiet interface mic), in 10 ms blocks.
        let amp = 0.02 * 2f32.sqrt();
        let mut out = Vec::new();
        for block in 0..400 {
            let input: Vec<f32> = (0..480)
                .flat_map(|i| {
                    let t = (block * 480 + i) as f32 / 48000.0;
                    [amp * (t * 2.0 * std::f32::consts::PI * 220.0).sin(), 0.0]
                })
                .collect();
            out.extend(chain.process(&input));
        }
        let tail = &out[out.len() - 48000 * 2..]; // last second
        let db = 20.0 * rms(tail).log10();
        assert!(db > -24.0 && db < -16.0, "output level {db:.1} dB");
        assert!(tail.iter().all(|v| v.abs() <= 1.0));
        // Mono fix: the left-only signal must be on both channels.
        assert!((tail[0] - tail[1]).abs() < 1e-6);
    }
}
