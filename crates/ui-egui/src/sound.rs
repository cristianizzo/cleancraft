//! Interface sounds: short, quiet cues for moments that matter (a scan starts or finishes, a
//! cleanup finishes, something moves to the Trash, an error).
//!
//! The sounds are CleanCraft's own "Deep Ocean" set (drops, bubbles, sonar pings and waves),
//! synthesized by `examples/make_sounds.rs` into
//! `assets/sounds/*.wav` and built into the binary. The first time a cue plays it is copied to
//! `<Application Support>/CleanCraft/Sounds/` (the env's data folder, so a fake Mac in tests)
//! and played with `/usr/bin/afplay` through [`Sys::spawn`](cleancraft_core::sys::Sys::spawn),
//! which returns at once. Nothing plays off macOS. Tests run on `FakeSys`, which only records
//! the `afplay` line.
//!
//! The switches follow [`Settings`]: `sounds` (on by default), `sound_volume` (0..1) and
//! `sound_ticks` (the tap cue, off by default). State is kept per data folder, so tests on
//! different fake Macs never share it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cleancraft_core::{Env, Settings};

/// `afplay` on every Mac.
pub const AFPLAY: &str = "/usr/bin/afplay";

/// One interface sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cue {
    /// A scan started: a soft underwater swell, filtered noise rising like a slow wave over a
    /// deep sine glide.
    ScanStart,
    /// A scan finished with results: two soft sonar pings a fifth apart, with a gentle tail.
    ScanDone,
    /// A cleanup finished: a wave washes in and draws back, then one bright water drop plink.
    CleanDone,
    /// Items moved to the Trash (a single item or a small action): a short bubble gulp.
    Trash,
    /// Something went wrong: a low, muted double tone, calm, as if heard through water.
    Error,
    /// A tiny water drop tick for switches and taps. Plays only when `sound_ticks` is on.
    Tap,
}

impl Cue {
    pub const ALL: [Cue; 6] = [
        Cue::ScanStart,
        Cue::ScanDone,
        Cue::CleanDone,
        Cue::Trash,
        Cue::Error,
        Cue::Tap,
    ];

    /// The file name in `assets/sounds` and in the cache folder.
    pub fn file_name(self) -> &'static str {
        match self {
            Cue::ScanStart => "scan-start.wav",
            Cue::ScanDone => "scan-done.wav",
            Cue::CleanDone => "clean-done.wav",
            Cue::Trash => "trash.wav",
            Cue::Error => "error.wav",
            Cue::Tap => "tap.wav",
        }
    }

    /// The WAV file (16-bit, 44.1 kHz, mono).
    pub fn wav(self) -> &'static [u8] {
        match self {
            Cue::ScanStart => include_bytes!("../../../assets/sounds/scan-start.wav"),
            Cue::ScanDone => include_bytes!("../../../assets/sounds/scan-done.wav"),
            Cue::CleanDone => include_bytes!("../../../assets/sounds/clean-done.wav"),
            Cue::Trash => include_bytes!("../../../assets/sounds/trash.wav"),
            Cue::Error => include_bytes!("../../../assets/sounds/error.wav"),
            Cue::Tap => include_bytes!("../../../assets/sounds/tap.wav"),
        }
    }

    /// The same cue asked for again within this time is dropped, so a burst of events (many
    /// items, a poll loop) gives one sound.
    pub fn min_gap(self) -> Duration {
        match self {
            Cue::Tap => Duration::from_millis(60),
            Cue::Trash => Duration::from_millis(250),
            _ => Duration::from_millis(600),
        }
    }
}

#[derive(Debug)]
struct State {
    enabled: bool,
    volume: f32,
    ticks: bool,
    last: BTreeMap<Cue, Instant>,
}

impl State {
    fn from(s: &Settings) -> State {
        State {
            enabled: s.sounds,
            volume: clamp_volume(s.sound_volume),
            ticks: s.sound_ticks,
            last: BTreeMap::new(),
        }
    }
}

static STATE: Mutex<BTreeMap<PathBuf, State>> = Mutex::new(BTreeMap::new());

/// Run `f` on the state of `env`'s data folder, read from the settings file the first time.
fn with_state<R>(env: &Env, f: impl FnOnce(&mut State) -> R) -> R {
    let mut all = match STATE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let st = all
        .entry(env.data_dir().to_path_buf())
        .or_insert_with(|| State::from(&Settings::load(env)));
    f(st)
}

fn clamp_volume(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// Take the sound settings (call after loading or saving settings).
pub fn apply(env: &Env, settings: &Settings) {
    with_state(env, |s| {
        s.enabled = settings.sounds;
        s.volume = clamp_volume(settings.sound_volume);
        s.ticks = settings.sound_ticks;
    });
}

/// Turn every interface sound on or off.
pub fn set_enabled(env: &Env, on: bool) {
    with_state(env, |s| s.enabled = on);
}

/// Loudness, 0.0 to 1.0.
pub fn set_volume(env: &Env, volume: f32) {
    with_state(env, |s| s.volume = clamp_volume(volume));
}

/// The tap tick on switches (off by default).
pub fn set_ticks(env: &Env, on: bool) {
    with_state(env, |s| s.ticks = on);
}

pub fn enabled(env: &Env) -> bool {
    with_state(env, |s| s.enabled)
}

pub fn volume(env: &Env) -> f32 {
    with_state(env, |s| s.volume)
}

/// Where the cues are copied to play them.
pub fn cache_dir(env: &Env) -> PathBuf {
    env.data_dir().join("Sounds")
}

/// Play `cue` if sounds are on (and, for [`Cue::Tap`], ticks are on). Returns at once; true
/// when the sound was started.
pub fn play(env: &Env, cue: Cue) -> bool {
    let go = with_state(env, |s| {
        if !s.enabled || (cue == Cue::Tap && !s.ticks) || s.volume <= 0.0 {
            return None;
        }
        let now = Instant::now();
        if s.last.get(&cue).is_some_and(|t| now - *t < cue.min_gap()) {
            return None;
        }
        s.last.insert(cue, now);
        Some(s.volume)
    });
    go.is_some_and(|v| start(env, cue, v))
}

/// The Settings preview button: the "scan done" cue at `volume`, whether or not sounds are on
/// (the user asked to hear it).
pub fn preview(env: &Env, volume: f32) -> bool {
    let v = clamp_volume(volume);
    v > 0.0 && start(env, Cue::ScanDone, v)
}

/// `afplay -v` is a linear gain; the slider is perceptual, so it is squared (half way is
/// about 12 dB down).
pub fn afplay_gain(volume: f32) -> f32 {
    let v = clamp_volume(volume);
    v * v
}

fn start(env: &Env, cue: Cue, volume: f32) -> bool {
    if !env.sys.is_macos() {
        return false;
    }
    let Some(path) = cached(env, cue) else {
        return false;
    };
    let gain = format!("{:.2}", afplay_gain(volume).max(0.01));
    let p = path.to_string_lossy();
    env.sys.spawn(AFPLAY, &["-v", &gain, &p]).is_ok()
}

/// The cue's file in the cache folder, written (atomically) when missing or different.
fn cached(env: &Env, cue: Cue) -> Option<PathBuf> {
    let dir = cache_dir(env);
    let path = dir.join(cue.file_name());
    let bytes = cue.wav();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64)
        && std::fs::read(&path).is_ok_and(|b| b == bytes)
    {
        return Some(path);
    }
    write_atomic(&dir, &path, bytes).ok().map(|()| path)
}

fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("wav.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

/// A decoded 16-bit mono WAV: sample rate and samples (for tests and tools).
pub fn decode_wav(bytes: &[u8]) -> Option<(u32, Vec<i16>)> {
    let u16_at = |i: usize| Some(u16::from_le_bytes(bytes.get(i..i + 2)?.try_into().ok()?));
    let u32_at = |i: usize| Some(u32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?));
    if bytes.get(0..4)? != b"RIFF" || bytes.get(8..16)? != b"WAVEfmt " {
        return None;
    }
    if u32_at(4)? as usize + 8 != bytes.len() {
        return None;
    }
    // PCM, mono, 16-bit.
    if u16_at(20)? != 1 || u16_at(22)? != 1 || u16_at(34)? != 16 {
        return None;
    }
    let rate = u32_at(24)?;
    if bytes.get(36..40)? != b"data" {
        return None;
    }
    let len = u32_at(40)? as usize;
    let data = bytes.get(44..44 + len)?;
    let samples = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect();
    Some((rate, samples))
}
