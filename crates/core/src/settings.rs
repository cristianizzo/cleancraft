//! User settings, shared by the app, the CLI and the MCP server:
//! `~/Library/Application Support/CleanCraft/settings.json`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::env::Env;
use crate::executor::Method;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Settings {
    /// Where removed items go.
    pub method: Method,
    /// Paths never proposed by any scan (and everything inside them).
    pub ignore: Vec<PathBuf>,
    /// Large & Old Files: minimum size in bytes.
    pub large_min_bytes: u64,
    /// Large & Old Files: "old" means not opened for this many days.
    pub old_days: u64,
    /// Unused disk images and old downloads: days.
    pub downloads_days: u64,
    /// Let AI agents (MCP) carry out confirmed plans. Off: agents can scan and preview only.
    pub agents_may_clean: bool,
    /// Show the menu bar monitor.
    pub menu_bar: bool,
    /// Languages to keep when reporting language files (besides English and Base).
    pub keep_languages: Vec<String>,
    /// Quick Sweep: modules the user turned off (shown dimmed, not run).
    pub sweep_skip: Vec<crate::plan::FeatureId>,
    /// The filters and sort order each list keeps between launches, by list ("apps",
    /// "plan.system_junk", "history"...). Searches are not kept.
    pub lists: std::collections::BTreeMap<String, ListPrefs>,
    /// Play interface sounds (scan finished, cleanup finished, errors).
    pub sounds: bool,
    /// Loudness of interface sounds, 0.0 to 1.0.
    pub sound_volume: f32,
    /// Also play a very soft tick on switches and taps (off unless the user turns it on).
    pub sound_ticks: bool,
    /// Developer Clutter: code folders to look in besides the usual ones (~/Projects...).
    pub dev_roots: Vec<PathBuf>,
    /// Developer Clutter: a project with no file changed for this many days is inactive and
    /// its build folders are selected for you.
    pub dev_inactive_days: u64,
    /// Scheduled scans by the menu bar agent (look only): off, daily or weekly.
    pub scan_every: crate::schedule::ScanEvery,
    /// Hour of the day for scheduled scans, 0 to 23, local time.
    pub scan_hour: u8,
    /// Day of the week for weekly scans, 1 (Monday) to 7 (Sunday).
    pub scan_weekday: u8,
    /// The menu bar agent warns (at most once a day) when the startup disk has less than this
    /// percent free. 0: never.
    pub disk_low_percent: u8,
    /// The menu bar agent warns (at most once a day) when memory pressure stays high.
    pub memory_alert: bool,
    /// The menu bar agent warns (at most once a week) when the Trash holds over 5 GB.
    pub trash_alert: bool,
    /// The menu bar agent shows no notifications while this is on.
    pub notifications_paused: bool,
    /// Menu bar agent: offer to remove an app's leftovers when it is dragged to the Trash.
    pub watch_trash_offer: bool,
    /// Menu bar agent: check new apps, startup items and downloads with Threat Check's rules.
    pub watch_new_items: bool,
    /// The first-launch steps (welcome, Full Disk Access, AI apps) were shown. Set as soon
    /// as the first step shows, so they appear only on the very first launch, whatever
    /// happened then. A settings file from before this field counts as set up (existing
    /// users never see the steps); a fresh install (no file) starts with false.
    #[serde(default = "set_up")]
    pub setup_done: bool,
    /// Look for a new CleanCraft on GitHub at most once a day (menu bar agent, app launch).
    #[serde(default = "on")]
    pub update_check_auto: bool,
    /// Install a new CleanCraft by itself. Off: the person is told and installs from the
    /// "Update available" banner.
    pub update_install_auto: bool,
    /// The interface language ("en", "it"). Empty: the first of the Mac's preferred languages
    /// CleanCraft speaks, else English (see `i18n`).
    pub language: String,
}

fn on() -> bool {
    true
}

fn set_up() -> bool {
    true
}

/// Interface sounds start at this loudness (0.0 to 1.0).
pub const DEFAULT_SOUND_VOLUME: f32 = 0.6;
/// Scheduled scans run at this hour unless the user picks another.
pub const DEFAULT_SCAN_HOUR: u8 = 10;
/// "Disk almost full" below this percent free.
pub const DEFAULT_DISK_LOW_PERCENT: u8 = 10;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            method: Method::Trash,
            ignore: Vec::new(),
            large_min_bytes: 50_000_000,
            old_days: 365,
            downloads_days: 90,
            agents_may_clean: false,
            menu_bar: true,
            keep_languages: Vec::new(),
            sweep_skip: Vec::new(),
            lists: Default::default(),
            sounds: true,
            sound_volume: DEFAULT_SOUND_VOLUME,
            sound_ticks: false,
            dev_roots: Vec::new(),
            dev_inactive_days: 30,
            scan_every: crate::schedule::ScanEvery::Off,
            scan_hour: DEFAULT_SCAN_HOUR,
            scan_weekday: 1,
            disk_low_percent: DEFAULT_DISK_LOW_PERCENT,
            memory_alert: true,
            trash_alert: true,
            notifications_paused: false,
            watch_trash_offer: false,
            watch_new_items: false,
            setup_done: false,
            update_check_auto: true,
            update_install_auto: false,
            language: String::new(),
        }
    }
}

impl Settings {
    pub fn path(env: &Env) -> PathBuf {
        env.data_dir().join("settings.json")
    }

    /// Load settings; when the file exists but cannot be read as settings (cut short by a crash
    /// or a full disk), keep a copy of it next to it and return a warning to show, so the user
    /// knows their ignore list and choices were not applied.
    pub fn load_checked(env: &Env) -> (Settings, Option<String>) {
        let path = Self::path(env);
        let Ok(bytes) = std::fs::read(&path) else {
            return (Settings::default(), None);
        };
        match serde_json::from_slice(&bytes) {
            Ok(s) => (s, None),
            Err(_) => {
                let copy = path.with_extension("damaged.json");
                let kept = std::fs::read(&copy).is_ok_and(|b| b == bytes)
                    || std::fs::write(&copy, &bytes).is_ok();
                let msg = if kept {
                    format!(
                        "Your settings file could not be read, so default settings are in use (ignore list empty). A copy of the damaged file is at {}.",
                        env.display(&copy)
                    )
                } else {
                    "Your settings file could not be read, so default settings are in use (ignore list empty).".to_string()
                };
                // A damaged file means CleanCraft ran here before: no first-launch steps.
                let s = Settings {
                    setup_done: true,
                    ..Settings::default()
                };
                (s, Some(msg))
            }
        }
    }

    pub fn load(env: &Env) -> Settings {
        match std::fs::read(Self::path(env)) {
            Err(_) => Settings::default(),
            // A file that cannot be read still means CleanCraft ran here before.
            Ok(b) => serde_json::from_slice(&b).unwrap_or(Settings {
                setup_done: true,
                ..Settings::default()
            }),
        }
    }

    pub fn save(&self, env: &Env) -> std::io::Result<()> {
        std::fs::create_dir_all(env.data_dir())?;
        let tmp = Self::path(env).with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(tmp, Self::path(env))
    }

    /// Is `p` on the ignore list (or inside an entry)? Names compare like a default Mac volume
    /// (letter case and Unicode composition do not matter): `~/downloads/x` is `~/Downloads/x`.
    pub fn is_ignored(&self, p: &Path) -> bool {
        self.ignore
            .iter()
            .any(|i| p.starts_with(i) || crate::policy::starts_with_ci(p, i))
    }
}

/// What a list remembers: its filters that are on and its sort order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ListPrefs {
    /// Keys of the filters that are on ("app_store", "unused").
    pub filters: Vec<String>,
    /// Key of the sort order ("size", "name"); empty means the list's first one.
    pub sort: String,
    /// The sort runs the other way round from its usual direction (smallest first).
    pub reverse: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settings_file_from_before_sounds_still_loads_with_sounds_on() {
        let dir = tempfile::tempdir().expect("tmp");
        let env = Env::fake(dir.path());
        std::fs::create_dir_all(env.data_dir()).expect("dir");
        // Written by an earlier version: no sound fields.
        std::fs::write(
            Settings::path(&env),
            r#"{"method":"trash","ignore":[],"large_min_bytes":1000,"old_days":30,
               "downloads_days":90,"agents_may_clean":false,"menu_bar":true,
               "keep_languages":[],"sweep_skip":[]}"#,
        )
        .expect("write");
        let (s, warning) = Settings::load_checked(&env);
        assert!(warning.is_none(), "{warning:?}");
        assert_eq!(s.large_min_bytes, 1000);
        assert!(s.sounds);
        assert_eq!(s.sound_volume, DEFAULT_SOUND_VOLUME);
        assert!(!s.sound_ticks);
    }

    #[test]
    fn sound_settings_round_trip() {
        let dir = tempfile::tempdir().expect("tmp");
        let env = Env::fake(dir.path());
        let s = Settings {
            sounds: false,
            sound_volume: 0.25,
            sound_ticks: true,
            ..Settings::default()
        };
        s.save(&env).expect("save");
        assert_eq!(Settings::load(&env), s);
    }
}
