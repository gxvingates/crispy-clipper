# How Crispy Clipper works (and what it learned from Medal)

Crispy Clipper was designed after statically inspecting Medal's Windows install (app 2619.164.1, recorder
2619.1609.1) to see what a game clipper needs at its core and what is extra weight. Nothing from Medal is reused; the
notes below describe its architecture as observed in its shipped files.

## Medal, observed

| Part | What it is |
|---|---|
| `Medal.exe` + `resources/app` | Electron UI (React/Redux, ~1000 JS files, ~213 MB exe) |
| `MedalEncoder.exe` | .NET Framework recorder that does the actual capture and encoding |
| `Host/medal-hook64.dll`, `inject-helper64.exe`, `get-graphics-offsets64.exe` | OBS Studio's game-capture hook, renamed. Its Vulkan layer manifest exports `OBS_Negotiate` |
| FFmpeg 7 (`avcodec-61.dll`…) | Encoding/remuxing; shipped twice |
| `Plugins/`, Tesseract OCR, `events.json` | Per-game event detection for automatic highlight clips |

Recorder pipeline, from identifiers in `MedalEncoder.exe`:
- **Video:** game hook injection, falling back to window capture and then DXGI Desktop Duplication (`DuplicateOutput`).
- **Encode:** GPU encoder with a CPU fallback.
- **Audio:** WASAPI loopback, all PC audio or split per process. Audio frames go through an `AudioFrameQueue` /
  `AudioFrameStabiliser` with per-stage timestamps before the encoder.
- **Replay buffer:** encoded packets are kept in RAM or a memory-mapped buffer (`CreateMemoryBuffer`,
  `CreateMMapBuffer`). A hotkey muxes out the last N seconds without re-encoding.
- **Storage:** plain MP4s plus a SQLite library database.

Around that core, Medal also ships:
- telemetry: Sentry, Amplitude with session replay, LaunchDarkly, Firebase and OpenTelemetry
- an ads webview
- about 200 premium checks
- cloud upload and social features

## Crispy Clipper

| Medal | Crispy Clipper |
|---|---|
| OBS hook injected into the game | DXGI Desktop Duplication (FFmpeg `ddagrab`) with frames kept on the GPU. No injection, so it's anti-cheat safe |
| .NET recorder + libav DLLs | ~2 MB Rust app driving a minimal FFmpeg over pipes |
| RAM / mmap replay buffer of encoded packets | RAM ring of the MPEG-TS stream indexed by keyframe (`src/ring.rs`); clips are remuxed, never re-encoded |
| AudioFrameQueue / stabiliser | WASAPI capture → clock-driven mixer with a jitter buffer → queue → writer thread. FFmpeg can pause reading its pipe for ~0.6 s, and the queue absorbs that (`src/audio.rs`) |
| Game database + event plugins | Detects games by "fullscreen/borderless foreground app not on the ignore list"; capture runs only while that process is alive |
| Electron UI always resident | Library is a separate WebView2 process that exits when its window closes |
| Per-process audio split | "Game only" audio via Windows process loopback (Win10 2004+) |

Main loop: `src/main.rs` (tray, hotkeys, notifications) → `src/recorder.rs` (game tracking, starting and stopping
FFmpeg, clips, recordings, screenshots) → `src/library.rs` + `src/ui.html` (library window).
