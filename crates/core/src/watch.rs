//! Watchers for the menu bar agent: they only look and tell, never clean.
//!
//! Two watches, both on file system events from the `notify` crate (FSEvents on macOS):
//!
//! - **Drag-to-Trash offer.** A new `*.app` bundle in `~/.Trash` (the user dragged an app
//!   there) is looked up with App Remover's leftover rules. When it left files behind, one event
//!   says so ("Editor was moved to the Trash" / "12 items it left behind use 340 MB.") and opens
//!   App Remover on a plan of those leftovers ([`trashed_app_plan`]), previewed and confirmed
//!   there as usual. Apps CleanCraft itself moved (they are in the journal), apps that are still
//!   installed elsewhere and bundles still being copied (their size keeps changing) are ignored.
//!   Reading `~/.Trash` needs Full Disk Access on recent macOS: without it the watch reports
//!   [`Access::NoFullDiskAccess`] and sends one [`WatchKind::NeedsFullDiskAccess`] event.
//! - **New app or startup item.** New or changed items in `/Applications`, `~/Applications`
//!   and the three launchd folders are checked with Threat Check's own rules for that one item
//!   ([`malware::launch_item_findings`], [`malware::app_findings`]: rule list, `codesign`,
//!   `spctl`, all through `env.sys`). An event comes only when a rule matches or the program
//!   is unsigned or ad hoc signed in a launch location. It opens Threat Check with that finding
//!   selected for review ([`preselect_threat`]). Nothing is ever quarantined by the watcher.
//! - **New download.** With the same switch, new files and folders in `~/Downloads` are read
//!   with Apple's malware rules and the abuse.ch sample list, like a deep scan
//!   ([`crate::xprotect::walk`]). A match opens Threat Check on it, and is kept in a short list
//!   ([`flagged`]) that every Quick check reads again, so it stays listed until it is moved.
//!
//! Limits: one notification per item (a small seen-list in CleanCraft's data folder), bursts
//! are coalesced (an installer writing 200 files is one item; several items found within
//! [`WatchSettings::coalesce`] become one event), quiet hours hold events until they end, and a
//! pause flag drops everything while set.
//!
//! The watcher never calls the executor and never moves, removes or changes a watched file.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::apps::{self, AppInfo, Store};
use crate::env::{Env, Fault};
use crate::executor::{self, ActionKind};
use crate::features::Feature;
use crate::features::malware;
use crate::features::uninstaller::{self, CAT_HIGH, CAT_MEDIUM, Uninstaller};
use crate::fsutil::{children, human_size, items};
use crate::plan::{FeatureId, Plan};
use crate::settings::Settings;
use crate::startup::{self, Domain};
use crate::xprotect;

// ---------------------------------------------------------------------------------------------
// What the agent gets

/// Where a notification opens the app: [`crate::route::AppRoute`] (string form with
/// `to_arg`/`parse` for the agent). Single findings: the trashed app opens
/// `UninstallApp(<~/.Trash/X.app>)`, which the app builds with [`trashed_app_plan`]; a startup
/// item or app opens `Scan(Malware)`, and the app ticks the finding with [`preselect_threat`]
/// using `WatchEvent::paths`; missing Full Disk Access opens `Settings`.
pub use crate::route::AppRoute;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WatchKind {
    /// An app was dragged to the Trash and left files behind.
    TrashedApp,
    /// A new or changed launch agent or daemon Threat Check flags.
    StartupItem,
    /// A new or changed app Threat Check flags.
    NewApp,
    /// A download that matches Apple's malware rules or a known sample.
    Download,
    /// The Trash watch cannot read the Trash.
    NeedsFullDiskAccess,
}

/// One notification for the agent to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchEvent {
    /// Stable per item (or per coalesced group): the agent may use it as the notification id.
    pub id: String,
    pub kind: WatchKind,
    pub title: String,
    pub body: String,
    pub open: AppRoute,
    /// Seconds since the epoch.
    pub time: i64,
    /// The items it is about (env paths). One for a single event, several for a coalesced one.
    pub paths: Vec<PathBuf>,
}

/// The local hour (0 to 23), for quiet hours. Tests give their own clock.
pub type Clock = Arc<dyn Fn() -> u8 + Send + Sync>;

#[derive(Clone)]
pub struct WatchSettings {
    /// Offer to remove leftovers when an app is dragged to the Trash.
    pub trash_offer: bool,
    /// Check new apps and startup items with Threat Check's rules.
    pub threat_watch: bool,
    /// Local hours `(start, end)`: events found from `start` until `end` are held and sent
    /// once at `end`. `(22, 7)` spans midnight. `None`: no quiet hours.
    pub quiet_hours: Option<(u8, u8)>,
    /// An item must stay the same size this long before it is looked at (a bundle being
    /// copied keeps growing).
    pub debounce: Duration,
    /// Items found within this time of the first one are sent as one event.
    pub coalesce: Duration,
    /// How often the watcher thread wakes up when nothing happens.
    pub tick: Duration,
    /// The local hour; `None` uses the system clock.
    pub clock: Option<Clock>,
}

impl Default for WatchSettings {
    fn default() -> Self {
        WatchSettings {
            trash_offer: true,
            threat_watch: true,
            quiet_hours: None,
            debounce: Duration::from_secs(2),
            coalesce: Duration::from_secs(5),
            tick: Duration::from_millis(500),
            clock: None,
        }
    }
}

impl std::fmt::Debug for WatchSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchSettings")
            .field("trash_offer", &self.trash_offer)
            .field("threat_watch", &self.threat_watch)
            .field("quiet_hours", &self.quiet_hours)
            .field("debounce", &self.debounce)
            .field("coalesce", &self.coalesce)
            .finish()
    }
}

impl WatchSettings {
    fn hour(&self) -> u8 {
        match &self.clock {
            Some(c) => c(),
            None => {
                use chrono::Timelike;
                chrono::Local::now().hour() as u8
            }
        }
    }

    /// Is `hour` inside the quiet hours?
    pub fn is_quiet_at(&self, hour: u8) -> bool {
        match self.quiet_hours {
            None => false,
            Some((s, e)) if s == e => false,
            Some((s, e)) if s < e => hour >= s && hour < e,
            Some((s, e)) => hour >= s || hour < e,
        }
    }
}

/// State of one watch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Watching,
    /// macOS keeps the folder private until CleanCraft has Full Disk Access.
    NoFullDiskAccess,
    /// Turned off in the settings.
    Off,
    /// Could not start (the text says why).
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WatchStatus {
    pub trash: Access,
    pub threats: Access,
    pub paused: bool,
    /// Folders that are not watched yet and why ("~/Applications does not exist yet").
    pub notes: Vec<String>,
}

/// The running watcher. Dropping it stops the thread.
pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    status: Arc<Mutex<WatchStatus>>,
    thread: Option<JoinHandle<()>>,
}

impl WatchHandle {
    /// While paused, events are dropped (not saved for later).
    pub fn set_paused(&self, on: bool) {
        self.paused.store(on, Ordering::SeqCst);
        lock(&self.status).paused = on;
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> WatchStatus {
        lock(&self.status).clone()
    }

    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.halt();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

pub struct Watcher;

impl Watcher {
    /// Start watching on a thread of its own. `sink` is called from that thread for every
    /// event. Errors only when the thread or the file system watcher cannot start at all;
    /// a folder that cannot be watched is reported in [`WatchHandle::status`].
    pub fn start(
        env: Env,
        settings: WatchSettings,
        sink: impl Fn(WatchEvent) + Send + 'static,
    ) -> io::Result<WatchHandle> {
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(WatchStatus {
            trash: Access::Off,
            threats: Access::Off,
            paused: false,
            notes: Vec::new(),
        }));
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let fs_watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            // Reads (including the watcher's own) are not changes.
            if let Ok(ev) = res
                && !matches!(ev.kind, notify::EventKind::Access(_))
            {
                for p in ev.paths {
                    let _ = tx.send(p);
                }
            }
        })
        .map_err(io::Error::other)?;
        let mut engine = Engine::new(env, settings, status.clone());
        // Folders are watched and their contents remembered before start returns, so
        // anything that arrives afterwards is new.
        let mut fs_watcher = fs_watcher;
        engine.maintain(&mut fs_watcher, Instant::now(), true);
        let (stop2, paused2) = (stop.clone(), paused.clone());
        let thread = std::thread::Builder::new()
            .name("cleancraft-watch".into())
            .spawn(move || {
                let tick = engine.settings.tick;
                let mut sent_fda = false;
                while !stop2.load(Ordering::SeqCst) {
                    let first = rx.recv_timeout(tick);
                    let now = Instant::now();
                    let paused = paused2.load(Ordering::SeqCst);
                    let mut got: Vec<PathBuf> = Vec::new();
                    match first {
                        Ok(p) => got.push(p),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    got.extend(rx.try_iter());
                    if paused {
                        engine.clear();
                        continue;
                    }
                    for p in got {
                        engine.on_path(&p, now);
                    }
                    engine.maintain(&mut fs_watcher, now, false);
                    if !sent_fda && let Some(ev) = engine.fda_event() {
                        sent_fda = true;
                        sink(ev);
                    }
                    for ev in engine.tick(now) {
                        if paused2.load(Ordering::SeqCst) {
                            break;
                        }
                        sink(ev);
                    }
                }
            })?;
        Ok(WatchHandle {
            stop,
            paused,
            status,
            thread: Some(thread),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Plans the routes open

/// App Remover's plan for the leftovers of an app that is in the Trash (`trashed`, an env
/// path): the same categories, rules and selection as an uninstall, without the app itself.
/// Locked when the app is still running.
pub fn trashed_app_plan(env: &Env, settings: &Settings, trashed: &Path) -> anyhow::Result<Plan> {
    // The route is untrusted text: only a real app folder directly in the Trash.
    if !in_trash(env, trashed) {
        anyhow::bail!("{} is not an app in the Trash", env.display(trashed));
    }
    let app = trashed_app_info(env, trashed).ok_or_else(|| {
        anyhow::anyhow!("{} is not an app CleanCraft can read", env.display(trashed))
    })?;
    if app.apple {
        anyhow::bail!("{} is part of macOS", app.name);
    }
    let all = apps::inventory(env, false, &|_| {});
    let mut plan = Plan::new(FeatureId::Uninstaller);
    uninstaller::add_leftover_categories(&mut plan);
    let allow = Uninstaller.allow_roots(env);
    uninstaller::push_leftovers(env, settings, &app, &all, &allow, &mut plan);
    plan.notes.push(format!(
        "{} is already in the Trash. These are the files it left behind.",
        app.name
    ));
    if uninstaller::is_running(env, trashed) {
        let why = format!("Quit {} first", app.name);
        for i in 0..plan.items.len() {
            plan.lock(i, why.clone());
        }
        plan.notes.push(why);
    }
    Ok(plan)
}

/// Select the Threat Check finding at `path` (a startup item or an app) for review. Returns its
/// index in the plan, `None` when the scan did not list it (it was fixed meanwhile).
/// Locked items stay unselected.
pub fn preselect_threat(plan: &mut Plan, path: &Path) -> Option<usize> {
    let i = plan.items.iter().position(|it| it.path == path)?;
    plan.set_selected(i, true);
    Some(i)
}

/// A real app folder (not a link) directly inside the user's Trash.
pub fn in_trash(env: &Env, p: &Path) -> bool {
    p.parent() == Some(env.user_trash().as_path())
        && is_app(p)
        && !crate::env::has_parent_component(p)
        && fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// A bundle id the Trash offer trusts: reverse-DNS with at least two parts (`com.example`),
/// not Apple's. The Info.plist of a trashed app is untrusted: a short or empty id would match
/// unrelated Library entries.
fn trusted_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    parts.len() >= 2
        && parts.iter().all(|p| !p.is_empty())
        && id.len() >= 5
        && !id.to_ascii_lowercase().starts_with("com.apple.")
}

/// The app's facts from its own Info.plist (it is not in the Applications folders any more).
fn trashed_app_info(env: &Env, app: &Path) -> Option<AppInfo> {
    let b = apps::read_bundle(app)?;
    if !trusted_id(&b.bundle_id) {
        return None;
    }
    let name = app.file_stem()?.to_string_lossy().into_owned();
    Some(AppInfo {
        path: app.to_path_buf(),
        name,
        bundle_name: b.name.clone(),
        bundle_id: b.bundle_id.clone(),
        version: b.short_version.clone(),
        build: b.version.clone(),
        size: 0,
        last_used: None,
        last_active: None,
        store: Store::Other,
        vendor: apps::plist_developer(&b),
        apple: apps::macos_app(app) || apps::in_system(env, app),
        feed_url: b.feed_url.clone(),
        part_of: None,
        vendor_uninstaller: None,
    })
}

// ---------------------------------------------------------------------------------------------
// The engine (no threads, no notify: driven by paths and the clock)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Area {
    Trash,
    Apps,
    Launch(Domain),
    Downloads,
}

impl Area {
    fn recursive(self) -> bool {
        matches!(self, Area::Apps)
    }
}

struct Root {
    area: Area,
    path: PathBuf,
    /// The same folder with links resolved, as FSEvents reports it.
    canon: Option<PathBuf>,
    watching: bool,
    /// Entries seen so far (names), to tell new ones from old.
    known: BTreeSet<PathBuf>,
    /// When the folder was last tried (missing or unreadable).
    tried: Option<Instant>,
}

struct Pending {
    area: Area,
    /// When it was first seen.
    since: Instant,
    sig: Option<Sig>,
    stable_since: Instant,
    checked: Instant,
}

/// Size, file count and newest change of an item: equal twice = it stopped changing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sig {
    bytes: u64,
    files: u64,
    newest: i128,
}

fn sig_of(p: &Path) -> Option<Sig> {
    fn walk(p: &Path, s: &mut Sig) {
        let Ok(md) = fs::symlink_metadata(p) else {
            return;
        };
        s.files += 1;
        s.bytes += md.len();
        if let Ok(t) = md.modified()
            && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
        {
            s.newest = s.newest.max(d.as_nanos() as i128);
        }
        if md.is_dir()
            && let Ok(rd) = fs::read_dir(p)
        {
            for e in rd.flatten() {
                walk(&e.path(), s);
            }
        }
    }
    fs::symlink_metadata(p).ok()?;
    let mut s = Sig {
        bytes: 0,
        files: 0,
        newest: 0,
    };
    walk(p, &mut s);
    Some(s)
}

/// One finding waiting to be sent.
#[derive(Clone, Debug)]
struct Finding {
    id: String,
    kind: WatchKind,
    path: PathBuf,
    /// What one item says, alone.
    title: String,
    body: String,
    open: AppRoute,
    /// For a merged message.
    name: String,
    count: usize,
    bytes: u64,
}

/// Ids already sent, with when (seconds since the epoch). Kept in the data folder.
struct Seen {
    path: PathBuf,
    ids: BTreeMap<String, i64>,
}

/// A Trash offer for the same app is not repeated within a day; other findings never.
const TRASH_REPEAT_SECS: i64 = 86_400;

impl Seen {
    fn load(env: &Env) -> Seen {
        let path = env.data_dir().join("watch-seen.json");
        let ids = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Seen { path, ids }
    }

    fn has(&self, id: &str, now: i64) -> bool {
        match self.ids.get(id) {
            None => false,
            Some(t) if id.starts_with("trash:") => now - t < TRASH_REPEAT_SECS,
            Some(_) => true,
        }
    }

    fn add(&mut self, id: &str, now: i64) {
        self.ids.insert(id.to_string(), now);
    }

    fn save(&self) {
        if let Some(d) = self.path.parent() {
            let _ = fs::create_dir_all(d);
        }
        if let Ok(b) = serde_json::to_vec(&self.ids) {
            let tmp = self.path.with_extension("json.tmp");
            if fs::write(&tmp, b).is_ok() {
                let _ = fs::rename(tmp, &self.path);
            }
        }
    }
}

struct Engine {
    env: Env,
    settings: WatchSettings,
    status: Arc<Mutex<WatchStatus>>,
    roots: Vec<Root>,
    pending: BTreeMap<PathBuf, Pending>,
    /// Found, waiting for the coalescing window: (when the first was found, findings).
    outbox: Option<(Instant, Vec<Finding>)>,
    /// Found during quiet hours.
    held: Vec<Finding>,
    seen: Seen,
    trash_fda_missing: bool,
}

/// How long a bundle in the Trash without a readable Info.plist is waited for.
const INCOMPLETE_WAIT: Duration = Duration::from_secs(60);

/// How often a missing or unreadable folder is tried again.
const RETRY: Duration = Duration::from_secs(2);

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Engine {
    fn new(env: Env, settings: WatchSettings, status: Arc<Mutex<WatchStatus>>) -> Engine {
        let mut roots = Vec::new();
        if settings.trash_offer {
            roots.push((Area::Trash, env.user_trash()));
        }
        if settings.threat_watch {
            roots.push((Area::Apps, env.sys_path("/Applications")));
            roots.push((Area::Apps, env.home_path("Applications")));
            for d in Domain::ALL {
                roots.push((Area::Launch(d), d.path(&env)));
            }
            roots.push((Area::Downloads, env.home_path("Downloads")));
        }
        let roots = roots
            .into_iter()
            .map(|(area, path)| Root {
                area,
                canon: None,
                path,
                watching: false,
                known: BTreeSet::new(),
                tried: None,
            })
            .collect();
        let seen = Seen::load(&env);
        Engine {
            env,
            settings,
            status,
            roots,
            pending: BTreeMap::new(),
            outbox: None,
            held: Vec::new(),
            seen,
            trash_fda_missing: false,
        }
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.outbox = None;
        self.held.clear();
    }

    /// Can the Trash be read? Recent macOS answers "Operation not permitted" without Full
    /// Disk Access.
    fn trash_readable(&self, p: &Path) -> Result<(), io::Error> {
        if matches!(self.env.fault_for(p), Some(Fault::PermissionDenied)) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        fs::read_dir(p).map(|_| ())
    }

    /// Watch the folders that are not watched yet (they may appear later, or Full Disk
    /// Access may be granted), and update the status.
    fn maintain(&mut self, w: &mut impl notify::Watcher, now: Instant, first: bool) {
        let mut notes = Vec::new();
        let mut appeared: Vec<(Area, PathBuf)> = Vec::new();
        for i in 0..self.roots.len() {
            if self.roots[i].watching {
                continue;
            }
            if self.roots[i]
                .tried
                .is_some_and(|t| now.duration_since(t) < RETRY)
            {
                notes.push(self.not_watched_note(i));
                continue;
            }
            self.roots[i].tried = Some(now);
            let (area, path) = (self.roots[i].area, self.roots[i].path.clone());
            if !path.is_dir() {
                notes.push(self.not_watched_note(i));
                continue;
            }
            if area == Area::Trash {
                match self.trash_readable(&path) {
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                        self.trash_fda_missing = true;
                        notes.push(self.not_watched_note(i));
                        continue;
                    }
                    _ => self.trash_fda_missing = false,
                }
            }
            let mode = if area.recursive() {
                notify::RecursiveMode::Recursive
            } else {
                notify::RecursiveMode::NonRecursive
            };
            match w.watch(&path, mode) {
                Ok(()) => {
                    let r = &mut self.roots[i];
                    r.watching = true;
                    r.canon = fs::canonicalize(&path).ok().filter(|c| *c != path);
                    r.known = children(&path).into_iter().collect();
                    // A folder that appears after the start: what is in it is new.
                    if !first {
                        for c in r.known.clone() {
                            appeared.push((area, c));
                        }
                    }
                }
                Err(e) => notes.push(format!("{} is not watched: {e}", self.env.display(&path))),
            }
        }
        for (area, c) in appeared {
            self.add_pending(area, c, now);
        }
        let state = |area_is: &dyn Fn(Area) -> bool, on: bool| -> Access {
            if !on {
                return Access::Off;
            }
            if self.roots.iter().any(|r| area_is(r.area) && r.watching) {
                Access::Watching
            } else {
                Access::Failed("none of its folders can be watched yet".into())
            }
        };
        let mut st = lock(&self.status);
        st.trash = if !self.settings.trash_offer {
            Access::Off
        } else if self.trash_fda_missing {
            Access::NoFullDiskAccess
        } else {
            state(&|a| a == Area::Trash, true)
        };
        st.threats = state(&|a| a != Area::Trash, self.settings.threat_watch);
        st.notes = notes;
    }

    fn not_watched_note(&self, i: usize) -> String {
        let r = &self.roots[i];
        let shown = self.env.display(&r.path);
        if r.area == Area::Trash && self.trash_fda_missing {
            format!("{shown} needs Full Disk Access to be watched")
        } else {
            format!("{shown} does not exist yet")
        }
    }

    /// The one-time event that the Trash cannot be read.
    fn fda_event(&mut self) -> Option<WatchEvent> {
        if !self.trash_fda_missing {
            return None;
        }
        let id = "fda:trash";
        let now = now_secs();
        if self.seen.has(id, now) {
            return None;
        }
        self.seen.add(id, now);
        self.seen.save();
        Some(WatchEvent {
            id: id.into(),
            kind: WatchKind::NeedsFullDiskAccess,
            title: "Trash watch needs Full Disk Access".into(),
            body: "macOS keeps the Trash private. Give CleanCraft Full Disk Access in System Settings to get an offer when you drag an app to the Trash.".into(),
            open: AppRoute::FullDiskAccess,
            time: now,
            paths: vec![self.env.user_trash()],
        })
    }

    /// A path from a file system event: find its item (the app bundle or property list) and
    /// wait for it to settle.
    fn on_path(&mut self, p: &Path, now: Instant) {
        for i in 0..self.roots.len() {
            let r = &self.roots[i];
            if !r.watching {
                continue;
            }
            let rel = p
                .strip_prefix(&r.path)
                .ok()
                .map(Path::to_path_buf)
                .or_else(|| {
                    r.canon
                        .as_ref()
                        .and_then(|c| p.strip_prefix(c).ok().map(Path::to_path_buf))
                });
            let Some(rel) = rel else {
                continue;
            };
            let area = r.area;
            let root = r.path.clone();
            let mut comps = rel.components();
            match comps.next() {
                // The folder itself changed (FSEvents may say only that): look for new entries.
                None => {
                    let now_children: BTreeSet<PathBuf> = children(&root).into_iter().collect();
                    let new: Vec<PathBuf> = now_children
                        .difference(&self.roots[i].known)
                        .cloned()
                        .collect();
                    for c in new {
                        self.add_pending(area, c, now);
                    }
                }
                Some(first) => {
                    let mut item = root.join(first);
                    // An app one folder down (/Applications/Vendor/Tool.app).
                    if area == Area::Apps
                        && !is_app(&item)
                        && let Some(second) = comps.next()
                    {
                        let deeper = item.join(second);
                        if is_app(&deeper) {
                            item = deeper;
                        }
                    }
                    self.add_pending(area, item, now);
                }
            }
            return;
        }
    }

    fn add_pending(&mut self, area: Area, item: PathBuf, now: Instant) {
        let wanted = match area {
            Area::Trash => is_app(&item),
            Area::Apps => is_app(&item),
            Area::Launch(_) => item.extension().is_some_and(|e| e == "plist"),
            // A file still downloading arrives again under its final name.
            Area::Downloads => !item.extension().is_some_and(|e| {
                ["crdownload", "download", "part", "partial"]
                    .iter()
                    .any(|x| e.eq_ignore_ascii_case(x))
            }),
        };
        if !wanted {
            return;
        }
        if area == Area::Trash {
            // Only new names: an app already in the Trash when watching began is old news.
            let known = self
                .roots
                .iter()
                .any(|r| r.area == Area::Trash && r.known.contains(&item));
            if known {
                return;
            }
        }
        let e = self.pending.entry(item).or_insert(Pending {
            area,
            since: now,
            sig: None,
            stable_since: now,
            checked: now.checked_sub(self.settings.debounce).unwrap_or(now),
        });
        // Something changed in it: the quiet time starts again.
        e.stable_since = now;
    }

    /// Settle pending items, look at the ones that stopped changing, and send what is due.
    fn tick(&mut self, now: Instant) -> Vec<WatchEvent> {
        let debounce = self.settings.debounce;
        let every = (debounce / 2).max(Duration::from_millis(20));
        let mut ready = Vec::new();
        let mut gone = Vec::new();
        for (p, pend) in self.pending.iter_mut() {
            if now.duration_since(pend.checked) < every {
                continue;
            }
            pend.checked = now;
            let sig = sig_of(p);
            if sig.is_none() {
                gone.push(p.clone());
                continue;
            }
            if sig != pend.sig {
                pend.sig = sig;
                pend.stable_since = now;
                continue;
            }
            if now.duration_since(pend.stable_since) < debounce {
                continue;
            }
            // A bundle still being copied may not have its Info.plist yet: keep waiting a while.
            if pend.area != Area::Trash
                || apps::read_bundle(p).is_some()
                || now.duration_since(pend.since) >= INCOMPLETE_WAIT
            {
                ready.push((p.clone(), pend.area));
            }
        }
        for p in gone {
            self.pending.remove(&p);
        }
        for (p, area) in ready {
            self.pending.remove(&p);
            // Looked at: a later change of the folder alone does not read it again.
            if matches!(area, Area::Trash | Area::Downloads) {
                for r in self.roots.iter_mut().filter(|r| r.area == area) {
                    r.known.insert(p.clone());
                }
            }
            if let Some(f) = self.examine(&p, area) {
                let now_s = now_secs();
                if self.seen.has(&f.id, now_s) {
                    continue;
                }
                if self
                    .outbox
                    .as_ref()
                    .is_some_and(|(_, v)| v.iter().any(|x| x.id == f.id))
                    || self.held.iter().any(|x| x.id == f.id)
                {
                    continue;
                }
                match &mut self.outbox {
                    Some((_, v)) => v.push(f),
                    None => self.outbox = Some((now, vec![f])),
                }
            }
        }
        let mut out = Vec::new();
        let quiet =
            self.settings.quiet_hours.is_some() && self.settings.is_quiet_at(self.settings.hour());
        if let Some((since, _)) = &self.outbox
            && now.duration_since(*since) >= self.settings.coalesce
        {
            let (_, found) = self.outbox.take().unwrap_or((now, Vec::new()));
            if quiet {
                self.held.extend(found);
            } else {
                out.extend(self.send(found));
            }
        }
        if !quiet && !self.held.is_empty() {
            let held = std::mem::take(&mut self.held);
            out.extend(self.send(held));
        }
        out
    }

    /// Mark the findings seen and turn them into events: one per kind.
    fn send(&mut self, found: Vec<Finding>) -> Vec<WatchEvent> {
        let now = now_secs();
        let mut by_kind: BTreeMap<u8, Vec<Finding>> = BTreeMap::new();
        for f in found {
            if self.seen.has(&f.id, now) {
                continue;
            }
            self.seen.add(&f.id, now);
            let k = match f.kind {
                WatchKind::TrashedApp => 0,
                WatchKind::StartupItem => 1,
                WatchKind::NewApp => 2,
                WatchKind::Download => 3,
                WatchKind::NeedsFullDiskAccess => 4,
            };
            by_kind.entry(k).or_default().push(f);
        }
        self.seen.save();
        by_kind
            .into_values()
            .filter_map(|v| merge(v, now))
            .collect()
    }

    /// Look at one settled item with the rules of its watch. `None`: nothing to say.
    fn examine(&self, p: &Path, area: Area) -> Option<Finding> {
        match area {
            Area::Trash => self.examine_trashed(p),
            Area::Apps => self.examine_app(p),
            Area::Launch(d) => self.examine_launch(p, d),
            Area::Downloads => self.examine_download(p),
        }
    }

    /// Was `p` put there by CleanCraft (a move in the journal), or put back by an Undo?
    fn moved_by_cleancraft(&self, p: &Path) -> bool {
        executor::history(&self.env).iter().any(|r| match &r.kind {
            ActionKind::Moved {
                original, stored, ..
            } => {
                (r.undone.is_none() && (stored == p || p.starts_with(stored)))
                    || (r.undone.is_some() && original == p)
            }
            _ => false,
        })
    }

    fn examine_trashed(&self, p: &Path) -> Option<Finding> {
        if !in_trash(&self.env, p) || self.moved_by_cleancraft(p) {
            return None;
        }
        let app = trashed_app_info(&self.env, p)?;
        if app.apple {
            return None;
        }
        // Another copy is still installed: its files are not leftovers.
        let installed = apps::inventory(&self.env, false, &|_| {});
        if installed
            .iter()
            .any(|a| a.bundle_id.eq_ignore_ascii_case(&app.bundle_id))
        {
            return None;
        }
        let settings = Settings::load(&self.env);
        let plan = trashed_app_plan(&self.env, &settings, p).ok()?;
        let left: Vec<_> = plan
            .items
            .iter()
            .filter(|i| i.category == CAT_HIGH || i.category == CAT_MEDIUM)
            .collect();
        if left.is_empty() {
            return None;
        }
        let bytes: u64 = left.iter().map(|i| i.size).sum();
        let count = left.len();
        Some(Finding {
            id: format!("trash:{}", app.bundle_id.to_lowercase()),
            kind: WatchKind::TrashedApp,
            path: p.to_path_buf(),
            title: format!("{} was moved to the Trash", app.name),
            body: format!(
                "{} it left behind use {}. Review them in App Remover.",
                items(count),
                human_size(bytes)
            ),
            open: AppRoute::UninstallApp(p.to_path_buf()),
            name: app.name,
            count,
            bytes,
        })
    }

    fn examine_launch(&self, p: &Path, d: Domain) -> Option<Finding> {
        if self.moved_by_cleancraft(p) {
            return None;
        }
        let it = startup::launch_item_at(&self.env, d, p, &BTreeMap::new());
        let (reasons, _) = malware::launch_item_findings(&it)?;
        let what = match d {
            Domain::Daemon => "background service",
            _ => "login item",
        };
        let title = if reasons.iter().any(|r| r.starts_with("matches ")) {
            format!("New {what} matches known unwanted software")
        } else if it.program_exists && it.signer == startup::Signer::Unsigned {
            format!("New {what} from an unsigned program")
        } else if it.program_exists && it.signer == startup::Signer::AdHoc {
            format!("New {what} from a program no known developer signed")
        } else {
            format!("New {what} needs a look")
        };
        Some(Finding {
            id: format!("startup:{}:{}", it.plist_shown, short_hash(&reasons)),
            kind: WatchKind::StartupItem,
            path: p.to_path_buf(),
            body: format!(
                "{}: {}. Review it in Threat Check.",
                it.label,
                reasons.join("; ")
            ),
            title,
            open: AppRoute::ThreatReview(p.to_path_buf()),
            name: it.label,
            count: 1,
            bytes: 0,
        })
    }

    fn examine_app(&self, p: &Path) -> Option<Finding> {
        let link = fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
        if link || self.moved_by_cleancraft(p) || apps::macos_app(p) {
            return None;
        }
        let reasons = malware::app_findings(&self.env, p);
        if reasons.is_empty() {
            return None;
        }
        let name = p.file_stem()?.to_string_lossy().into_owned();
        let title = if reasons.iter().any(|r| r.starts_with("matches ")) {
            format!("New app matches known unwanted software: {name}")
        } else {
            format!("New app Gatekeeper rejects: {name}")
        };
        Some(Finding {
            id: format!("app:{}:{}", self.env.display(p), short_hash(&reasons)),
            kind: WatchKind::NewApp,
            path: p.to_path_buf(),
            title,
            body: format!("{}. Review it in Threat Check.", reasons.join("; ")),
            open: AppRoute::ThreatReview(p.to_path_buf()),
            name,
            count: 1,
            bytes: 0,
        })
    }

    /// A download (a file, or a folder read whole) with the deep scan's checks.
    fn examine_download(&self, p: &Path) -> Option<Finding> {
        let link = fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
        if link || self.moved_by_cleancraft(p) {
            return None;
        }
        let rules = xprotect::load(&self.env).ok()?;
        let settings = Settings::load(&self.env);
        let found = xprotect::walk(
            &rules,
            &[p.to_path_buf()],
            &|q| settings.is_ignored(q),
            &|| false,
            &mut |_, _| {},
        );
        let (file, hits) = found.hits.into_iter().next()?;
        let target = malware::hit_target(&file);
        let name = target.file_name()?.to_string_lossy().into_owned();
        let what: Vec<String> = hits.iter().map(|h| h.describe()).collect();
        let inside = file.strip_prefix(&target).unwrap_or(Path::new(""));
        let it = if inside.as_os_str().is_empty() {
            "It".to_string()
        } else {
            format!("{} inside it", inside.display())
        };
        flag(&self.env, &target);
        Some(Finding {
            id: format!(
                "download:{}:{}",
                self.env.display(&target),
                short_hash(&what)
            ),
            kind: WatchKind::Download,
            path: target.clone(),
            title: format!("{name} looks like malware"),
            body: format!(
                "{it} matches {}. Review it in Threat Check.",
                what.join("; ")
            ),
            open: AppRoute::ThreatReview(target),
            name,
            count: 1,
            bytes: 0,
        })
    }
}

/// The watcher's list of flagged downloads (env paths).
const FLAGGED_FILE: &str = "watch-flagged.json";
const MAX_FLAGGED: usize = 200;

/// The downloads the watcher flagged that are still there, oldest first.
pub fn flagged(env: &Env) -> Vec<PathBuf> {
    let list: Vec<PathBuf> = fs::read(env.data_dir().join(FLAGGED_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    list.into_iter()
        .filter(|p| fs::symlink_metadata(p).is_ok())
        .collect()
}

fn flag(env: &Env, p: &Path) {
    let mut list = flagged(env);
    list.retain(|q| q != p);
    list.push(p.to_path_buf());
    let list = &list[list.len().saturating_sub(MAX_FLAGGED)..];
    let path = env.data_dir().join(FLAGGED_FILE);
    let _ = fs::create_dir_all(env.data_dir());
    if let Ok(b) = serde_json::to_vec(list) {
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, b).is_ok() {
            let _ = fs::rename(tmp, &path);
        }
    }
}

fn is_app(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("app"))
}

fn short_hash(reasons: &[String]) -> String {
    let h = blake3::hash(reasons.join("\n").as_bytes());
    h.to_hex()[..12].to_string()
}

/// "a, b and c", "a, b, c and 2 more".
fn names_list(names: &[String]) -> String {
    const SHOWN: usize = 3;
    let shown: Vec<&str> = names.iter().take(SHOWN).map(String::as_str).collect();
    let rest = names.len().saturating_sub(SHOWN);
    match (shown.len(), rest) {
        (0, _) => String::new(),
        (1, 0) => shown[0].to_string(),
        (_, 0) => format!(
            "{} and {}",
            shown[..shown.len() - 1].join(", "),
            shown[shown.len() - 1]
        ),
        (_, r) => format!("{} and {r} more", shown.join(", ")),
    }
}

/// One event for findings of one kind.
fn merge(mut v: Vec<Finding>, now: i64) -> Option<WatchEvent> {
    if v.len() <= 1 {
        let f = v.pop()?;
        return Some(WatchEvent {
            id: f.id,
            kind: f.kind,
            title: f.title,
            body: f.body,
            open: f.open,
            time: now,
            paths: vec![f.path],
        });
    }
    let kind = v[0].kind;
    let names: Vec<String> = v.iter().map(|f| f.name.clone()).collect();
    let n = v.len();
    let (title, body, open) = match kind {
        WatchKind::TrashedApp => {
            let count: usize = v.iter().map(|f| f.count).sum();
            let bytes: u64 = v.iter().map(|f| f.bytes).sum();
            (
                format!("{n} apps were moved to the Trash"),
                format!(
                    "{} left {} behind, using {}. Review them in App Remover.",
                    names_list(&names),
                    items(count),
                    human_size(bytes)
                ),
                AppRoute::Scan(FeatureId::Uninstaller),
            )
        }
        WatchKind::StartupItem => (
            format!("{n} new startup items need a look"),
            format!("{}. Review them in Threat Check.", names_list(&names)),
            AppRoute::Scan(FeatureId::Malware),
        ),
        WatchKind::NewApp => (
            format!("{n} new apps need a look"),
            format!("{}. Review them in Threat Check.", names_list(&names)),
            AppRoute::Scan(FeatureId::Malware),
        ),
        WatchKind::Download => (
            format!("{n} downloads look like malware"),
            format!("{}. Review them in Threat Check.", names_list(&names)),
            AppRoute::Scan(FeatureId::Malware),
        ),
        WatchKind::NeedsFullDiskAccess => return None,
    };
    Some(WatchEvent {
        id: format!(
            "batch:{}",
            short_hash(&v.iter().map(|f| f.id.clone()).collect::<Vec<_>>())
        ),
        kind,
        title,
        body,
        open,
        time: now,
        paths: v.into_iter().map(|f| f.path).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_hours_span_midnight() {
        let s = WatchSettings {
            quiet_hours: Some((22, 7)),
            ..Default::default()
        };
        assert!(s.is_quiet_at(23) && s.is_quiet_at(0) && s.is_quiet_at(6));
        assert!(!s.is_quiet_at(7) && !s.is_quiet_at(12) && !s.is_quiet_at(21));
        let day = WatchSettings {
            quiet_hours: Some((9, 17)),
            ..Default::default()
        };
        assert!(day.is_quiet_at(9) && !day.is_quiet_at(17) && !day.is_quiet_at(8));
        assert!(!WatchSettings::default().is_quiet_at(3));
    }

    #[test]
    fn name_lists() {
        let n = |v: &[&str]| names_list(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(n(&["a"]), "a");
        assert_eq!(n(&["a", "b"]), "a and b");
        assert_eq!(n(&["a", "b", "c"]), "a, b and c");
        assert_eq!(n(&["a", "b", "c", "d", "e"]), "a, b, c and 2 more");
    }
}
