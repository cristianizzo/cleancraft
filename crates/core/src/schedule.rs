//! The menu bar agent's brain, without any UI: scheduled scans, the "disk almost full" alert,
//! notices from the watchers, and what the menu bar item shows.
//!
//! Safety: the agent only looks. A scheduled scan is Quick Sweep's scan ([`sweep::scan`]):
//! it saves the plans so the app can show them, and never calls the executor. Notifications
//! and "open the app" are the only things it does to the outside.
//!
//! Times are local wall-clock times ([`NaiveDateTime`]) passed in by the caller, so the
//! schedule is tested without a clock.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};

use crate::env::Env;
use crate::fsutil::human_size;
use crate::monitor::{self, Snapshot, UNKNOWN};
use crate::notify::{self, Notification};
use crate::settings::Settings;
use crate::sweep::{self, Sources};

/// How often the agent scans by itself.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ScanEvery {
    #[default]
    Off,
    Daily,
    Weekly,
}

impl ScanEvery {
    pub const ALL: [ScanEvery; 3] = [ScanEvery::Off, ScanEvery::Daily, ScanEvery::Weekly];

    pub fn label(self) -> &'static str {
        match self {
            ScanEvery::Off => "Off",
            ScanEvery::Daily => "Every day",
            ScanEvery::Weekly => "Every week",
        }
    }
}

/// "Monday" for 1 ... "Sunday" for 7.
pub fn weekday_name(d: u8) -> &'static str {
    match d {
        1 => "Monday",
        2 => "Tuesday",
        3 => "Wednesday",
        4 => "Thursday",
        5 => "Friday",
        6 => "Saturday",
        _ => "Sunday",
    }
}

/// "09:00".
pub fn hour_text(h: u8) -> String {
    format!("{:02}:00", h.min(23))
}

/// The schedule as one key: a change of plan re-arms it from the time of the change.
pub fn schedule_key(s: &Settings) -> String {
    match s.scan_every {
        ScanEvery::Off => "off".into(),
        ScanEvery::Daily => format!("daily@{}", s.scan_hour.min(23)),
        ScanEvery::Weekly => format!("weekly@{}d{}", s.scan_hour.min(23), s.scan_weekday),
    }
}

fn at_hour(d: NaiveDate, hour: u8) -> NaiveDateTime {
    d.and_time(NaiveTime::from_hms_opt(u32::from(hour.min(23)), 0, 0).unwrap_or(NaiveTime::MIN))
}

/// The latest scheduled moment at or before `now`; `None` when scans are off.
pub fn last_slot(
    every: ScanEvery,
    hour: u8,
    weekday: u8,
    now: NaiveDateTime,
) -> Option<NaiveDateTime> {
    let today = at_hour(now.date(), hour);
    match every {
        ScanEvery::Off => None,
        ScanEvery::Daily => Some(if today <= now {
            today
        } else {
            today - Duration::days(1)
        }),
        ScanEvery::Weekly => {
            let want = i64::from(weekday.clamp(1, 7));
            let have = i64::from(now.date().weekday().number_from_monday());
            let back = (have - want).rem_euclid(7);
            let slot = at_hour(now.date() - Duration::days(back), hour);
            Some(if slot <= now {
                slot
            } else {
                slot - Duration::days(7)
            })
        }
    }
}

/// The next scheduled moment after `now`; `None` when scans are off.
pub fn next_slot(
    every: ScanEvery,
    hour: u8,
    weekday: u8,
    now: NaiveDateTime,
) -> Option<NaiveDateTime> {
    let last = last_slot(every, hour, weekday, now)?;
    Some(match every {
        ScanEvery::Weekly => last + Duration::days(7),
        _ => last + Duration::days(1),
    })
}

/// Is a scan due? Yes when a scheduled moment passed after both the last scan and the time
/// the schedule was set (`armed`). Moments missed while the Mac slept give one scan, not one
/// per missed moment, because the scan that runs is later than all of them.
pub fn due(
    every: ScanEvery,
    hour: u8,
    weekday: u8,
    now: NaiveDateTime,
    last_scan: Option<NaiveDateTime>,
    armed: Option<NaiveDateTime>,
) -> bool {
    let Some(slot) = last_slot(every, hour, weekday, now) else {
        return false;
    };
    match last_scan.max(armed) {
        Some(since) => slot > since,
        None => false,
    }
}

/// "Next scan: Tuesday at 10:00" for the Settings page.
pub fn next_text(s: &Settings, now: NaiveDateTime) -> Option<String> {
    let n = next_slot(s.scan_every, s.scan_hour, s.scan_weekday, now)?;
    let day = if n.date() == now.date() {
        "today".to_string()
    } else if n.date() == now.date() + Duration::days(1) {
        "tomorrow".to_string()
    } else {
        weekday_name(n.date().weekday().number_from_monday() as u8).to_string()
    };
    Some(format!("Next scan: {day} at {}", hour_text(n.hour() as u8)))
}

// ---------------------------------------------------------------------------------------------
// The agent's saved state: data_dir/agent-state.json.

/// Local times as seconds (a wall-clock time stored as if it were UTC).
fn secs(t: NaiveDateTime) -> i64 {
    t.and_utc().timestamp()
}

fn from_secs(s: i64) -> Option<NaiveDateTime> {
    chrono::DateTime::from_timestamp(s, 0).map(|d| d.naive_utc())
}

/// Something found that the person can open from the menu bar: the text of the menu item
/// and the page it opens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub text: String,
    pub route: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentState {
    /// When the current schedule was set (local seconds).
    pub armed: Option<i64>,
    /// The schedule [`schedule_key`] that `armed` belongs to.
    pub key: String,
    /// When the last scheduled scan ran (local seconds), and what it found ready to clean.
    pub last_scan: Option<i64>,
    pub last_found: Option<u64>,
    /// The day of the last "disk almost full" notification, "2026-10-20".
    pub disk_alert_day: Option<String>,
    /// Since when memory pressure has been Warning or Critical without a break.
    pub pressed_since: Option<i64>,
    /// The day of the last "memory under pressure" notification.
    pub memory_alert_day: Option<String>,
    /// The day of the last "the Trash is big" notification.
    pub trash_alert_day: Option<String>,
    /// The day of the last "Apple's malware rules are old" notification.
    pub rules_alert_day: Option<String>,
    /// What the menu offers to open, newest last (at most [`REVIEWS_MAX`]).
    pub reviews: Vec<Review>,
}

/// Memory pressure must stay Warning or Critical this long before it is notified.
pub const MEMORY_ALERT_AFTER: i64 = 5 * 60;
/// Days between two "the Trash is big" notifications.
pub const TRASH_ALERT_DAYS: i64 = 7;
/// Days between two "Apple's malware rules are old" notifications.
pub const RULES_ALERT_DAYS: i64 = 7;
/// What the old-rules notification opens: Threat Check.
pub const THREAT_ROUTE: &str = "malware";

/// Menu items kept for findings.
pub const REVIEWS_MAX: usize = 3;
/// What the scheduled scan's notification and menu item open: Quick Sweep, scanning.
pub const SWEEP_ROUTE: &str = "scan:quick_sweep";

impl AgentState {
    pub fn path(env: &Env) -> PathBuf {
        env.data_dir().join("agent-state.json")
    }

    pub fn load(env: &Env) -> AgentState {
        std::fs::read(Self::path(env))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
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

    fn push_review(&mut self, r: Review) {
        self.reviews.retain(|x| x.route != r.route);
        self.reviews.push(r);
        while self.reviews.len() > REVIEWS_MAX {
            self.reviews.remove(0);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The menu.

/// What the menu bar item shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuModel {
    /// "CPU 12% RAM 61%", with " Disk low" when the startup disk is almost full.
    pub title: String,
    pub disk_low: bool,
    /// CPU, memory, free space and Trash lines (always four, in this order).
    pub info: [String; 4],
    /// Findings to open, newest first.
    pub reviews: Vec<Review>,
    pub paused: bool,
}

pub const OPEN_APP: &str = "Open CleanCraft";
pub const QUICK_SWEEP: &str = "Quick Sweep...";
/// Pause stops scheduled scans and notifications (the menu bar panel and menu, and Settings).
pub const PAUSE: &str = "Pause Scans and Notifications";
pub const RESUME: &str = "Resume Scans and Notifications";
pub const QUIT: &str = "Quit Menu Bar Agent";
pub const DISK_LOW_BADGE: &str = "Disk low";

impl MenuModel {
    pub fn pause_label(&self) -> &'static str {
        if self.paused { RESUME } else { PAUSE }
    }
}

/// What a menu item asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// Open the n-th review (index in [`MenuModel::reviews`]).
    Review(usize),
    /// Open the app on a fixed route (the panel's quick actions).
    Route(&'static str),
    Open,
    QuickSweep,
    TogglePause,
    Quit,
}

/// Free space under the setting's percent of the disk? `None` when unknown or turned off.
pub fn disk_low(snap: &Snapshot, percent: u8) -> Option<bool> {
    if percent == 0 {
        return None;
    }
    let (total, free) = (snap.storage.total?, snap.storage.free?);
    if total == 0 {
        return None;
    }
    Some((free as f64) < total as f64 * f64::from(percent) / 100.0)
}

/// The four information lines of the menu.
pub fn info_lines(snap: Option<&Snapshot>) -> [String; 4] {
    let cpu = snap
        .and_then(|s| s.cpu.as_ref())
        .map(|c| monitor::pct(c.load))
        .unwrap_or_else(|| UNKNOWN.into());
    let mem = snap
        .and_then(|s| s.memory.as_ref())
        .map(|m| {
            format!(
                "{} of {} used",
                crate::fsutil::memory_size(m.used),
                crate::fsutil::memory_size(m.total)
            )
        })
        .unwrap_or_else(|| UNKNOWN.into());
    let free = snap
        .and_then(|s| s.storage.free)
        .map(human_size)
        .unwrap_or_else(|| UNKNOWN.into());
    let trash = snap
        .and_then(|s| s.storage.trash)
        .map(human_size)
        .unwrap_or_else(|| UNKNOWN.into());
    [
        format!("CPU: {cpu}"),
        format!("Memory: {mem}"),
        format!("Free space: {free}"),
        format!("Trash: {trash}"),
    ]
}

/// The notification text of a scan that found `bytes` ready to clean.
pub fn found_text(bytes: u64) -> String {
    format!("CleanCraft found {} to clean", human_size(bytes))
}

// ---------------------------------------------------------------------------------------------
// The agent.

/// What [`Agent::check`] did, for the log and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Bytes a scheduled scan found ready to clean, when one ran.
    pub scanned: Option<u64>,
    /// Notifications shown (none while paused).
    pub shown: Vec<Notification>,
    /// Notifications that could not be shown, with why.
    pub failed: Vec<String>,
}

pub struct Agent {
    pub state: AgentState,
    sources: Sources,
}

impl Agent {
    pub fn new(env: &Env, sources: Sources) -> Agent {
        Agent {
            state: AgentState::load(env),
            sources,
        }
    }

    pub fn menu(&self, snap: Option<&Snapshot>, settings: &Settings) -> MenuModel {
        let low = snap
            .and_then(|s| disk_low(s, settings.disk_low_percent))
            .unwrap_or(false);
        let mut title = match snap {
            Some(s) => monitor::menu_bar_title(s),
            None => "CPU ? RAM ?".into(),
        };
        if low {
            title.push(' ');
            title.push_str(DISK_LOW_BADGE);
        }
        MenuModel {
            title,
            disk_low: low,
            info: info_lines(snap),
            reviews: self.state.reviews.iter().rev().cloned().collect(),
            paused: settings.notifications_paused,
        }
    }

    /// The periodic work: a scheduled scan when one is due, and the disk alert at most once a
    /// day. While paused, scheduled scans are skipped and notifications are not shown. The
    /// state is saved.
    pub fn check(
        &mut self,
        env: &Env,
        settings: &Settings,
        snap: Option<&Snapshot>,
        now: NaiveDateTime,
    ) -> CheckReport {
        let mut rep = CheckReport::default();
        // Arm a new schedule from now: moments before it was set do not count.
        let key = schedule_key(settings);
        if key != self.state.key {
            self.state.key = key;
            self.state.armed = (settings.scan_every != ScanEvery::Off).then(|| secs(now));
        }
        let is_due = due(
            settings.scan_every,
            settings.scan_hour,
            settings.scan_weekday,
            now,
            self.state.last_scan.and_then(from_secs),
            self.state.armed.and_then(from_secs),
        );
        if is_due && settings.notifications_paused {
            // Paused: the moment passes without a scan, and resuming does not catch up on it.
            self.state.armed = Some(secs(now));
        } else if is_due {
            let found = scan_only(env, settings, &self.sources);
            // A light home measure for Mac Health's "What grew" (at most once a day).
            let _ = crate::health::record_baseline(env, chrono::Utc::now().timestamp());
            self.state.last_scan = Some(secs(now));
            self.state.last_found = Some(found);
            rep.scanned = Some(found);
            if found > 0 {
                let text = found_text(found);
                self.state.push_review(Review {
                    text: format!("Review: {text}"),
                    route: SWEEP_ROUTE.into(),
                });
                self.show(
                    env,
                    settings,
                    Notification {
                        title: "CleanCraft".into(),
                        subtitle: None,
                        // notify adds "Open the CleanCraft menu bar item to review."
                        body: format!("{text}."),
                        route: Some(SWEEP_ROUTE.into()),
                    },
                    &mut rep,
                );
            }
        }
        if let Some(s) = snap
            && disk_low(s, settings.disk_low_percent) == Some(true)
        {
            let today = now.date().format("%Y-%m-%d").to_string();
            if self.state.disk_alert_day.as_deref() != Some(today.as_str()) {
                self.state.disk_alert_day = Some(today);
                let free = s.storage.free.map(human_size).unwrap_or_default();
                self.state.push_review(Review {
                    text: format!("Review: startup disk almost full ({free} free)"),
                    route: "quick_sweep".into(),
                });
                self.show(
                    env,
                    settings,
                    Notification {
                        title: "Your startup disk is almost full".into(),
                        subtitle: None,
                        body: format!("{free} free. Quick Sweep can show what to clean."),
                        route: Some("quick_sweep".into()),
                    },
                    &mut rep,
                );
            }
        }
        self.memory_alert(env, settings, snap, now, &mut rep);
        self.trash_alert(env, settings, snap, now, &mut rep);
        self.rules_alert(env, settings, now, std::time::SystemTime::now(), &mut rep);
        // Free-space history for Mac Health (at most one sample an hour).
        let _ = crate::space_history::record(env);
        let _ = self.state.save(env);
        rep
    }

    /// Memory pressure Warning or Critical for [`MEMORY_ALERT_AFTER`] without a break: one
    /// notification a day that opens the Live Monitor (top apps, Free Up RAM).
    fn memory_alert(
        &mut self,
        env: &Env,
        settings: &Settings,
        snap: Option<&Snapshot>,
        now: NaiveDateTime,
        rep: &mut CheckReport,
    ) {
        let pressed = snap.and_then(|s| s.memory.as_ref()).is_some_and(|m| {
            matches!(
                m.pressure,
                Some(monitor::Pressure::Warning | monitor::Pressure::Critical)
            )
        });
        if !pressed || !settings.memory_alert {
            self.state.pressed_since = None;
            return;
        }
        let since = *self.state.pressed_since.get_or_insert(secs(now));
        let today = now.date().format("%Y-%m-%d").to_string();
        if secs(now) - since < MEMORY_ALERT_AFTER
            || self.state.memory_alert_day.as_deref() == Some(today.as_str())
        {
            return;
        }
        self.state.memory_alert_day = Some(today);
        self.state.push_review(Review {
            text: "Review: memory under pressure".into(),
            route: "monitor".into(),
        });
        self.show(
            env,
            settings,
            Notification {
                title: "Your Mac is short of memory".into(),
                subtitle: None,
                body: "Memory pressure has been high for 5 minutes. Live Monitor shows which apps use the most and can free up RAM.".into(),
                route: Some("monitor".into()),
            },
            rep,
        );
    }

    /// The Trash over [`monitor::LARGE_TRASH_BYTES`]: one notification a week that opens
    /// Trash Bins (scanning, which only looks).
    fn trash_alert(
        &mut self,
        env: &Env,
        settings: &Settings,
        snap: Option<&Snapshot>,
        now: NaiveDateTime,
        rep: &mut CheckReport,
    ) {
        let Some(size) = snap.and_then(|s| s.storage.trash) else {
            return;
        };
        let recent = self
            .state
            .trash_alert_day
            .as_deref()
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .is_some_and(|d| (now.date() - d).num_days() < TRASH_ALERT_DAYS);
        if !settings.trash_alert || size <= monitor::LARGE_TRASH_BYTES || recent {
            return;
        }
        self.state.trash_alert_day = Some(now.date().format("%Y-%m-%d").to_string());
        let size = human_size(size);
        self.state.push_review(Review {
            text: format!("Review: the Trash holds {size}"),
            route: "scan:trash_bins".into(),
        });
        self.show(
            env,
            settings,
            Notification {
                title: "Your Trash is getting big".into(),
                subtitle: None,
                body: format!(
                    "It holds {size}. Trash Bins shows what is in it before you empty it."
                ),
                route: Some("scan:trash_bins".into()),
            },
            rep,
        );
    }

    /// Apple's malware rules older than [`crate::xprotect::STALE_DAYS`]: one notification a
    /// week that opens Threat Check, which can update them. `wall` is the real clock, compared
    /// with the date of the rules file.
    pub fn rules_alert(
        &mut self,
        env: &Env,
        settings: &Settings,
        now: NaiveDateTime,
        wall: std::time::SystemTime,
        rep: &mut CheckReport,
    ) {
        let Some(days) = crate::xprotect::version(env)
            .filter(|v| v.is_old(wall))
            .and_then(|v| v.age_days(wall))
        else {
            return;
        };
        let recent = self
            .state
            .rules_alert_day
            .as_deref()
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .is_some_and(|d| (now.date() - d).num_days() < RULES_ALERT_DAYS);
        if recent {
            return;
        }
        self.state.rules_alert_day = Some(now.date().format("%Y-%m-%d").to_string());
        let title = format!("Apple's malware rules are {days} days old");
        self.state.push_review(Review {
            text: format!("Review: {title}"),
            route: THREAT_ROUTE.into(),
        });
        self.show(
            env,
            settings,
            Notification {
                title,
                subtitle: None,
                body: "Threat Check can ask macOS for the newest ones.".into(),
                route: Some(THREAT_ROUTE.into()),
            },
            rep,
        );
    }

    /// A notice from a watcher: shown (unless paused) and kept as a menu item.
    pub fn notice(&mut self, env: &Env, settings: &Settings, n: Notification) -> CheckReport {
        let mut rep = CheckReport::default();
        if let Some(route) = &n.route {
            self.state.push_review(Review {
                text: format!("Review: {}", n.title),
                route: route.clone(),
            });
        }
        self.show(env, settings, n, &mut rep);
        let _ = self.state.save(env);
        rep
    }

    fn show(&self, env: &Env, settings: &Settings, n: Notification, rep: &mut CheckReport) {
        if settings.notifications_paused {
            return;
        }
        match notify::send(env, &n) {
            Ok(()) => rep.shown.push(n),
            Err(e) => rep.failed.push(e.to_string()),
        }
    }

    /// A menu item was chosen. Pause changes `settings` (the caller saves them). Returns true
    /// for Quit.
    pub fn act(
        &mut self,
        env: &Env,
        settings: &mut Settings,
        a: MenuAction,
    ) -> Result<bool, String> {
        match a {
            MenuAction::Quit => return Ok(true),
            MenuAction::Open => {
                notify::request_open(env, "")?;
            }
            MenuAction::QuickSweep => {
                notify::request_open(env, "quick_sweep")?;
            }
            MenuAction::Route(r) => {
                notify::request_open(env, r)?;
            }
            MenuAction::Review(i) => {
                // Newest first in the menu, newest last in the state.
                let n = self.state.reviews.len();
                if i >= n {
                    return Err("That item is gone.".into());
                }
                let r = self.state.reviews[n - 1 - i].clone();
                notify::request_open(env, &r.route)?;
                self.state.reviews.remove(n - 1 - i);
                let _ = self.state.save(env);
            }
            MenuAction::TogglePause => {
                settings.notifications_paused = !settings.notifications_paused;
                settings
                    .save(env)
                    .map_err(|e| format!("Could not save the setting: {e}"))?;
            }
        }
        Ok(false)
    }
}

/// Quick Sweep's scan, looking only: every module's plan is saved for the app, nothing is
/// run. Returns the bytes Quick Sweep's Run would move.
pub fn scan_only(env: &Env, settings: &Settings, sources: &Sources) -> u64 {
    let res = sweep::scan(env, settings, sources, &AtomicBool::new(false), &|_| {});
    for t in &res.tiles {
        if let Some(p) = t.plan() {
            let _ = crate::plans::save(env, p);
            // Mac Health's "found by the last scans" (health6).
            let _ = crate::health::note_plan(env, p);
        }
    }
    res.ready_bytes(&settings.sweep_skip)
}

/// The local time now.
pub fn local_now() -> NaiveDateTime {
    chrono::Local::now().naive_local()
}

/// The watcher settings for the agent's Settings, `None` when both watches are off.
pub fn watch_settings(s: &Settings) -> Option<crate::watch::WatchSettings> {
    (s.watch_trash_offer || s.watch_new_items).then(|| crate::watch::WatchSettings {
        trash_offer: s.watch_trash_offer,
        threat_watch: s.watch_new_items,
        ..Default::default()
    })
}

/// A watcher event as the agent's notification, opening the page the event names.
pub fn watch_notification(ev: &crate::watch::WatchEvent) -> Notification {
    Notification {
        title: ev.title.clone(),
        subtitle: None,
        body: ev.body.clone(),
        route: Some(ev.open.to_arg()),
    }
}
