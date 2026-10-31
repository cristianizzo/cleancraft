//! Mac Health: a summary built only from facts CleanCraft already reads (free space and its
//! history, the Trash, the last scans, apps not opened for months, startup items, the battery,
//! memory pressure, the macOS version), and recommendations with one action each.
//!
//! There is no score. The page's state is "Good" or "Needs attention", decided by the
//! documented rules in [`RULES`]: it needs attention when at least one card of level
//! [`Level::Attention`] or [`Level::Urgent`] is shown, and it says which. Tips never change
//! the state.
//!
//! Every card opens a page ([`AppRoute`]); none runs a cleanup or a task. Scans started from
//! a card only look, and the person reviews and confirms as anywhere else.
//!
//! Also here: the notes of the last scans (`scan-notes.json`) and the Disk Map summaries per
//! folder (`disk-map-history.json`, thinned to about one a day, then one a week), both in
//! CleanCraft's data folder. A light measure of the home folder is added at most once a day
//! (at launch and on scheduled scans), so "what grew" has data without the person mapping
//! anything.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::env::Env;
use crate::fsutil::{count, human_size};
use crate::monitor::{BatteryReading, Pressure};
use crate::plan::{FeatureId, Plan};
use crate::route::AppRoute;
use crate::space_history;

// ---------------------------------------------------------------------------------------------
// Thresholds (each one is listed in RULES and tested).

/// Free space is low: under 10% of the disk or under 10 GB (the same line as Live Monitor's
/// alert).
pub const URGENT_FREE_FRACTION: f64 = crate::monitor::LOW_FREE_FRACTION;
pub const URGENT_FREE_BYTES: u64 = crate::monitor::LOW_FREE_BYTES;
/// Free space is getting low: under 15% of the disk or under 20 GB.
pub const ATTENTION_FREE_FRACTION: f64 = 0.15;
pub const ATTENTION_FREE_BYTES: u64 = 20_000_000_000;
/// Free space fell by 10 GB or more over the last 7 days.
pub const FAST_DROP_BYTES: u64 = 10_000_000_000;
pub const DROP_DAYS: i64 = 7;
/// The Trash holds 5 GB or more (same as Live Monitor).
pub const BIG_TRASH_BYTES: u64 = crate::monitor::LARGE_TRASH_BYTES;
/// The last junk scan found 1 GB or more.
pub const JUNK_TIP_BYTES: u64 = 1_000_000_000;
/// A scan or check older than this is out of date.
pub const SCAN_STALE_DAYS: i64 = 30;
pub const UPDATES_STALE_DAYS: i64 = 14;
/// Apps not opened for this many days (App Remover's "Unused").
pub const UNUSED_DAYS: i64 = crate::apps::UNUSED_DAYS;
/// Many startup items: 15 or more turned on, outside macOS.
pub const MANY_STARTUP_ITEMS: usize = 15;
/// Battery capacity under 80% of its design capacity, or 1000 charge cycles or more.
pub const BATTERY_HEALTH_PCT: u8 = 80;
pub const BATTERY_CYCLES: u32 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Worth doing; does not change the summary.
    Tip,
    /// The summary becomes "Needs attention".
    Attention,
    /// The summary becomes "Needs attention"; shown first.
    Urgent,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Tip => "Tip",
            Level::Attention => "Needs attention",
            Level::Urgent => "Act soon",
        }
    }
}

/// One documented rule: what it checks and the level it gives.
pub struct Rule {
    pub id: &'static str,
    pub level: Level,
    pub text: &'static str,
}

/// Every rule the page applies, in the order cards are made. Shown on the page under "How
/// this is decided".
pub const RULES: &[Rule] = &[
    Rule {
        id: "free_low",
        level: Level::Urgent,
        text: "Free space under 10% of the disk or under 10 GB.",
    },
    Rule {
        id: "free_getting_low",
        level: Level::Attention,
        text: "Free space under 15% of the disk or under 20 GB.",
    },
    Rule {
        id: "free_drop",
        level: Level::Attention,
        text: "Free space fell by 10 GB or more in the last 7 days (needs samples over at least 2 days).",
    },
    Rule {
        id: "threats",
        level: Level::Urgent,
        text: "The last Threat Check found something that is still there.",
    },
    Rule {
        id: "startup_unsigned",
        level: Level::Attention,
        text: "A startup item that is turned on is not signed, or not signed by a known developer.",
    },
    Rule {
        id: "memory",
        level: Level::Attention,
        text: "Memory pressure is elevated (critical: act soon).",
    },
    Rule {
        id: "battery",
        level: Level::Attention,
        text: "The battery holds under 80% of its design capacity, or has 1000 charge cycles or more.",
    },
    Rule {
        id: "trash",
        level: Level::Tip,
        text: "The Trash holds 5 GB or more.",
    },
    Rule {
        id: "junk",
        level: Level::Tip,
        text: "The last junk scan found 1 GB or more, or there was no junk scan in 30 days.",
    },
    Rule {
        id: "threat_scan",
        level: Level::Tip,
        text: "No Threat Check in 30 days.",
    },
    Rule {
        id: "unused_apps",
        level: Level::Tip,
        text: "Apps not opened for 180 days (by Spotlight's last-opened date; unknown dates never count).",
    },
    Rule {
        id: "startup_many",
        level: Level::Tip,
        text: "15 or more startup items outside macOS are turned on.",
    },
    Rule {
        id: "updates",
        level: Level::Tip,
        text: "The last update check found updates, or there was no check in 14 days.",
    },
];

// ---------------------------------------------------------------------------------------------
// Scan notes

/// What a scan found, kept so Mac Health can say "1.2 GB of junk found 3 days ago".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanNote {
    /// When the scan ran, seconds since the epoch.
    pub at: i64,
    /// Bytes still proposed (what the scan found, minus what was moved since).
    pub bytes: u64,
    /// Items still proposed (threats for Threat Check, updates for App Updates).
    pub items: usize,
    /// The plan the note is about (empty for counts).
    #[serde(default)]
    pub plan_id: String,
    /// Threat Check only: the items found (at most [`NOTE_PATHS`]), so Mac Health can open
    /// Threat Check with the one it counted selected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<PathBuf>,
}

/// Paths kept in a Threat Check note.
pub const NOTE_PATHS: usize = 20;

fn notes_path(env: &Env) -> PathBuf {
    env.data_dir().join("scan-notes.json")
}

/// The latest note of each module. A missing or damaged file gives none.
pub fn scan_notes(env: &Env) -> BTreeMap<FeatureId, ScanNote> {
    std::fs::read(notes_path(env))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_json<T: Serialize>(p: &Path, v: &T) -> io::Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(v).map_err(io::Error::other)?)?;
    std::fs::rename(&tmp, p)
}

/// Keep `note` as the latest of `f`. A note older than the one kept is ignored.
pub fn note(env: &Env, f: FeatureId, note: ScanNote) -> io::Result<()> {
    let mut all = scan_notes(env);
    if all.get(&f).is_some_and(|old| old.at > note.at) {
        return Ok(());
    }
    if all.get(&f) == Some(&note) {
        return Ok(());
    }
    all.insert(f, note);
    write_json(&notes_path(env), &all)
}

/// Note what a plan proposes (all its items, selected or not).
pub fn note_plan(env: &Env, plan: &Plan) -> io::Result<()> {
    let Some(f) = plan.feature else {
        return Ok(());
    };
    note(
        env,
        f,
        ScanNote {
            at: plan.created,
            bytes: plan.items.iter().map(|i| i.size).sum(),
            items: plan.items.len(),
            plan_id: plan.id.clone(),
            paths: if f == FeatureId::Malware {
                plan.items
                    .iter()
                    .take(NOTE_PATHS)
                    .map(|i| i.path.clone())
                    .collect()
            } else {
                Vec::new()
            },
        },
    )
}

/// Note a count (App Updates: updates found) at `at`.
pub fn note_count(env: &Env, f: FeatureId, items: usize, at: i64) -> io::Result<()> {
    note(
        env,
        f,
        ScanNote {
            at,
            bytes: 0,
            items,
            plan_id: String::new(),
            paths: Vec::new(),
        },
    )
}

// ---------------------------------------------------------------------------------------------
// Disk Map summaries ("what grew")

/// Sizes of the folders at the first two levels of a Disk Map scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapSummary {
    pub at: i64,
    pub root: PathBuf,
    /// (path, bytes), biggest first.
    pub entries: Vec<(PathBuf, u64)>,
}

/// Folders kept per summary.
pub const MAP_ENTRIES: usize = 400;
/// Roots remembered.
pub const MAP_ROOTS: usize = 6;
/// A folder "grew" when it gained this much or more.
pub const GREW_BYTES: u64 = 500_000_000;

fn map_path(env: &Env) -> PathBuf {
    env.data_dir().join("disk-map-history.json")
}

pub fn map_summaries(env: &Env) -> Vec<MapSummary> {
    std::fs::read(map_path(env))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// The first two levels of a finished Disk Map tree.
pub fn summarize(tree: &crate::space::Tree, at: i64) -> Option<MapSummary> {
    let root = tree.root.as_ref()?;
    if tree.stopped {
        return None;
    }
    let mut entries = vec![(root.path.clone(), root.size)];
    for c in root.children.iter().filter(|c| c.is_dir) {
        entries.push((c.path.clone(), c.size));
        for cc in c.children.iter().filter(|c| c.is_dir) {
            entries.push((cc.path.clone(), cc.size));
        }
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.1));
    entries.truncate(MAP_ENTRIES);
    Some(MapSummary {
        at,
        root: root.path.clone(),
        entries,
    })
}

/// Keep a finished Disk Map scan: the two latest summaries of each folder, a few folders.
pub fn note_disk_map(env: &Env, tree: &crate::space::Tree, at: i64) -> io::Result<bool> {
    match summarize(tree, at) {
        Some(s) => keep_map_summary(env, s),
        None => Ok(false),
    }
}

/// Keep a summary made with [`summarize`] (the app writes it once the person leaves Disk Map,
/// so its own file never changes the sizes on screen). Per folder the two newest stay, and
/// older ones thinned out (about one a day for [`KEEP_DAILY_DAYS`] days, then one a week up
/// to [`KEEP_DAYS`] days), so "what grew" can compare over 7, 30 or 90 days.
pub fn keep_map_summary(env: &Env, s: MapSummary) -> io::Result<bool> {
    let mut all = map_summaries(env);
    if all
        .iter()
        .any(|o| o.root == s.root && o.entries == s.entries)
    {
        return Ok(false);
    }
    all.push(s.clone());
    all = thin(all);
    write_json(&map_path(env), &all)?;
    Ok(true)
}

/// Summaries kept at most per folder.
pub const MAP_PER_ROOT: usize = 24;
/// Older summaries: about one a day for this many days...
pub const KEEP_DAILY_DAYS: i64 = 10;
/// ...then one a week, up to this age.
pub const KEEP_DAYS: i64 = 100;

/// The retention of [`keep_map_summary`]: per folder the two newest, then one per day (by
/// the day it was taken) for [`KEEP_DAILY_DAYS`] days and one per week after that, none
/// older than [`KEEP_DAYS`], at most [`MAP_PER_ROOT`]; at most [`MAP_ROOTS`] folders. Days
/// and weeks are counted from the epoch, so a measure kept once stays kept as time goes on.
fn thin(mut all: Vec<MapSummary>) -> Vec<MapSummary> {
    all.sort_by_key(|o| std::cmp::Reverse(o.at));
    let newest: BTreeMap<PathBuf, i64> = all.iter().fold(BTreeMap::new(), |mut m, o| {
        m.entry(o.root.clone()).or_insert(o.at);
        m
    });
    // The oldest measure of each day or week bucket stays (oldest first, so it wins).
    let mut by_bucket: BTreeMap<(PathBuf, i64), i64> = BTreeMap::new();
    for o in all.iter().rev() {
        let age = newest[&o.root] - o.at;
        let bucket = if age <= KEEP_DAILY_DAYS * 86_400 {
            o.at.div_euclid(86_400)
        } else {
            -1 - o.at.div_euclid(7 * 86_400)
        };
        by_bucket.entry((o.root.clone(), bucket)).or_insert(o.at);
    }
    let mut count: BTreeMap<PathBuf, usize> = BTreeMap::new();
    all.retain(|o| {
        let n = count.entry(o.root.clone()).or_default();
        let age = newest[&o.root] - o.at;
        let bucket = if age <= KEEP_DAILY_DAYS * 86_400 {
            o.at.div_euclid(86_400)
        } else {
            -1 - o.at.div_euclid(7 * 86_400)
        };
        let keep = *n < 2
            || (*n < MAP_PER_ROOT
                && age <= KEEP_DAYS * 86_400
                && by_bucket.get(&(o.root.clone(), bucket)) == Some(&o.at));
        if keep {
            *n += 1;
        }
        keep
    });
    let mut roots: Vec<PathBuf> = Vec::new();
    for o in &all {
        if !roots.contains(&o.root) {
            roots.push(o.root.clone());
        }
    }
    roots.truncate(MAP_ROOTS);
    all.retain(|o| roots.contains(&o.root));
    all
}

/// A light measure of the home folder for "what grew" (U7-1): the size of each folder at
/// the first two levels, no tree kept in memory. Read only; the same size formula as Disk
/// Map. Runs off the UI thread (it reads the whole home folder once).
pub fn light_summary(env: &Env, at: i64) -> Option<MapSummary> {
    let home = env.home();
    if !std::fs::symlink_metadata(home).is_ok_and(|m| m.is_dir()) {
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    let mut total = std::fs::symlink_metadata(home)
        .map(|m| crate::fsutil::entry_size(&m))
        .unwrap_or(0);
    for c in crate::fsutil::children(home) {
        let Ok(md) = std::fs::symlink_metadata(&c) else {
            continue;
        };
        if !md.is_dir() {
            total += crate::fsutil::tree_size_once(&c, &mut seen).0;
            continue;
        }
        let mut size = crate::fsutil::entry_size(&md);
        for cc in crate::fsutil::children(&c) {
            let s = crate::fsutil::tree_size_once(&cc, &mut seen).0;
            if std::fs::symlink_metadata(&cc).is_ok_and(|m| m.is_dir()) {
                entries.push((cc, s));
            }
            size += s;
        }
        entries.push((c, size));
        total += size;
    }
    entries.push((home.to_path_buf(), total));
    entries.sort_by_key(|e| std::cmp::Reverse(e.1));
    entries.truncate(MAP_ENTRIES);
    Some(MapSummary {
        at,
        root: home.to_path_buf(),
        entries,
    })
}

/// A light home measure is taken at most this often (seconds).
pub const BASELINE_EVERY: i64 = 20 * 3_600;

/// Is a light home measure due at `now`: none of the home folder in [`BASELINE_EVERY`]?
pub fn baseline_due(env: &Env, now: i64) -> bool {
    let home = env.home();
    !map_summaries(env)
        .iter()
        .any(|s| s.root == home && now - s.at < BASELINE_EVERY && s.at <= now + 60)
}

/// Take and keep a light home measure when one is due (at launch and on each scheduled scan).
/// Writes only CleanCraft's own `disk-map-history.json`. Returns whether one was kept.
pub fn record_baseline(env: &Env, now: i64) -> io::Result<bool> {
    if !baseline_due(env, now) {
        return Ok(false);
    }
    match light_summary(env, now) {
        Some(s) => keep_map_summary(env, s),
        None => Ok(false),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grew {
    pub path: PathBuf,
    pub before: u64,
    pub after: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Growth {
    pub root: PathBuf,
    pub from: i64,
    pub to: i64,
    /// Folders that gained [`GREW_BYTES`] or more, most first. A folder is left out when one
    /// folder inside it accounts for most (80%) of its growth: the inner one is named.
    pub grew: Vec<Grew>,
}

/// Compare two summaries of the same folder (older first).
pub fn compare(old: &MapSummary, new: &MapSummary) -> Growth {
    let before: BTreeMap<&PathBuf, u64> = old.entries.iter().map(|(p, s)| (p, *s)).collect();
    let mut grew: Vec<Grew> = new
        .entries
        .iter()
        .filter(|(p, _)| *p != new.root)
        .filter_map(|(p, s)| {
            let b = before.get(p).copied().unwrap_or(0);
            (s.saturating_sub(b) >= GREW_BYTES).then(|| Grew {
                path: p.clone(),
                before: b,
                after: *s,
            })
        })
        .collect();
    let gain = |g: &Grew| g.after - g.before;
    let snapshot = grew.clone();
    grew.retain(|g| {
        !snapshot
            .iter()
            .any(|c| c.path != g.path && c.path.starts_with(&g.path) && gain(c) * 5 >= gain(g) * 4)
    });
    grew.sort_by_key(|g| std::cmp::Reverse(gain(g)));
    Growth {
        root: new.root.clone(),
        from: old.at,
        to: new.at,
        grew,
    }
}

/// What grew between the two latest Disk Map scans of the most recently scanned folder.
pub fn what_grew(env: &Env) -> Option<Growth> {
    let all = map_summaries(env);
    let newest = all.iter().max_by_key(|s| s.at)?;
    let mut same: Vec<&MapSummary> = all.iter().filter(|s| s.root == newest.root).collect();
    same.sort_by_key(|s| s.at);
    if same.len() < 2 {
        return None;
    }
    Some(compare(same[same.len() - 2], same[same.len() - 1]))
}

/// What grew over the chart's range (U7-1): the newest measure of the most recently measured
/// folder compared with the one of the same folder closest to `days` days before `now`
/// (an older one always; the nearest one when none is that old).
pub fn what_grew_in(all: &[MapSummary], now: i64, days: i64) -> Option<Growth> {
    let newest = all.iter().max_by_key(|s| s.at)?;
    let start = now - days * 86_400;
    let older = all
        .iter()
        .filter(|s| s.root == newest.root && s.at < newest.at)
        .min_by_key(|s| (s.at - start).abs())?;
    Some(compare(older, newest))
}

/// The measures of the most recently measured folder: (how many, the oldest one's time).
pub fn measures(all: &[MapSummary]) -> (usize, Option<i64>) {
    let Some(newest) = all.iter().max_by_key(|s| s.at) else {
        return (0, None);
    };
    let same: Vec<i64> = all
        .iter()
        .filter(|s| s.root == newest.root)
        .map(|s| s.at)
        .collect();
    (same.len(), same.iter().min().copied())
}

// ---------------------------------------------------------------------------------------------
// Facts

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartupFacts {
    /// Launch agents and daemons outside macOS that are turned on.
    pub enabled: usize,
    /// Of those, the ones not signed or signed ad hoc (labels).
    pub weak: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Memory {
    pub pressure: Option<Pressure>,
    pub used_fraction: f64,
}

/// Everything the page shows, read once. `None` means it could not be read (the page says
/// "Unknown"), never a guess.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub now: i64,
    pub disk_total: Option<u64>,
    pub disk_free: Option<u64>,
    /// Change of free space over [`DROP_DAYS`] days and the seconds it covers.
    pub free_change: Option<(i64, i64)>,
    pub trash: Option<u64>,
    pub junk: Option<ScanNote>,
    pub threats: Option<ScanNote>,
    pub updates: Option<ScanNote>,
    /// Apps not opened for [`UNUSED_DAYS`] days, and how many apps had a known date.
    pub unused_apps: Option<usize>,
    pub apps_dated: usize,
    pub startup: Option<StartupFacts>,
    pub battery: Option<BatteryReading>,
    pub memory: Option<Memory>,
    pub macos: Option<String>,
    /// What could not be read, in plain words.
    pub problems: Vec<String>,
}

/// Read every fact (commands through `env.sys`; slow parts: one `mdls` per app, one
/// `codesign` per startup item, `top` for a second). Run it off the UI thread.
pub fn gather(
    env: &Env,
    settings: &crate::settings::Settings,
    now: i64,
    progress: &dyn Fn(&str),
) -> Facts {
    gather_metered(
        env,
        settings,
        now,
        progress,
        &crate::progress::Progress::new(),
    )
}

/// [`gather`] reporting into `meter`: its three steps (disk and memory, apps, startup items).
pub fn gather_metered(
    env: &Env,
    settings: &crate::settings::Settings,
    now: i64,
    progress: &dyn Fn(&str),
    meter: &crate::progress::Progress,
) -> Facts {
    let mut f = Facts {
        now,
        ..Default::default()
    };
    meter.set(0, 3);
    progress("Reading disk, memory and battery");
    let snap = crate::monitor::Sampler::new().sample(env, now as f64);
    f.disk_total = snap.storage.total;
    f.disk_free = snap.storage.free;
    f.trash = snap.storage.trash;
    f.battery = snap.battery.clone();
    f.memory = snap.memory.as_ref().map(|m| Memory {
        pressure: m.pressure,
        used_fraction: m.used_fraction(),
    });
    f.problems.extend(
        snap.problems
            .iter()
            .filter(|p| !p.contains("network") && !p.contains("netstat") && !p.contains("top"))
            .cloned(),
    );
    let hist = space_history::load(env);
    if let Some(d) = hist.damaged {
        f.problems.push(d);
    }
    f.free_change = space_history::change(&hist.history, now, DROP_DAYS);

    let notes = scan_notes(env);
    f.junk = notes.get(&FeatureId::SystemJunk).cloned();
    f.threats = notes.get(&FeatureId::Malware).cloned();
    f.updates = notes.get(&FeatureId::Updater).cloned();

    meter.set(1, 3);
    progress("Reading when apps were last opened");
    let (unused, dated) = unused_apps(env, now);
    f.unused_apps = Some(unused);
    f.apps_dated = dated;

    meter.set(2, 3);
    progress("Reading startup items");
    let uid = crate::tasks::current_uid(env).ok();
    let (items, _) = crate::startup::list_launch_items(env, uid);
    let mine: Vec<_> = items
        .iter()
        .filter(|i| !i.apple && i.enabled && !settings.is_ignored(&i.plist))
        .collect();
    f.startup = Some(StartupFacts {
        enabled: mine.len(),
        weak: mine
            .iter()
            .filter(|i| i.signer.is_weak())
            .map(|i| i.display_name())
            .collect(),
    });

    f.macos = env
        .sys
        .run("/usr/bin/sw_vers", &["-productVersion"])
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.stdout.trim().to_string())
        .filter(|v| crate::tasks::parse_version(v).is_some());
    if f.macos.is_none() {
        f.problems
            .push("the macOS version could not be read".into());
    }
    f
}

/// (apps not opened for [`UNUSED_DAYS`], apps with a known last-opened date). Apps that are
/// part of macOS are not counted.
pub fn unused_apps(env: &Env, now: i64) -> (usize, usize) {
    let apps = crate::apps::inventory(env, false, &|_| {});
    let mut unused = 0;
    let mut dated = 0;
    for a in apps.iter().filter(|a| !a.apple) {
        let shown = env.display(&a.path);
        let date = env
            .sys
            .run(
                "/usr/bin/mdls",
                &["-name", "kMDItemLastUsedDate", "-raw", &shown],
            )
            .ok()
            .filter(|o| o.ok())
            .and_then(|o| crate::apps::parse_mdls_date(&o.stdout));
        if let Some(t) = date {
            dated += 1;
            if now - t > UNUSED_DAYS * 86_400 {
                unused += 1;
            }
        }
    }
    (unused, dated)
}

// ---------------------------------------------------------------------------------------------
// Recommendations

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    /// The rule that made it ([`RULES`]).
    pub rule: &'static str,
    pub level: Level,
    pub headline: String,
    pub detail: String,
    /// The one button: its label and the page it opens.
    pub action: String,
    pub route: AppRoute,
    /// Said on the page the button opens when it scans again to show what the card counted
    /// ("Scanning again to show what Threat Check found 2 days ago.").
    pub landing: Option<String>,
}

/// "today", "yesterday", "3 days ago".
pub fn ago(at: i64, now: i64) -> String {
    let days = crate::fsutil::calendar_days_ago(at, now);
    match days {
        i64::MIN..=0 => "today".into(),
        1 => "yesterday".into(),
        d => format!("{d} days ago"),
    }
}

fn days_since(at: i64, now: i64) -> i64 {
    (now - at).max(0) / 86_400
}

/// The low-space level of `free` on a disk of `total` bytes, if any.
pub fn space_level(free: u64, total: Option<u64>) -> Option<Level> {
    let frac = total.filter(|t| *t > 0).map(|t| free as f64 / t as f64);
    if free < URGENT_FREE_BYTES || frac.is_some_and(|f| f < URGENT_FREE_FRACTION) {
        Some(Level::Urgent)
    } else if free < ATTENTION_FREE_BYTES || frac.is_some_and(|f| f < ATTENTION_FREE_FRACTION) {
        Some(Level::Attention)
    } else {
        None
    }
}

/// The cards for `f`, most important first (urgent, then attention, then tips; rule order
/// within a level).
pub fn recommendations(f: &Facts) -> Vec<Card> {
    let mut v = Vec::new();
    let now = f.now;
    if let Some(free) = f.disk_free
        && let Some(level) = space_level(free, f.disk_total)
    {
        v.push(Card {
            rule: if level == Level::Urgent {
                "free_low"
            } else {
                "free_getting_low"
            },
            level,
            headline: format!("Only {} free on your startup disk", human_size(free)),
            detail: match f.disk_total {
                Some(t) if t > 0 => format!(
                    "{:.0}% of {} is free. macOS needs room for updates, caches and swap.",
                    free as f64 * 100.0 / t as f64,
                    human_size(t)
                ),
                _ => "macOS needs room for updates, caches and swap.".into(),
            },
            action: "See What Takes Space".into(),
            route: AppRoute::Scan(FeatureId::SpaceLens),
            landing: None,
        });
    }
    if let Some((delta, secs)) = f.free_change
        && delta < 0
        && delta.unsigned_abs() >= FAST_DROP_BYTES
    {
        let days = (secs as f64 / 86_400.0).round().max(1.0) as i64;
        v.push(Card {
            rule: "free_drop",
            level: Level::Attention,
            headline: format!(
                "Free space fell by {} in {}",
                human_size(delta.unsigned_abs()),
                count(days as usize, "day", "days")
            ),
            detail: "What grew (on the right) compares the home folder with a measure from days ago; Disk Map shows what takes the space now."
                .into(),
            action: "Open Disk Map".into(),
            route: AppRoute::Scan(FeatureId::SpaceLens),
            landing: None,
        });
    }
    if let Some(n) = &f.threats
        && n.items > 0
    {
        v.push(Card {
            rule: "threats",
            level: Level::Urgent,
            headline: format!(
                "Threat Check found {} {}",
                count(n.items, "item", "items"),
                ago(n.at, now)
            ),
            detail: "Nothing was removed. Review what it found and decide.".into(),
            action: "Review in Threat Check".into(),
            route: match n.paths.first() {
                Some(p) => AppRoute::ThreatReview(p.clone()),
                None => AppRoute::Scan(FeatureId::Malware),
            },
            landing: Some(format!(
                "Scanning again to show what Threat Check found {}.",
                ago(n.at, now)
            )),
        });
    }
    if let Some(s) = &f.startup
        && !s.weak.is_empty()
    {
        let names = s
            .weak
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        v.push(Card {
            rule: "startup_unsigned",
            level: Level::Attention,
            headline: format!(
                "{} not signed by a known developer",
                if s.weak.len() == 1 {
                    "1 startup item is".to_string()
                } else {
                    format!("{} startup items are", s.weak.len())
                }
            ),
            detail: format!(
                "{names}{}. Check that you know what starts with your Mac.",
                if s.weak.len() > 3 { " and more" } else { "" }
            ),
            action: "Review Startup Items".into(),
            route: AppRoute::Feature(FeatureId::LoginItems),
            landing: None,
        });
    }
    if let Some(m) = &f.memory
        && let Some(p @ (Pressure::Warning | Pressure::Critical)) = m.pressure
    {
        v.push(Card {
            rule: "memory",
            level: if p == Pressure::Critical {
                Level::Urgent
            } else {
                Level::Attention
            },
            headline: format!("Memory pressure is {}", p.label().to_lowercase()),
            detail: format!(
                "{:.0}% of memory is in use. Live Monitor shows the apps using the most.",
                m.used_fraction * 100.0
            ),
            action: "Open Live Monitor".into(),
            route: AppRoute::Feature(FeatureId::Monitor),
            landing: None,
        });
    }
    if let Some(BatteryReading::Present(b)) = &f.battery {
        let worn = b.health.is_some_and(|h| h < BATTERY_HEALTH_PCT);
        let cycled = b.cycles.is_some_and(|c| c >= BATTERY_CYCLES);
        if worn || cycled {
            let mut parts = Vec::new();
            if let Some(h) = b.health {
                parts.push(format!("holds {h}% of its design capacity"));
            }
            if let Some(c) = b.cycles {
                parts.push(format!("has {c} charge cycles"));
            }
            v.push(Card {
                rule: "battery",
                level: Level::Attention,
                headline: format!("The battery {}", parts.join(" and ")),
                detail: "An older battery lasts less per charge. Battery Settings says if it needs service.".into(),
                action: "Open Battery Settings".into(),
                route: AppRoute::BatterySettings,
                landing: None,
            });
        }
    }
    if let Some(t) = f.trash
        && t >= BIG_TRASH_BYTES
    {
        v.push(Card {
            rule: "trash",
            level: Level::Tip,
            headline: format!("The Trash holds {}", human_size(t)),
            detail: "It still takes space on your disk until the Trash is emptied.".into(),
            action: "Review in Trash Bins".into(),
            route: AppRoute::Scan(FeatureId::TrashBins),
            landing: None,
        });
    }
    match &f.junk {
        Some(n) if n.bytes >= JUNK_TIP_BYTES => v.push(Card {
            rule: "junk",
            level: Level::Tip,
            headline: format!("{} of junk found {}", human_size(n.bytes), ago(n.at, now)),
            detail: format!(
                "{} that macOS and your apps can make again.",
                count(n.items, "item", "items")
            ),
            action: "Review in System Junk".into(),
            route: AppRoute::Scan(FeatureId::SystemJunk),
            landing: Some(format!(
                "Scanning again to show what was found {}.",
                ago(n.at, now)
            )),
        }),
        Some(n) if days_since(n.at, now) < SCAN_STALE_DAYS => {}
        _ => v.push(Card {
            rule: "junk",
            level: Level::Tip,
            headline: format!("No junk scan in the last {SCAN_STALE_DAYS} days"),
            detail: "A scan only looks; you choose what moves to the Trash.".into(),
            action: "Scan System Junk".into(),
            route: AppRoute::Scan(FeatureId::SystemJunk),
            landing: None,
        }),
    }
    if f.threats
        .as_ref()
        .is_none_or(|n| n.items == 0 && days_since(n.at, now) >= SCAN_STALE_DAYS)
    {
        v.push(Card {
            rule: "threat_scan",
            level: Level::Tip,
            headline: format!("No Threat Check in the last {SCAN_STALE_DAYS} days"),
            detail: "It looks at apps and what starts with your Mac for known unwanted software."
                .into(),
            action: "Run Threat Check".into(),
            route: AppRoute::Scan(FeatureId::Malware),
            landing: None,
        });
    }
    if let Some(n) = f.unused_apps
        && n > 0
    {
        v.push(Card {
            rule: "unused_apps",
            level: Level::Tip,
            headline: format!("{} not opened for 6 months", count(n, "app", "apps")),
            detail: "By Spotlight's last-opened date. Apps with no known date are not counted."
                .into(),
            action: "Review in App Remover".into(),
            route: AppRoute::UnusedApps,
            landing: None,
        });
    }
    if let Some(s) = &f.startup
        && s.enabled >= MANY_STARTUP_ITEMS
    {
        v.push(Card {
            rule: "startup_many",
            level: Level::Tip,
            headline: format!("{} start with your Mac", count(s.enabled, "item", "items")),
            detail: "Agents and services outside macOS that are turned on. Fewer can mean a quicker start.".into(),
            action: "Review Startup Items".into(),
            route: AppRoute::Feature(FeatureId::LoginItems),
            landing: None,
        });
    }
    match &f.updates {
        Some(n) if n.items > 0 => v.push(Card {
            rule: "updates",
            level: Level::Tip,
            headline: format!(
                "{} available (checked {})",
                count(n.items, "update", "updates"),
                ago(n.at, now)
            ),
            detail: "Updates fix bugs and security issues.".into(),
            action: "Open App Updates".into(),
            route: AppRoute::Feature(FeatureId::Updater),
            landing: None,
        }),
        Some(n) if days_since(n.at, now) < UPDATES_STALE_DAYS => {}
        _ => v.push(Card {
            rule: "updates",
            level: Level::Tip,
            headline: format!("Updates not checked in the last {UPDATES_STALE_DAYS} days"),
            detail: "App Updates looks for new versions of your apps and macOS.".into(),
            action: "Check in App Updates".into(),
            route: AppRoute::Feature(FeatureId::Updater),
            landing: None,
        }),
    }
    // Most important first; the sort is stable, so rule order stays within a level.
    v.sort_by_key(|c| std::cmp::Reverse(c.level));
    v
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Good,
    NeedsAttention,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Good => "Good",
            State::NeedsAttention => "Needs attention",
        }
    }
}

/// The page's state and why: the headlines of the cards that decided it.
pub fn summary(cards: &[Card]) -> (State, Vec<String>) {
    let why: Vec<String> = cards
        .iter()
        .filter(|c| c.level >= Level::Attention)
        .map(|c| c.headline.clone())
        .collect();
    if why.is_empty() {
        (State::Good, why)
    } else {
        (State::NeedsAttention, why)
    }
}
