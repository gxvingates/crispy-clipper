use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_TOML: &str = r#"# Crispy Clipper settings (also editable from the library's Settings tab).

# "games" = only capture while a game is running (lowest resource use), "always" = capture the desktop all the time.
capture_mode = "games"
# Seconds saved when you press the clip hotkey.
clip_seconds = 30
clip_hotkey = "F8"
record_hotkey = "F9"
screenshot_hotkey = "F10"
# Empty = %USERPROFILE%\Videos\Crispy Clipper. Clips are sorted into one folder per game.
output_dir = ""

# Monitor to capture: -1 = whichever monitor the game is on, 0 = first, 1 = second...
monitor = -1
fps = 60
# "native", "1440p", "1080p" or "720p". Downscaling is done on the GPU with NVIDIA, on the CPU otherwise.
resolution = "native"
# "h264" plays everywhere; "hevc"/"av1" are smaller at the same quality.
codec = "h264"
# Constant-quality target: lower = better and bigger (18 near-lossless, 23 good, 28 small).
quality = 21
# Upper limit for fast motion.
max_bitrate_mbps = 50
# "auto", or an FFmpeg encoder name like "h264_nvenc", "hevc_amf", "libx264".
encoder = "auto"
show_cursor = true

# Audio works like Medal's:
#   "split" = record specific programs (default): the game's own sound, Discord and any apps you add.
#             Music or videos you play elsewhere are left out, whatever output device they use.
#   "all"   = record whole output devices ("auto" = Windows' default output).
audio_mode = "split"
game_audio = true
game_volume = 100
discord_audio = true
discord_volume = 100
# Extra programs to record in "split" mode, e.g. [{ exe = "spotify.exe", volume = 60 }]
extra_apps = []
pc_audio_devices = ["auto"]
pc_volume = 100

mic_enabled = true
# "auto" = Windows' default microphone, or a device id (pick it in Settings).
mic_device = "auto"
mic_volume = 100
# Use the louder channel / average to mono (fixes mics that only come in on the left).
mic_mono = true
mic_noise_suppression = false
# Evens out your voice level automatically (quiet mics come up to a normal speaking level).
mic_auto_level = true
mic_noise_gate = false
mic_noise_gate_db = -45
push_to_talk = false
push_to_talk_key = ""

# Save game, mic and Discord as separate tracks next to the full mix (rebalance them later in the editor).
multiple_audio_tracks = true

notify_overlay = true
notify_sound = true
start_with_windows = true
# Look for new versions on GitHub and offer them in the app.
check_updates = true

# Lower-case exe names. "game_apps" are always treated as games; "ignored_apps" never are,
# even when fullscreen.
game_apps = []
ignored_apps = ["explorer.exe", "chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "vivaldi.exe",
  "vlc.exe", "mpv.exe", "mpc-hc64.exe", "potplayermini64.exe", "wmplayer.exe", "code.exe", "devenv.exe",
  "discord.exe", "obs64.exe", "applicationframehost.exe", "searchhost.exe", "lockapp.exe", "shellexperiencehost.exe",
  "startmenuexperiencehost.exe", "textinputhost.exe", "steamwebhelper.exe", "msedgewebview2.exe",
  "crispyclipper.exe", "powerpnt.exe", "acrobat.exe", "spotify.exe", "netflix.exe", "screenclippinghost.exe",
  "steam.exe", "epicgameslauncher.exe", "riotclientservices.exe", "riotclientux.exe", "riotclientuxrender.exe",
  "battle.net.exe", "eadesktop.exe", "galaxyclient.exe", "upc.exe", "rockstarlauncher.exe", "unrealcefsubprocess.exe"]

# Path to ffmpeg.exe. "ffmpeg" = use PATH, or ffmpeg.exe next to CrispyClipper.exe.
ffmpeg = "ffmpeg"
"#;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct ExtraApp {
    pub exe: String,
    pub volume: u32,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct Config {
    pub capture_mode: String,
    pub clip_seconds: u32,
    pub clip_hotkey: String,
    pub record_hotkey: String,
    pub screenshot_hotkey: String,
    pub output_dir: String,
    pub monitor: i32,
    pub fps: u32,
    pub resolution: String,
    pub codec: String,
    pub quality: u32,
    pub max_bitrate_mbps: u32,
    pub encoder: String,
    pub show_cursor: bool,
    pub audio_mode: String,
    pub game_audio: bool,
    pub game_volume: u32,
    pub discord_audio: bool,
    pub discord_volume: u32,
    pub extra_apps: Vec<ExtraApp>,
    pub pc_audio_devices: Vec<String>,
    pub pc_volume: u32,
    pub mic_enabled: bool,
    pub mic_device: String,
    pub mic_volume: u32,
    pub mic_mono: bool,
    pub mic_noise_suppression: bool,
    pub mic_auto_level: bool,
    pub mic_noise_gate: bool,
    pub mic_noise_gate_db: i32,
    pub push_to_talk: bool,
    pub push_to_talk_key: String,
    pub multiple_audio_tracks: bool,
    pub notify_overlay: bool,
    pub notify_sound: bool,
    pub start_with_windows: bool,
    pub check_updates: bool,
    pub game_apps: Vec<String>,
    pub ignored_apps: Vec<String>,
    pub ffmpeg: String,
}

pub fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(Into::into)).unwrap_or_default()
}

/// %LOCALAPPDATA%\Crispy Clipper (settings, webview cache).
pub fn data_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| exe_dir());
    base.join("Crispy Clipper")
}

pub fn config_path() -> PathBuf {
    data_dir().join("settings.toml")
}

fn defaults() -> toml::Table {
    toml::from_str(DEFAULT_TOML).expect("default config parses")
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let path = config_path();
        let _ = std::fs::create_dir_all(data_dir());
        if !path.exists() {
            let _ = std::fs::write(&path, DEFAULT_TOML);
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parses settings; anything missing falls back to the defaults.
    pub fn parse(text: &str) -> Result<Config, String> {
        let mut table = defaults();
        // Notepad may save with a BOM.
        let user: toml::Table = toml::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
        table.extend(user);
        table.try_into().map_err(|e: toml::de::Error| e.to_string())
    }

    pub fn save(&self) -> Result<(), String> {
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        let header = "# Crispy Clipper settings. See the defaults in the app's source (config.rs) for comments.\n\n";
        std::fs::write(config_path(), format!("{header}{text}")).map_err(|e| e.to_string())
    }

    pub fn output_dir(&self) -> PathBuf {
        if self.output_dir.trim().is_empty() {
            let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join("Videos").join("Crispy Clipper")
        } else {
            PathBuf::from(&self.output_dir)
        }
    }

    /// True when `other` differs from `self` only in volumes, which the recorder applies live
    /// (like Medal) instead of restarting.
    pub fn only_volumes_differ(&self, other: &Config) -> bool {
        let mut a = self.clone();
        a.game_volume = other.game_volume;
        a.discord_volume = other.discord_volume;
        a.pc_volume = other.pc_volume;
        a.mic_volume = other.mic_volume;
        if a.extra_apps.len() == other.extra_apps.len() {
            for (x, y) in a.extra_apps.iter_mut().zip(&other.extra_apps) {
                x.volume = y.volume;
            }
        }
        a == *other
    }

    /// Output height for `resolution`, or None for native.
    pub fn target_height(&self) -> Option<u32> {
        match self.resolution.as_str() {
            "2160p" => Some(2160),
            "1440p" => Some(1440),
            "1080p" => Some(1080),
            "720p" => Some(720),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn save_roundtrip() {
        let c = super::Config::parse("").unwrap();
        let back = super::Config::parse(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
        let mut louder = c.clone();
        louder.mic_volume = 250;
        assert!(c.only_volumes_differ(&louder));
        louder.mic_mono = !louder.mic_mono;
        assert!(!c.only_volumes_differ(&louder));
    }
}
