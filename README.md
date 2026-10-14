# Crispy Clipper

Save the last 30 seconds of your game with one key. Crispy Clipper runs entirely on your own PC: there's no account,
no uploads, no ads and no telemetry. It's a ~1.5 MB app, and it uses nothing while you aren't playing.

## Install

1. Download **CrispyClipper.exe** from the [latest release](https://github.com/gxvingates/crispy-clipper/releases/latest).
2. Double-click it and choose **Yes** to install. It adds itself to the Start menu and desktop and starts with
   Windows.
3. On first run it downloads its video encoder (a 9 MB stripped-down FFmpeg, built by this repo's
   [workflow](.github/workflows/ffmpeg.yml)).

Windows may show "Windows protected your PC" because the exe isn't code-signed: click **More info → Run anyway**.

Requirements: Windows 10 or 11, 64-bit. Any NVIDIA, AMD or Intel GPU works; without a hardware encoder it falls back to
Windows' built-in one.

## Use

| Key | What it does |
|---|---|
| **F8** | Save the last 30 seconds (clip length is configurable) |
| **F9** | Start / stop a full recording |
| **F10** | Screenshot |

Clips are saved to `Videos\Crispy Clipper\<Game>`. Open the app (desktop icon or tray icon) to browse, play, trim,
rename, delete, copy a clip to paste into Discord, or make a 10 MB copy that fits Discord's upload limit.

By default it only records while a game is running: any fullscreen or borderless app that isn't on the ignore list.
Add windowed games under **Settings → Always treat as games**, or switch **When to capture** to **Always**.

## Features

- Instant-replay clips from an in-memory buffer, cut on keyframes without re-encoding (clips save in under a second).
- Full-session recording and screenshots.
- Hardware encoding: NVENC, AMF or Media Foundation, as H.264, HEVC or AV1, quality-based with a bitrate cap.
- Audio: everything you hear, or only the game's own sound, plus your mic, all mixed into the clip.
- Automatic game detection with one folder per game, and capture follows the game's monitor.
- Library with thumbnails, a trimmer (fast or frame-exact), Discord-size compression, and copy to clipboard.
- A "Clip saved" popup over borderless games, plus a sound cue.
- The library is a separate process that only exists while its window is open, so the background recorder stays tiny.

## How it works

See [docs/how-it-works.md](docs/how-it-works.md) for the architecture and how it compares with Medal.

## Build from source

You need Rust (stable) with the MSVC toolchain and the Windows SDK.

```bash
cargo build --release
```

The exe is written to `target\release\CrispyClipper.exe`. For development, an `ffmpeg.exe` on PATH or next to the exe
is used if present. Debug builds (anything under `target\`) never offer to install or set autostart.

## Settings and logs

Settings: `%LOCALAPPDATA%\Crispy Clipper\settings.toml` (also editable in the app). Log:
`%LOCALAPPDATA%\Crispy Clipper\log.txt`.

## Third-party

FFmpeg is LGPL-2.1 and runs as a separate program; its source is at https://github.com/FFmpeg/FFmpeg and the
exact build configuration is in `.github/workflows/ffmpeg.yml`.
