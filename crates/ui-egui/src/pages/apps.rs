//! App Remover and App Updates.
//!
//! App Remover: the app list (search, sort, groups), an app's detail with every related file
//! and a checkbox per file, Uninstall and Reset with a confirmation listing the exact files,
//! then a Done screen with Undo. The groups Leftovers, Installers and Extensions show the App
//! Remover's own scan with Remove Selected.
//!
//! App Updates: Check, then per app "installed -> newest", release notes, and buttons that
//! hand off to the app's own updater, its download page or the App Store; macOS updates open
//! Software Update. CleanCraft never installs software itself.
//!
//! Several apps at once: a checkbox on every app row (Cmd-click toggles, Shift-click selects a
//! range, Space toggles the focused row), "select all" on the group rows, a bar with the
//! apps and the size they move with their leftovers, then one review of every app's files
//! (grouped by app; a running app is locked with its own Quit banner while the others go),
//! one confirmation listing every file, one result screen with Undo All and Undo per app.
//!
//! Keys: Enter in the search box opens the first app shown; Esc leaves an app's detail or
//! cancels the confirmation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cleancraft_core::app_quit;
use cleancraft_core::apps::{self, AppInfo, Store};
use cleancraft_core::executor::{self, Method, Report};
use cleancraft_core::extensions::CAT_EXTENSIONS;
use cleancraft_core::features::{self, leftovers, uninstaller};
use cleancraft_core::fsutil::human_size;
use cleancraft_core::updater::{self, Source, UpdateInfo, UpdatesReport};
use cleancraft_core::{FeatureId, Plan};
use egui::{Align, Color32, Key, Layout, Rect, Sense, Ui, pos2, vec2};

use crate::app::CleanApp;
use crate::icons::{self, Icon};
use crate::jobs::Job;
use crate::kit;
use crate::kit_list::{self, Filter};
use crate::theme::{Weight, font, size, space};
use cleancraft_core::settings::ListPrefs;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Group {
    #[default]
    All,
    Unused,
    AppStore,
    Setapp,
    Other,
    Leftovers,
    Installers,
    Extensions,
}

impl Group {
    pub const LIST: [Group; 8] = [
        Group::All,
        Group::Unused,
        Group::AppStore,
        Group::Setapp,
        Group::Other,
        Group::Leftovers,
        Group::Installers,
        Group::Extensions,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::All => "All Apps",
            Group::Unused => "Unused",
            Group::AppStore => "From App Store",
            Group::Setapp => "From Setapp",
            Group::Other => "From Elsewhere",
            Group::Leftovers => "Leftovers",
            Group::Installers => "Installers",
            Group::Extensions => "Extensions",
        }
    }

    /// The name shown in the narrow group column (the full title is announced).
    pub fn short_title(self) -> &'static str {
        match self {
            Group::AppStore => "App Store",
            Group::Setapp => "Setapp",
            Group::Other => "Other Sources",
            g => g.title(),
        }
    }

    /// The App Remover scan category this group shows, for file groups.
    pub fn category(self) -> Option<&'static str> {
        match self {
            Group::Leftovers => Some(leftovers::CAT_ORPHANS),
            Group::Installers => Some(leftovers::CAT_INSTALLERS),
            Group::Extensions => Some(CAT_EXTENSIONS),
            _ => None,
        }
    }

    fn has(self, a: &AppInfo, now: i64) -> bool {
        match self {
            Group::All => true,
            Group::Unused => a.unused(now) && !a.apple,
            Group::AppStore => a.store == Store::AppStore,
            Group::Setapp => a.store == Store::Setapp,
            Group::Other => a.store == Store::Other,
            _ => false,
        }
    }
}

/// The app list's name in the settings (its filters and sort order).
pub const LIST: &str = "apps";
/// The file groups' list (Leftovers, Installers, Extensions).
pub const FILES_LIST: &str = "apps.files";
/// Kept with the app list's filters: apps that are part of macOS are listed too (they are
/// hidden by default: they cannot be removed).
pub const SHOW_MACOS: &str = "show_macos";
/// "Large": an app of at least this size.
pub const LARGE_APP: u64 = 1_000_000_000;

/// The app list's sort orders.
pub const APP_SORTS: [kit_list::SortKey; 5] = [
    kit_list::BY_NAME,
    kit_list::BY_SIZE,
    kit_list::SortKey {
        key: "opened",
        label: "Sort by last opened",
        first: "Longest unused first",
        second: "Recently opened first",
        descending: false,
    },
    kit_list::SortKey {
        key: "developer",
        label: "Sort by developer",
        first: "A to Z",
        second: "Z to A",
        descending: false,
    },
    kit_list::SortKey {
        key: "installed",
        label: "Sort by install date",
        first: "Newest first",
        second: "Oldest first",
        descending: true,
    },
];

/// What the list knows about each app besides its `AppInfo`, for the filters and sorts:
/// which run, which have files of their own in the Library, and when each was installed.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub running: BTreeSet<PathBuf>,
    pub leftovers: BTreeSet<PathBuf>,
    /// Seconds since the epoch: when the bundle was created on this Mac (else last changed).
    pub installed: BTreeMap<PathBuf, i64>,
    /// When `running` was last asked.
    pub running_at: Option<std::time::Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Uninstall,
    Reset,
    RemoveFiles,
    /// Several apps from the list ([`Batch`]).
    UninstallApps,
}

#[derive(Default)]
pub enum Phase {
    #[default]
    Browse,
    Confirm(Action),
    Working(Job<Result<Report, String>>, Action),
    Done(Report, Action),
}

type Loaded = (Vec<AppInfo>, Result<Plan, String>);

#[derive(Default)]
pub struct State {
    /// The app list, once looked at.
    pub list: Option<Vec<AppInfo>>,
    /// Leftovers of removed apps, installers and extensions.
    pub extras: Option<Plan>,
    load: Option<Job<Loaded>>,
    pub group: Group,
    pub search: String,
    /// The app list's filters and sort (a copy of `Settings::lists["apps"]`, read each frame).
    pub prefs: ListPrefs,
    /// The same for the file groups.
    pub files_prefs: ListPrefs,
    /// Running, has files, install date: filled in the background after the list loads.
    pub facts: Facts,
    facts_job: Option<Job<Facts>>,
    /// The app whose detail is open.
    pub selected: Option<PathBuf>,
    /// Its uninstall plan (checkboxes).
    pub detail: Option<Plan>,
    /// The plan shown in the confirmation and carried out.
    pub pending: Option<Plan>,
    pub phase: Phase,
    pub error: Option<String>,
    pub updates: Option<UpdatesReport>,
    upd_job: Option<Job<UpdatesReport>>,
    pub notes_open: BTreeSet<PathBuf>,
    /// App Updates' search.
    pub upd_search: String,
    /// The open app is running (its files are locked until it quits).
    pub running: bool,
    /// What of it runs: the app, its helpers, agents and services (apps4's app_quit).
    pub parts: Option<app_quit::RunningState>,
    /// Quitting needs the administrator password (a system service): asking first.
    pub confirm_admin: bool,
    /// Quitting the open app from its banner.
    pub quit: Quit,
    /// The Force Quit confirmation is open.
    pub confirm_force: bool,
    /// Background check whether the open app still runs (rows unlock when it quits).
    still_running: Option<Job<app_quit::RunningState>>,
    checked_running: Option<std::time::Instant>,
    /// What the list reflects of CleanCraft's action log (moves made by any page).
    watch: super::history::JournalWatch,
    /// Apps ticked in the list, to uninstall together.
    pub picked: BTreeSet<PathBuf>,
    /// The app last ticked (Shift-click selects from it to the clicked one).
    anchor: Option<PathBuf>,
    /// What each ticked app moves with its usual preselection (bundle and files named
    /// after its id).
    pub pick_sizes: BTreeMap<PathBuf, u64>,
    pick_job: Option<Job<Vec<(PathBuf, u64)>>>,
    /// "Select all N apps?" (the All Apps group row), with the apps it would tick.
    pub confirm_all: Option<Vec<PathBuf>>,
    /// The review of the ticked apps, then their results.
    pub batch: Option<Batch>,
    /// Uninstall was asked for an app whose maker ships its own uninstaller: the question
    /// which one to use is open.
    pub vendor_ask: bool,
    /// The maker's uninstaller was opened: what to do next.
    pub vendor_note: Option<String>,
}

/// The review of several apps: one plan, grouped by app.
pub struct Batch {
    pub plan: Plan,
    pub apps: Vec<uninstaller::BatchApp>,
    /// Apps whose files are folded away.
    pub collapsed: BTreeSet<PathBuf>,
    /// What runs of each running app (helpers, agents, services).
    pub parts: BTreeMap<PathBuf, app_quit::RunningState>,
    /// Apps that did not quit when asked, or could not be quit: what to say.
    pub quit_notes: BTreeMap<PathBuf, (String, String)>,
    /// Quitting these apps (one after the other).
    quitting: Option<(
        Vec<PathBuf>,
        Job<Vec<(PathBuf, Result<QuitOutcome, String>)>>,
    )>,
    /// Force Quit asked for this app: the confirmation is open.
    pub confirm_force: Option<PathBuf>,
    /// Quit asked for these apps, and one runs a system service: the password question.
    pub confirm_admin: Option<Vec<PathBuf>>,
    still_running: Option<Job<Vec<(PathBuf, app_quit::RunningState)>>>,
    checked_running: Option<std::time::Instant>,
    /// After the run: what happened per app.
    pub results: Vec<uninstaller::AppResult>,
    /// Apps put back from the result screen.
    pub undone: BTreeSet<PathBuf>,
    /// Uninstall was asked while some ticked apps still ran: quit them, then uninstall
    /// everything once none of them run any more.
    pub auto_uninstall: bool,
}

/// Quitting the app whose detail is open.
#[derive(Default)]
pub enum Quit {
    #[default]
    Idle,
    /// Asked to quit (or forced); the job waits a few seconds for it to be gone.
    Asking(Job<Result<QuitOutcome, String>>),
    /// Parts of it still run after waiting (a save dialog, or they refused): what to say.
    StillOpen(String),
    Failed(String),
}

/// What a quit or force quit left behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuitOutcome {
    /// Nothing of the app runs any more.
    pub done: bool,
    /// One or two sentences for the banner.
    pub message: String,
}

/// How often the open, running app is checked again.
const RUNNING_CHECK: std::time::Duration = std::time::Duration::from_millis(1500);

impl State {
    pub fn is_loading(&self) -> bool {
        self.load.is_some()
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The apps of `list` that run now (one look at the running processes).
fn running_set(env: &cleancraft_core::Env, list: &[AppInfo]) -> BTreeSet<PathBuf> {
    let running: Vec<PathBuf> = env
        .sys
        .running_apps()
        .into_iter()
        .filter_map(|r| r.path)
        .map(|ap| {
            if env.is_fake() {
                env.sys_path(&ap.to_string_lossy())
            } else {
                ap
            }
        })
        .collect();
    list.iter()
        .filter(|a| running.iter().any(|r| r.starts_with(&a.path)))
        .map(|a| a.path.clone())
        .collect()
}

/// What the filters and sorts need beyond the list (slow part: each app's Library files).
pub fn facts(env: &cleancraft_core::Env, list: &[AppInfo]) -> Facts {
    let installed = list
        .iter()
        .filter_map(|a| {
            let m = std::fs::metadata(&a.path).ok()?;
            let t = m.created().or_else(|_| m.modified()).ok()?;
            let secs = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
            Some((a.path.clone(), secs))
        })
        .collect();
    let leftovers = list
        .iter()
        .filter(|a| !a.apple)
        .filter(|a| {
            uninstaller::find_leftovers(env, a, list)
                .iter()
                .any(|(_, _, c)| *c != uninstaller::Confidence::Shared)
        })
        .map(|a| a.path.clone())
        .collect();
    Facts {
        running: running_set(env, list),
        leftovers,
        installed,
        running_at: Some(std::time::Instant::now()),
    }
}

fn start_facts(app: &mut CleanApp) {
    let Some(list) = app.apps.list.clone() else {
        return;
    };
    let env = app.env.clone();
    app.apps.facts_job = Some(Job::start(app.sync, move |_| facts(&env, &list)));
}

/// Look again at which apps run, at most every few seconds while the list shows.
fn refresh_running(app: &mut CleanApp) {
    let due = app
        .apps
        .facts
        .running_at
        .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(5));
    if !due || app.apps.facts_job.is_some() {
        return;
    }
    if let Some(list) = &app.apps.list {
        app.apps.facts.running = running_set(&app.env, list);
        app.apps.facts.running_at = Some(std::time::Instant::now());
    }
}

// ---------- work ----------

/// Look at the apps again (size, last opened) and the App Remover's own scan.
pub fn start_load(app: &mut CleanApp) {
    let env = app.env.clone();
    let settings = app.settings.clone();
    app.apps.watch.prime(&app.env);
    let job = Job::start_metered(app.sync, move |meter| {
        // Reading the apps is most of the work; the leftovers scan is the rest.
        let list =
            apps::inventory_metered(&env, true, &|n| meter.set_step(n), &meter.range(0.0, 0.8));
        meter.set_step("leftovers and extensions");
        let extras =
            features::scan(&env, &settings, FeatureId::Uninstaller).map_err(|e| format!("{e:#}"));
        (list, extras)
    });
    app.apps.load = Some(job);
}

fn start_updates(app: &mut CleanApp) {
    let env = app.env.clone();
    let job = Job::start_metered(app.sync, move |meter| {
        let list = apps::inventory(&env, false, &|_| {});
        updater::check_all_metered(&env, &list, &|n| meter.set_step(n), meter)
    });
    app.apps.upd_job = Some(job);
    crate::sound::play(&app.env, crate::sound::Cue::ScanStart);
}

/// Open an app's detail with a fresh plan.
pub fn open_app(app: &mut CleanApp, path: &Path) {
    app.apps.selected = Some(path.to_path_buf());
    app.apps.error = None;
    app.apps.quit = Quit::Idle;
    app.apps.confirm_force = false;
    app.apps.vendor_ask = false;
    app.apps.vendor_note = None;
    rebuild_detail(app, None);
}

/// Leave the app's detail for the list.
fn close_detail(app: &mut CleanApp) {
    app.apps.selected = None;
    app.apps.detail = None;
    app.apps.running = false;
    app.apps.parts = None;
    app.apps.quit = Quit::Idle;
    app.apps.confirm_force = false;
    app.apps.confirm_admin = false;
    app.apps.still_running = None;
    app.apps.vendor_ask = false;
    app.apps.vendor_note = None;
}

/// Open the uninstaller the app's maker ships next to it, and say what to do after.
fn open_vendor_uninstaller(app: &mut CleanApp, uninstaller: &Path) {
    let shown = app.env.display(uninstaller);
    let uname = app_name(uninstaller);
    match app.env.sys.spawn("/usr/bin/open", &[&shown]) {
        Ok(()) => {
            app.apps.vendor_note = Some(format!(
                "{uname} is open. When it finishes, click Scan Again to see what it left behind."
            ))
        }
        Err(e) => app.warn(format!("Could not open {uname}: {e}")),
    }
}

/// Recompute the selected app's plan, keeping the user's checkboxes from `keep` (unless
/// `keep` was locked as a whole: then nothing in it was the user's choice).
fn rebuild_detail(app: &mut CleanApp, keep: Option<&Plan>) {
    let Some(path) = app.apps.selected.clone() else {
        return;
    };
    if !path.exists() {
        close_detail(app);
        return;
    }
    app.apps.running = uninstaller::is_running(&app.env, &path);
    app.apps.parts = app
        .apps
        .running
        .then(|| app_quit::running_parts(&app.env, &path));
    app.apps.checked_running = Some(std::time::Instant::now());
    // An app already in the Trash (the drag-to-Trash offer): only what it left behind.
    let plan = if cleancraft_core::watch::in_trash(&app.env, &path) {
        cleancraft_core::watch::trashed_app_plan(&app.env, &app.settings, &path)
    } else {
        uninstaller::uninstall_plan(&app.env, &app.settings, &path, false)
    };
    match plan {
        Ok(mut p) => {
            if let Some(old) = keep.filter(|o| o.notes.is_empty()) {
                copy_selection(old, &mut p);
            }
            app.apps.detail = Some(p);
        }
        Err(e) => {
            app.apps.detail = None;
            app.apps.error = Some(format!("{e:#}"));
        }
    }
}

/// Ask the open app to quit (or force it), then wait a little for it to be gone.
/// `allow_admin`: the person agreed to the password dialog for system services.
fn start_quit(app: &mut CleanApp, path: &Path, force: bool, allow_admin: bool) {
    let env = app.env.clone();
    let p = path.to_path_buf();
    app.apps.quit = Quit::Asking(Job::start(app.sync, move |_| {
        quit_now(&env, &p, force, allow_admin)
    }));
}

/// Quit the app at `path` with everything it runs (apps4's core flow), and say how it went.
fn quit_now(
    env: &cleancraft_core::Env,
    path: &Path,
    force: bool,
    allow_admin: bool,
) -> Result<QuitOutcome, String> {
    // Tests (FakeMac) do not wait: the fake processes stop at once or never.
    let fast = env.is_fake();
    let rep = if force {
        let mut o = app_quit::ForceOptions {
            allow_admin,
            ..Default::default()
        };
        if fast {
            o.grace = std::time::Duration::ZERO;
            o.wait = std::time::Duration::ZERO;
            o.poll = std::time::Duration::from_millis(1);
        }
        app_quit::force_quit(env, path, o)?
    } else {
        let mut o = app_quit::QuitOptions {
            allow_admin,
            ..Default::default()
        };
        if fast {
            o.wait = std::time::Duration::ZERO;
            o.poll = std::time::Duration::from_millis(1);
        }
        app_quit::quit_app(env, path, o)?
    };
    Ok(QuitOutcome {
        done: rep.done(),
        message: rep.message(),
    })
}

/// Forget what moved away (any page, or an agent): apps whose bundle is gone leave the list
/// and their groups, files gone leave the Leftovers, Installers and Extensions groups, so
/// every count is right at once, before the list is looked at again.
pub fn forget_moved(st: &mut State) {
    if let Some(list) = st.list.as_mut() {
        list.retain(|a| a.path.exists());
    }
    st.picked.retain(|p| p.exists());
    st.pick_sizes.retain(|p, _| p.exists());
    if let Some(p) = st.extras.as_mut() {
        p.items
            .retain(|i| std::fs::symlink_metadata(&i.path).is_ok());
        for (k, it) in p.items.iter_mut().enumerate() {
            it.id = k;
        }
    }
    // The Done screen keeps the app for Undo; Done itself returns to the list.
    if matches!(st.phase, Phase::Browse) && st.selected.as_ref().is_some_and(|p| !p.exists()) {
        st.selected = None;
        st.detail = None;
        st.running = false;
        st.quit = Quit::Idle;
    }
}

/// Carry checkbox choices over by path (leftovers only; the app item follows the action).
fn copy_selection(from: &Plan, to: &mut Plan) {
    for i in 0..to.items.len() {
        if to.items[i].category == uninstaller::CAT_APP {
            continue;
        }
        if let Some(o) = from.items.iter().find(|o| o.path == to.items[i].path) {
            to.set_selected(i, o.selected);
        }
    }
}

fn start_action(app: &mut CleanApp, action: Action) {
    let Some(plan) = app.apps.pending.clone() else {
        app.apps.phase = Phase::Browse;
        return;
    };
    let env = app.env.clone();
    let settings = app.settings.clone();
    let digest = plan.digest();
    let batch_apps = (action == Action::UninstallApps)
        .then(|| app.apps.batch.as_ref().map(|b| b.apps.clone()))
        .flatten();
    let job = Job::start_metered(app.sync, move |meter| {
        let mut progress = |n: usize, total: usize| {
            meter.set(n as u64, total as u64);
            meter.set_step(&kit::moving_text(n, total));
        };
        match &batch_apps {
            Some(apps) => {
                uninstaller::run_batch(&env, &settings, &plan, apps, &digest, &mut progress)
            }
            None => features::run_plan(&env, &settings, &plan, &digest, &mut progress),
        }
        .map_err(|e| format!("{e:#}"))
    });
    app.apps.phase = Phase::Working(job, action);
}

/// Finish background work. Returns true while something runs.
pub fn poll(app: &mut CleanApp) -> bool {
    let mut busy = false;
    let st = &mut app.apps;
    if let Some(job) = st.load.as_mut() {
        if let Some((list, extras)) = job.poll() {
            st.load = None;
            st.list = Some(list);
            match extras {
                Ok(p) => st.extras = Some(p),
                Err(e) => {
                    st.extras = None;
                    st.error = Some(e);
                }
            }
            // Anything moved while it looked.
            forget_moved(st);
            // Leftovers may have changed: the ticked apps are measured again.
            st.pick_sizes.clear();
            measure_picked(app);
            start_facts(app);
            return poll(app);
        } else if job.is_dead() {
            st.load = None;
            st.error = Some("Looking at the apps stopped unexpectedly.".into());
        } else {
            busy = true;
        }
    }
    if let Some(job) = st.facts_job.as_mut() {
        if let Some(f) = job.poll() {
            st.facts_job = None;
            st.facts = f;
        } else if job.is_dead() {
            st.facts_job = None;
        } else {
            busy = true;
        }
    }
    if let Some(job) = st.upd_job.as_mut() {
        if let Some(r) = job.poll() {
            st.upd_job = None;
            st.updates = Some(r);
            crate::sound::play(&app.env, crate::sound::Cue::ScanDone);
        } else if job.is_dead() {
            st.upd_job = None;
            st.error = Some("The update check stopped unexpectedly.".into());
        } else {
            busy = true;
        }
    }
    let mut finished = None;
    if let Phase::Working(job, action) = &mut st.phase {
        if let Some(r) = job.poll() {
            finished = Some((r, *action));
        } else if job.is_dead() {
            finished = Some((Err("The removal stopped unexpectedly.".into()), *action));
        } else {
            busy = true;
        }
    }
    if let Some((r, action)) = finished {
        match r {
            Ok(rep) => {
                let cue = if rep.moved.is_empty() && !rep.skipped.is_empty() {
                    crate::sound::Cue::Error
                } else {
                    crate::sound::Cue::CleanDone
                };
                crate::sound::play(&app.env, cue);
                if action == Action::UninstallApps
                    && let (Some(plan), Some(b)) = (&app.apps.pending, app.apps.batch.as_mut())
                {
                    b.results = uninstaller::batch_results(plan, &b.apps, &rep);
                }
                app.apps.phase = Phase::Done(rep, action);
                app.apps.pending = None;
                // What moved leaves the lists and counts now; the slow look below confirms.
                forget_moved(&mut app.apps);
                if let Phase::Done(rep, _) = &app.apps.phase {
                    let m = CleanApp::moved_paths(rep);
                    app.files_moved(&m);
                }
            }
            Err(e) => {
                app.apps.phase = Phase::Browse;
                app.apps.error = Some(e);
                app.apps.pending = None;
                if action == Action::UninstallApps {
                    rebuild_batch(app);
                } else {
                    let old = app.apps.detail.clone();
                    rebuild_detail(app, old.as_ref());
                }
            }
        }
        // The list changed on disk: look again.
        start_load(app);
        busy = true;
    }
    busy |= poll_quit(app);
    busy |= poll_running(app);
    busy |= poll_batch(app);
    // Moves and undos made elsewhere (another page, History, an agent).
    if app.apps.list.is_some()
        && app.apps.load.is_none()
        && !matches!(app.apps.phase, Phase::Working(..))
        && let Some(changes) = app.apps.watch.changes(&app.env)
    {
        let moved = super::history::moved_paths(&changes);
        if !moved.is_empty() {
            forget_moved(&mut app.apps);
            if moved.iter().any(|m| m.2) {
                // Something came back: only a new look finds it.
                start_load(app);
                busy = true;
            }
            if app.apps.selected.is_some() && matches!(app.apps.phase, Phase::Browse) {
                let old = app.apps.detail.clone();
                rebuild_detail(app, old.as_ref());
            }
            if app.apps.batch.is_some() && matches!(app.apps.phase, Phase::Browse) {
                rebuild_batch(app);
            }
        }
    }
    busy
}

fn poll_quit(app: &mut CleanApp) -> bool {
    let Quit::Asking(job) = &mut app.apps.quit else {
        return false;
    };
    let r = if let Some(r) = job.poll() {
        r
    } else if job.is_dead() {
        Err("Quitting stopped unexpectedly.".into())
    } else {
        return true;
    };
    match r {
        Ok(o) if o.done => {
            app.apps.quit = Quit::Idle;
            app.tell(o.message);
        }
        Ok(o) => app.apps.quit = Quit::StillOpen(o.message),
        Err(e) => app.apps.quit = Quit::Failed(e),
    }
    // Fresh defaults: nothing was selectable while it ran.
    rebuild_detail(app, None);
    false
}

/// While the open app runs, look again every moment: when the person quits it themselves
/// (or it quits after a save dialog), its files unlock without a click.
fn poll_running(app: &mut CleanApp) -> bool {
    let st = &mut app.apps;
    if !st.running || st.selected.is_none() || matches!(st.quit, Quit::Asking(_)) {
        st.still_running = None;
        return false;
    }
    if let Some(job) = st.still_running.as_mut() {
        match job.poll() {
            Some(state) if state.locked() => {
                st.still_running = None;
                st.parts = Some(state);
                st.checked_running = Some(std::time::Instant::now());
            }
            Some(_) => {
                st.still_running = None;
                st.quit = Quit::Idle;
                rebuild_detail(app, None);
                let name = app
                    .apps
                    .selected
                    .as_ref()
                    .and_then(|p| p.file_stem())
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                app.tell(format!("{name} has quit. You can uninstall it now."));
            }
            None if job.is_dead() => st.still_running = None,
            None => {}
        }
        return true;
    }
    let due = st
        .checked_running
        .is_none_or(|t| t.elapsed() >= RUNNING_CHECK);
    if due && let Some(path) = st.selected.clone() {
        let env = app.env.clone();
        st.still_running = Some(Job::start(app.sync, move |_| {
            app_quit::running_parts(&env, &path)
        }));
    }
    // Keep frames coming so the next check happens.
    true
}

// ---------- several apps: work ----------

/// Can this app be ticked? Never an app that is part of macOS.
fn pickable(a: &AppInfo) -> bool {
    !a.apple && !uninstaller::is_cleancraft(&a.path)
}

/// Tick or untick apps; the sizes of newly ticked ones are measured in the background.
pub fn set_picked(app: &mut CleanApp, paths: &[PathBuf], on: bool) {
    let allowed: BTreeSet<PathBuf> = app
        .apps
        .list
        .iter()
        .flatten()
        .filter(|a| pickable(a) && !app.settings.is_ignored(&a.path))
        .map(|a| a.path.clone())
        .collect();
    for p in paths {
        if on && allowed.contains(p) {
            app.apps.picked.insert(p.clone());
        } else if !on {
            app.apps.picked.remove(p);
        }
    }
    measure_picked(app);
}

/// Measure the ticked apps not measured yet (one job at a time).
fn measure_picked(app: &mut CleanApp) {
    let st = &mut app.apps;
    st.pick_sizes.retain(|p, _| st.picked.contains(p));
    if st.pick_job.is_some() {
        return;
    }
    let todo: Vec<PathBuf> = st
        .picked
        .iter()
        .filter(|p| !st.pick_sizes.contains_key(*p))
        .cloned()
        .collect();
    if todo.is_empty() {
        return;
    }
    let env = app.env.clone();
    let settings = app.settings.clone();
    app.apps.pick_job = Some(Job::start(app.sync, move |_| {
        uninstaller::preselected_sizes(&env, &settings, &todo)
    }));
}

/// The ticked apps' total, once every one is measured.
pub fn picked_size(st: &State) -> Option<u64> {
    st.picked
        .iter()
        .map(|p| st.pick_sizes.get(p).copied())
        .sum::<Option<u64>>()
}

/// Open the review of the ticked apps (in the list's order).
fn open_batch(app: &mut CleanApp) {
    let order: Vec<PathBuf> = {
        let mut v: Vec<&AppInfo> = app
            .apps
            .list
            .iter()
            .flatten()
            .filter(|a| app.apps.picked.contains(&a.path))
            .collect();
        v.sort_by_key(|a| std::cmp::Reverse(app.apps.pick_sizes.get(&a.path).copied()));
        v.into_iter().map(|a| a.path.clone()).collect()
    };
    match uninstaller::batch_plan(&app.env, &app.settings, &order) {
        Ok((plan, apps)) => {
            let parts = running_parts_of(&app.env, &apps);
            app.apps.batch = Some(Batch {
                plan,
                apps,
                collapsed: BTreeSet::new(),
                parts,
                quit_notes: BTreeMap::new(),
                quitting: None,
                confirm_force: None,
                confirm_admin: None,
                still_running: None,
                checked_running: Some(std::time::Instant::now()),
                results: Vec::new(),
                undone: BTreeSet::new(),
                auto_uninstall: false,
            });
            app.apps.error = None;
        }
        Err(e) => app.apps.error = Some(format!("{e:#}")),
    }
}

fn running_parts_of(
    env: &cleancraft_core::Env,
    apps: &[uninstaller::BatchApp],
) -> BTreeMap<PathBuf, app_quit::RunningState> {
    apps.iter()
        .filter(|a| a.running)
        .map(|a| (a.path.clone(), app_quit::running_parts(env, &a.path)))
        .collect()
}

/// Build the review again (an app quit, Scan Again, an app left out), keeping the person's
/// checkboxes for the apps that were not locked. Apps gone from the disk leave it.
fn rebuild_batch(app: &mut CleanApp) {
    let Some(b) = app.apps.batch.as_mut() else {
        return;
    };
    let paths: Vec<PathBuf> = b
        .apps
        .iter()
        .filter(|a| a.path.exists())
        .map(|a| a.path.clone())
        .collect();
    if paths.is_empty() {
        app.apps.batch = None;
        return;
    }
    match uninstaller::batch_plan(&app.env, &app.settings, &paths) {
        Ok((mut plan, apps)) => {
            let old = &b.plan;
            for i in 0..plan.items.len() {
                if plan.items[i].category == uninstaller::CAT_APP {
                    continue;
                }
                // Choices made while the app was locked do not exist: its defaults stay.
                let was_locked = b.apps.iter().any(|a| {
                    a.lock.is_some() && plan.items[i].group.as_deref() == Some(a.group.as_str())
                });
                if was_locked {
                    continue;
                }
                if let Some(o) = old.items.iter().find(|o| o.path == plan.items[i].path) {
                    plan.set_selected(i, o.selected);
                }
            }
            b.parts = running_parts_of(&app.env, &apps);
            b.plan = plan;
            b.apps = apps;
            b.checked_running = Some(std::time::Instant::now());
        }
        Err(e) => app.apps.error = Some(format!("{e:#}")),
    }
}

/// Leave an app out of the review (and untick it in the list).
fn leave_out(app: &mut CleanApp, path: &Path) {
    app.apps.picked.remove(path);
    app.apps.pick_sizes.remove(path);
    if let Some(b) = app.apps.batch.as_mut() {
        b.apps.retain(|a| a.path != path);
        b.parts.remove(path);
        b.quit_notes.remove(path);
    }
    rebuild_batch(app);
}

/// Quit (or force quit) these apps of the review, one after the other.
fn start_batch_quit(app: &mut CleanApp, paths: Vec<PathBuf>, force: bool, allow_admin: bool) {
    let env = app.env.clone();
    let list = paths.clone();
    let job = Job::start(app.sync, move |status| {
        list.iter()
            .map(|p| {
                status(&p.file_stem().unwrap_or_default().to_string_lossy());
                (p.clone(), quit_now(&env, p, force, allow_admin))
            })
            .collect()
    });
    if let Some(b) = app.apps.batch.as_mut() {
        for p in &paths {
            b.quit_notes.remove(p);
        }
        b.quitting = Some((paths, job));
    }
}

/// Ask to quit these apps: first the password question when one runs a system service.
fn ask_batch_quit(app: &mut CleanApp, paths: Vec<PathBuf>) {
    let admin = app.apps.batch.as_ref().is_some_and(|b| {
        paths
            .iter()
            .any(|p| b.parts.get(p).is_some_and(|s| s.needs_admin()))
    });
    if admin {
        if let Some(b) = app.apps.batch.as_mut() {
            b.confirm_admin = Some(paths);
        }
    } else {
        start_batch_quit(app, paths, false, false);
    }
}

fn app_name(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// After a quit asked for by the Uninstall button (not the separate Quit All banner): once
/// the quitting is done, whatever that freed up, carry on to the confirmation on its own
/// (the same as Quit All followed by a click on Uninstall, just in one step). An app that
/// refuses to quit stays running and out of the plan; the others still go.
fn maybe_auto_uninstall(app: &mut CleanApp) {
    let Some(b) = app.apps.batch.as_ref() else {
        return;
    };
    if !b.auto_uninstall || b.quitting.is_some() {
        return;
    }
    let ready = b.plan.selected_count() > 0;
    if let Some(b) = app.apps.batch.as_mut() {
        b.auto_uninstall = false;
    }
    if ready {
        app.apps.pending = app.apps.batch.as_ref().map(|b| b.plan.clone());
        app.apps.phase = Phase::Confirm(Action::UninstallApps);
    }
}

/// Quitting and the running check of the review. Returns true while something runs.
fn poll_batch(app: &mut CleanApp) -> bool {
    let mut busy = false;
    if let Some(job) = app.apps.pick_job.as_mut() {
        if let Some(v) = job.poll() {
            app.apps.pick_job = None;
            for (p, n) in v {
                app.apps.pick_sizes.insert(p, n);
            }
            // Ticked while it measured.
            measure_picked(app);
        } else if job.is_dead() {
            app.apps.pick_job = None;
        } else {
            busy = true;
        }
    }
    if !matches!(app.apps.phase, Phase::Browse | Phase::Confirm(_)) {
        return busy;
    }
    let Some(b) = app.apps.batch.as_mut() else {
        return busy;
    };
    // Quitting.
    let mut quit_done = None;
    if let Some((_, job)) = b.quitting.as_mut() {
        if let Some(r) = job.poll() {
            quit_done = Some(r);
        } else if job.is_dead() {
            let paths = b.quitting.take().map(|q| q.0).unwrap_or_default();
            quit_done = Some(
                paths
                    .into_iter()
                    .map(|p| (p, Err("Quitting stopped unexpectedly.".to_string())))
                    .collect(),
            );
        } else {
            return true;
        }
    }
    if let Some(r) = quit_done {
        b.quitting = None;
        let mut quit = Vec::new();
        for (p, o) in r {
            let name = app_name(&p);
            match o {
                Ok(o) if o.done => quit.push(name),
                Ok(o) => {
                    b.quit_notes
                        .insert(p, (format!("{name} did not quit"), o.message));
                }
                Err(e) => {
                    b.quit_notes
                        .insert(p, (format!("{name} could not be quit"), e));
                }
            }
        }
        rebuild_batch(app);
        if !quit.is_empty() {
            app.tell(format!(
                "{} quit. {} can be uninstalled now.",
                join_names(&quit),
                if quit.len() == 1 { "It" } else { "They" }
            ));
        }
        maybe_auto_uninstall(app);
        return busy;
    }
    // While an app of the review runs, look again every moment: when it quits by itself its
    // files unlock without a click.
    let running: Vec<PathBuf> = b
        .apps
        .iter()
        .filter(|a| a.running)
        .map(|a| a.path.clone())
        .collect();
    if running.is_empty() {
        b.still_running = None;
        return busy;
    }
    if let Some(job) = b.still_running.as_mut() {
        match job.poll() {
            Some(states) => {
                b.still_running = None;
                b.checked_running = Some(std::time::Instant::now());
                let gone: Vec<String> = states
                    .iter()
                    .filter(|(_, s)| !s.locked())
                    .map(|(p, _)| app_name(p))
                    .collect();
                for (p, s) in states {
                    if s.locked() {
                        b.parts.insert(p, s);
                    } else {
                        b.quit_notes.remove(&p);
                    }
                }
                if !gone.is_empty() {
                    rebuild_batch(app);
                    app.tell(format!(
                        "{} {} quit. You can uninstall {} now.",
                        join_names(&gone),
                        if gone.len() == 1 { "has" } else { "have" },
                        if gone.len() == 1 { "it" } else { "them" }
                    ));
                    maybe_auto_uninstall(app);
                }
            }
            None if job.is_dead() => b.still_running = None,
            None => {}
        }
        return true;
    }
    let due = b
        .checked_running
        .is_none_or(|t| t.elapsed() >= RUNNING_CHECK);
    if due {
        let env = app.env.clone();
        b.still_running = Some(Job::start(app.sync, move |_| {
            running
                .iter()
                .map(|p| (p.clone(), app_quit::running_parts(&env, p)))
                .collect()
        }));
    }
    true
}

/// "Editor", "Editor and Viewer", "Editor, Viewer and Notes".
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [a] => a.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

// ---------- page ----------

pub fn ui(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    let t = app.t;
    let (icon, _) = kit::feature_look(&t, f);
    let sub = if f == FeatureId::Updater {
        "Newer versions of your apps and of macOS. CleanCraft opens the right place to install them."
    } else {
        "Remove apps with the files they leave behind, or reset an app to a fresh start."
    };
    // The updates start screen has its own large title.
    let hero = f == FeatureId::Updater && app.apps.updates.is_none() && app.apps.upd_job.is_none();
    if !hero {
        kit::header(ui, &t, icon, f.title(), sub);
    }
    if let Some(e) = app.apps.error.clone() {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(egui::Vec2::splat(16.0), Sense::hover());
            icons::paint(ui.painter(), r, Icon::Warning, t.danger);
            ui.label(crate::rich(&e).color(t.danger));
            if kit::ghost(ui, &t, "Hide Message", None).clicked() {
                app.act();
                app.apps.error = None;
            }
        });
        ui.add_space(space::S);
    }
    if f == FeatureId::Updater {
        updates_page(app, ui);
    } else {
        remover_page(app, ui);
    }
}

fn remover_page(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    if app.apps.list.is_none() && app.apps.load.is_none() && app.apps.error.is_none() {
        start_load(app);
        poll(app);
    }
    match &app.apps.phase {
        Phase::Working(job, _) => {
            let s = job.status();
            kit::progress(
                ui,
                &t,
                FeatureId::Uninstaller,
                "Moving to the Trash...",
                &s,
                &job.meter.clone(),
                false,
            );
            return;
        }
        Phase::Done(_, Action::UninstallApps) => {
            batch_done(app, ui);
            return;
        }
        Phase::Done(..) => {
            done(app, ui);
            return;
        }
        _ => {}
    }
    if app.apps.list.is_none() {
        if let Some(job) = &app.apps.load {
            let s = job.status();
            kit::progress(
                ui,
                &t,
                FeatureId::Uninstaller,
                "Looking at your apps",
                &s,
                &job.meter.clone(),
                false,
            );
        } else if kit::primary(ui, &t, "Scan Again") {
            app.act();
            app.apps.error = None;
            start_load(app);
        }
        return;
    }
    if app.apps.selected.is_some() {
        detail(app, ui);
    } else if app.apps.batch.is_some() {
        batch_review(app, ui);
    } else {
        browse(app, ui);
    }
    match app.apps.phase {
        Phase::Confirm(Action::UninstallApps) => batch_confirm(app, ui),
        Phase::Confirm(action) => confirm(app, ui, action),
        _ => {}
    }
    if app.apps.confirm_all.is_some() {
        confirm_select_all(app, ui);
    }
}

/// The apps listed: in the chosen group and matching the search, never an app on the ignore
/// list (Settings): CleanCraft does not offer to remove what the user asked it to leave alone.
fn visible_apps(st: &State, settings: &cleancraft_core::Settings) -> Vec<AppInfo> {
    let now = now();
    let q = kit_list::query(&st.search);
    let filters = app_filters(st, settings);
    let mut v: Vec<AppInfo> = st
        .list
        .iter()
        .flatten()
        .filter(|a| st.group.has(a, now) && listed(st, a))
        .filter(|a| !settings.is_ignored(&a.path))
        .filter(|a| q.is_empty() || matches_search(a, &q))
        .filter(|a| kit_list::passes(&st.prefs, &filters, |k| app_has(st, a, k, now)))
        .cloned()
        .collect();
    sort_apps(st, &mut v);
    v
}

fn hit_field() -> f32 {
    crate::theme::hit::FIELD
}

fn macos_line(n_mac: usize, showing: bool) -> String {
    let apps = if n_mac == 1 { "app" } else { "apps" };
    if showing {
        format!("{n_mac} {apps} of macOS shown, locked.")
    } else {
        format!("{n_mac} macOS {apps} hidden")
    }
}

const SHOW_MACOS_LABEL: &str = "Show apps that are part of macOS";

/// The room the macOS apps line and its switch take.
fn macos_toggle_width(ui: &Ui, n_mac: usize, showing: bool) -> f32 {
    let t = kit::tokens(ui.ctx());
    let line = kit::galley(
        ui,
        &macos_line(n_mac, showing),
        font(size::FOOTNOTE, Weight::Regular),
        t.text,
    )
    .size()
    .x;
    let check = kit::galley(
        ui,
        SHOW_MACOS_LABEL,
        font(size::CALLOUT, Weight::Medium),
        t.text,
    )
    .size()
    .x;
    line + space::S + check + 32.0
}

/// "2 macOS apps hidden" and the switch to show them. True when it was switched.
fn macos_toggle(ui: &mut Ui, t: &crate::theme::Tokens, n_mac: usize, showing: bool) -> bool {
    ui.spacing_mut().item_spacing.x = space::S;
    kit::dim(ui, t, &macos_line(n_mac, showing));
    let mut on = showing;
    kit::check_text(ui, t, &mut on, SHOW_MACOS_LABEL).changed()
}

/// Apps that are part of macOS are listed only when the person asked for them.
fn listed(st: &State, a: &AppInfo) -> bool {
    !a.apple || show_macos(st)
}

fn show_macos(st: &State) -> bool {
    kit_list::on(&st.prefs, SHOW_MACOS)
}

/// Does app `a` match the filter `key`?
fn app_has(st: &State, a: &AppInfo, key: &str, now: i64) -> bool {
    match key {
        "app_store" => a.store == Store::AppStore,
        "setapp" => a.store == Store::Setapp,
        "other" => a.store == Store::Other,
        "unused" => a.unused(now) && !a.apple,
        "running" => st.facts.running.contains(&a.path),
        "large" => a.size >= LARGE_APP,
        "leftovers" => st.facts.leftovers.contains(&a.path),
        "selected" => st.picked.contains(&a.path),
        _ => false,
    }
}

/// The app list's filters, each with how many apps of the open group it matches.
fn app_filters(st: &State, settings: &cleancraft_core::Settings) -> Vec<Filter> {
    let now = now();
    let pool: Vec<&AppInfo> = st
        .list
        .iter()
        .flatten()
        .filter(|a| st.group.has(a, now) && listed(st, a) && !settings.is_ignored(&a.path))
        .collect();
    let n = |k: &str| pool.iter().filter(|a| app_has(st, a, k, now)).count();
    vec![
        Filter::any("Source", "app_store", "App Store", n("app_store")),
        Filter::any("Source", "setapp", "Setapp", n("setapp")),
        Filter::any("Source", "other", "Other sources", n("other")),
        Filter::only("Show only", "unused", "Unused", n("unused")),
        Filter::only("Show only", "running", "Running", n("running")),
        Filter::only("Show only", "large", "Large (1 GB or more)", n("large")),
        Filter::only(
            "Show only",
            "leftovers",
            "Has files in Library",
            n("leftovers"),
        ),
        Filter::only("Show only", "selected", "Selected", n("selected")),
    ]
}

/// A date compare that keeps unknown dates last in either direction.
fn by_date(a: Option<i64>, b: Option<i64>, desc: bool, rev: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (Some(x), Some(y)) => {
            let o = if desc { y.cmp(&x) } else { x.cmp(&y) };
            if rev { o.reverse() } else { o }
        }
        (Some(_), None) => Less,
        (None, Some(_)) => Greater,
        (None, None) => Equal,
    }
}

fn sort_apps(st: &State, v: &mut [AppInfo]) {
    let (key, rev) = kit_list::sort_of(&st.prefs, &APP_SORTS);
    let flip = |o: std::cmp::Ordering| if rev { o.reverse() } else { o };
    let name = |a: &AppInfo| a.name.to_lowercase();
    v.sort_by(|a, b| {
        let o = match key.key {
            "name" => flip(name(a).cmp(&name(b))),
            // Longest unopened first; unknown dates last.
            "opened" => by_date(a.last_used, b.last_used, false, rev),
            "developer" => {
                // Apps with no known developer go last.
                match (a.vendor.is_empty(), b.vendor.is_empty()) {
                    (false, true) => std::cmp::Ordering::Less,
                    (true, false) => std::cmp::Ordering::Greater,
                    _ => flip(a.vendor.to_lowercase().cmp(&b.vendor.to_lowercase())),
                }
            }
            "installed" => by_date(
                st.facts.installed.get(&a.path).copied(),
                st.facts.installed.get(&b.path).copied(),
                true,
                rev,
            ),
            _ => flip(b.size.cmp(&a.size)),
        };
        o.then_with(|| name(a).cmp(&name(b)))
    });
}

/// Does file item `it` of a file group match the filter `key`?
fn file_has(it: &cleancraft_core::plan::Item, key: &str) -> bool {
    match key {
        "selected" => it.selected,
        "unselected" => !it.selected && it.locked.is_none(),
        "locked" => it.locked.is_some(),
        _ => false,
    }
}

/// The file groups' filters, counted in the open group.
fn file_filters(st: &State) -> Vec<Filter> {
    let (Some(plan), Some(cat)) = (&st.extras, st.group.category()) else {
        return Vec::new();
    };
    let items: Vec<_> = plan.items.iter().filter(|i| i.category == cat).collect();
    let n = |k: &str| items.iter().filter(|i| file_has(i, k)).count();
    vec![
        Filter::any("Show", "selected", "Selected", n("selected")),
        Filter::any("Show", "unselected", "Not selected", n("unselected")),
        Filter::any("Show", "locked", "Locked", n("locked")),
    ]
}

fn visible_files(st: &State) -> Vec<usize> {
    let (Some(plan), Some(cat)) = (&st.extras, st.group.category()) else {
        return Vec::new();
    };
    let q = kit_list::query(&st.search);
    let filters = file_filters(st);
    let mut v: Vec<usize> = plan
        .items
        .iter()
        .filter(|i| i.category == cat)
        .filter(|i| {
            kit_list::matches(&q, &[&i.path.to_string_lossy(), &i.reason])
                && kit_list::passes(&st.files_prefs, &filters, |k| file_has(i, k))
        })
        .map(|i| i.id)
        .collect();
    let (key, rev) = kit_list::sort_of(&st.files_prefs, &FILE_SORTS);
    let name = |i: usize| {
        plan.items[i]
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    v.sort_by(|a, b| {
        let o = match key.key {
            "name" => name(*a).cmp(&name(*b)),
            _ => plan.items[*b].size.cmp(&plan.items[*a].size),
        };
        if rev { o.reverse() } else { o }
    });
    v
}

/// The file groups' sort orders.
pub const FILE_SORTS: [kit_list::SortKey; 2] = [kit_list::BY_SIZE, kit_list::BY_NAME];

fn group_count(st: &State, g: Group) -> (usize, u64) {
    if let Some(cat) = g.category() {
        return st
            .extras
            .as_ref()
            .map(|p| (p.category_count(cat), p.category_size(cat)))
            .unwrap_or((0, 0));
    }
    let now = now();
    st.list
        .iter()
        .flatten()
        .filter(|a| g.has(a, now) && listed(st, a))
        .fold((0, 0), |(n, s), a| (n + 1, s + a.size))
}

/// Apps that are part of macOS and hidden from the list (0 while they are shown).
fn macos_hidden(st: &State) -> usize {
    if show_macos(st) {
        return 0;
    }
    st.list.iter().flatten().filter(|a| a.apple).count()
}

/// The apps of group `g` that can be ticked (not part of macOS, not on the ignore list).
/// While searching or filtering the open group, only the apps shown. Never an app that is
/// part of macOS, shown or not.
fn group_apps(st: &State, settings: &cleancraft_core::Settings, g: Group) -> Vec<PathBuf> {
    let now = now();
    let q = kit_list::query(&st.search);
    let filters = app_filters(st, settings);
    st.list
        .iter()
        .flatten()
        .filter(|a| g.has(a, now) && pickable(a) && !settings.is_ignored(&a.path))
        .filter(|a| {
            st.group != g
                || ((q.is_empty() || matches_search(a, &q))
                    && kit_list::passes(&st.prefs, &filters, |k| app_has(st, a, k, now)))
        })
        .map(|a| a.path.clone())
        .collect()
}

fn matches_search(a: &AppInfo, q: &str) -> bool {
    a.name.to_lowercase().contains(q)
        || a.bundle_id.to_lowercase().contains(q)
        || a.vendor.to_lowercase().contains(q)
}

/// The "select all in this group" box of a group row: (enabled, on, mixed, name).
fn group_check(
    st: &State,
    settings: &cleancraft_core::Settings,
    g: Group,
) -> Option<(bool, bool, bool, String)> {
    let name = format!("Select all in {}", g.short_title());
    if let Some(cat) = g.category() {
        let plan = st.extras.as_ref()?;
        let free: Vec<_> = plan
            .items
            .iter()
            .filter(|i| i.category == cat && i.locked.is_none())
            .collect();
        let on = free.iter().filter(|i| i.selected).count();
        return Some((
            !free.is_empty(),
            !free.is_empty() && on == free.len(),
            on > 0 && on < free.len(),
            name,
        ));
    }
    let apps = group_apps(st, settings, g);
    let on = apps.iter().filter(|p| st.picked.contains(*p)).count();
    Some((
        !apps.is_empty(),
        !apps.is_empty() && on == apps.len(),
        on > 0 && on < apps.len(),
        name,
    ))
}

/// Tick or untick a whole group. Ticking every app (All Apps) asks first, with the count.
fn select_group(app: &mut CleanApp, g: Group, on: bool) {
    if let Some(cat) = g.category() {
        if let Some(p) = app.apps.extras.as_mut() {
            p.set_category_selected(cat, on);
        }
        return;
    }
    let apps = group_apps(&app.apps, &app.settings, g);
    if on && g == Group::All {
        app.apps.confirm_all = Some(apps);
        return;
    }
    set_picked(app, &apps, on);
}

/// "Select all 130 apps?": ticking every app is never one click.
fn confirm_select_all(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let Some(apps) = app.apps.confirm_all.clone() else {
        return;
    };
    let n = apps.len();
    let (mut go, mut close) = (false, false);
    let resp = kit::modal(egui::Id::new("apps-select-all"), &t).show(ui.ctx(), |ui| {
        ui.set_width(460.0);
        kit::dialog_head(
            ui,
            &t,
            Icon::Remover,
            t.sec_a,
            &format!("Select all {n} apps?"),
            "Every app listed is ticked for removal. Apps that are part of macOS stay unticked. Nothing is removed until you review and confirm.",
        );
        ui.add_space(space::L);
        let (g, c) = kit::dialog_actions(ui, &t, &format!("Select {n} Apps"), kit::Kind::Primary);
        go = g;
        close = c;
    });
    if resp.should_close() {
        close = true;
    }
    if go {
        app.act();
        app.apps.confirm_all = None;
        set_picked(app, &apps, true);
    } else if close {
        app.act();
        app.apps.confirm_all = None;
    }
}

/// The ticked apps: how many, what they move with their leftovers, and the way on.
fn apps_bottom_bar(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let n = app.apps.picked.len();
    // Ticked apps the search, filters or group do not show still go: say so.
    // (The group column only navigates: a ticked app in another group is not hidden.)
    let hidden = {
        let st = &app.apps;
        let q = kit_list::query(&st.search);
        let filters = app_filters(st, &app.settings);
        let now = now();
        st.list
            .iter()
            .flatten()
            .filter(|a| st.picked.contains(&a.path))
            .filter(|a| {
                !listed(st, a)
                    || !(q.is_empty() || matches_search(a, &q))
                    || !kit_list::passes(&st.prefs, &filters, |k| app_has(st, a, k, now))
            })
            .count()
    };
    let apps_text = kit_list::selected_text(
        &if n == 1 {
            "1 app selected".to_string()
        } else {
            format!("{n} apps selected")
        },
        hidden,
    );
    let size = picked_size(&app.apps);
    let (figure, full) = match size {
        Some(s) => (
            human_size(s),
            format!("{apps_text}: {} to move, leftovers included", human_size(s)),
        ),
        None => ("...".to_string(), format!("{apps_text}, measuring")),
    };
    kit::bottom_bar(ui, &t, egui::Id::new("apps-picked-bottom"), |ui| {
        kit::bar_figure(
            ui,
            &t,
            &figure,
            &format!("{apps_text}, leftovers included"),
            &full,
            size.is_none(),
        );
        ui.add_space(space::M);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if kit::primary(ui, &t, "Review and Uninstall") {
                app.act();
                open_batch(app);
            }
            if kit::secondary(ui, &t, "Clear Selection") {
                app.act();
                app.apps.picked.clear();
                app.apps.pick_sizes.clear();
                app.apps.anchor = None;
            }
            // Short enough to show whole (U7-18); the rest in its tooltip.
            kit::bar_hint(ui, &t, "Shift-click selects a range.").on_hover_text(crate::tr(
                "Each app's files are listed before anything moves.",
            ));
        });
    });
}

fn browse(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    // The headline every results page has (REVIEW-C C20): what the apps take, how many,
    // and Scan Again on the right.
    // The remembered filters and sort (Settings is the one place they live).
    app.apps.prefs = kit_list::prefs(&app.settings, LIST);
    app.apps.files_prefs = kit_list::prefs(&app.settings, FILES_LIST);
    refresh_running(app);
    let (n_apps, apps_size) = group_count(&app.apps, Group::All);
    let n_mac_hidden = macos_hidden(&app.apps);
    let loading = app.apps.load.is_some();
    ui.horizontal_top(|ui| {
        kit::display_count(ui, &t, "apps-total", apps_size);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.add_space(kit::baseline_offset(size::DISPLAY, size::TITLE3));
            ui.label(
                // The apps themselves; their leftovers are measured when ticked (U7-18).
                crate::rich({
                    let apps = if n_apps == 1 {
                        "in 1 app (apps only)".to_string()
                    } else {
                        format!("in {n_apps} apps (apps only)")
                    };
                    if n_mac_hidden > 0 {
                        format!("{apps}, {n_mac_hidden} from macOS hidden")
                    } else {
                        apps
                    }
                })
                .font(font(size::TITLE3, Weight::Medium))
                .color(t.text_dim),
            );
        });
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), kit::DISPLAY_ROW_H),
            Layout::right_to_left(Align::Center),
            |ui| {
                if loading {
                    let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                    let time = ui.input(|i| i.time);
                    kit::progress_ring(ui.painter(), r.center(), 16.0, &t, time);
                    ui.ctx().request_repaint();
                } else if kit::secondary(ui, &t, "Scan Again") {
                    app.act();
                    start_load(app);
                }
            },
        );
    });
    ui.add_space(space::M);
    // Toolbar: search, Filter, Sort (the file groups have their own filters).
    let file_group = app.apps.group.category().is_some();
    let list = if file_group { FILES_LIST } else { LIST };
    let filters = if file_group {
        file_filters(&app.apps)
    } else {
        app_filters(&app.apps, &app.settings)
    };
    let sorts: &[kit_list::SortKey] = if file_group { &FILE_SORTS } else { &APP_SORTS };
    let mut prefs = if file_group {
        app.apps.files_prefs.clone()
    } else {
        app.apps.prefs.clone()
    };
    let mut prefs_changed = false;
    // Apps of macOS in the open group (hidden unless asked for).
    let n_mac = if file_group {
        0
    } else {
        app.apps
            .list
            .iter()
            .flatten()
            .filter(|a| a.apple && app.apps.group.has(a, now()))
            .count()
    };
    let mut mac_inline = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = space::S;
        let (r, changed) = kit_list::search(
            ui,
            &t,
            &mut app.apps.search,
            "Search apps",
            "Search apps",
            240.0,
        );
        if changed {
            app.act();
        }
        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            app.act();
            if app.apps.group.category().is_none()
                && let Some(first) = visible_apps(&app.apps, &app.settings).first()
            {
                let p = first.path.clone();
                open_app(app, &p);
                return;
            }
        }
        if kit_list::menus(ui, &t, list, &filters, sorts, &mut prefs) {
            app.act();
            prefs_changed = true;
        }
        // The macOS apps switch at the end of the toolbar when it fits, else on its own
        // line under it.
        if n_mac > 0 {
            let showing = kit_list::on(&prefs, SHOW_MACOS);
            let need = macos_toggle_width(ui, n_mac, showing) + space::L;
            if ui.available_width() >= need {
                mac_inline = true;
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Laid out right to left: drawn in reading order by a child.
                    ui.allocate_ui_with_layout(
                        vec2(need - space::L, hit_field()),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            if macos_toggle(ui, &t, n_mac, showing) {
                                app.act();
                                kit_list::toggle(&mut prefs, SHOW_MACOS);
                                prefs_changed = true;
                            }
                        },
                    );
                });
            }
        }
    });
    if app.apps.selected.is_some() {
        return;
    }
    // What the search and filters hide, and the apps of macOS left out of the list.
    let (shown, total) = if file_group {
        let cat = app.apps.group.category().unwrap_or_default();
        (
            visible_files(&app.apps).len(),
            app.apps
                .extras
                .as_ref()
                .map(|p| p.category_count(cat))
                .unwrap_or(0),
        )
    } else {
        (
            visible_apps(&app.apps, &app.settings).len(),
            group_count(&app.apps, app.apps.group).0,
        )
    };
    let noun = if file_group {
        ("item", "items")
    } else {
        ("app", "apps")
    };
    let st = kit_list::status(
        ui,
        &t,
        shown,
        total,
        noun,
        &filters,
        &mut prefs,
        &mut app.apps.search,
    );
    if st.any() {
        app.act();
        prefs_changed |= st.prefs;
    }
    if !file_group && n_mac > 0 && !mac_inline {
        ui.horizontal(|ui| {
            if macos_toggle(ui, &t, n_mac, kit_list::on(&prefs, SHOW_MACOS)) {
                app.act();
                kit_list::toggle(&mut prefs, SHOW_MACOS);
                prefs_changed = true;
            }
        });
    }
    if prefs_changed {
        if kit_list::store(&mut app.settings, list, prefs.clone()) {
            app.save_settings();
        }
        if file_group {
            app.apps.files_prefs = prefs;
        } else {
            app.apps.prefs = prefs;
        }
    }
    ui.add_space(8.0);
    if file_group {
        files_bottom_bar(app, ui);
    } else if !app.apps.picked.is_empty() {
        apps_bottom_bar(app, ui);
    }
    let avail_h = ui.available_height() - 8.0;
    let body =
        egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), avail_h));
    let left_w = if body.width() < 760.0 { 236.0 } else { 272.0 };
    let left = egui::Rect::from_min_size(body.min, egui::vec2(left_w, avail_h));
    let right = egui::Rect::from_min_max(
        egui::pos2(left.right() + space::GUTTER, body.top()),
        body.max,
    );
    ui.allocate_rect(body, egui::Sense::hover());
    ui.scope_builder(egui::UiBuilder::new().max_rect(left), |ui| {
        ui.set_clip_rect(left.expand(24.0).intersect(ui.clip_rect()));
        kit::card(&t)
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.set_min_size(egui::vec2(left_w - 18.0, avail_h - 18.0));
                ui.set_max_width(left_w - 18.0);
                egui::ScrollArea::vertical()
                    .id_salt("app-groups")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        let total = group_count(&app.apps, Group::All).1.max(1);
                        for (k, g) in Group::LIST.into_iter().enumerate() {
                            if g == Group::Leftovers {
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    ui.add_space(10.0);
                                    kit::eyebrow(ui, &t, "Left behind");
                                });
                                ui.add_space(2.0);
                            }
                            let (n, size) = group_count(&app.apps, g);
                            let text = if size > 0 {
                                format!("{}  {n}  ·  {}", g.title(), human_size(size))
                            } else {
                                format!("{}  {n}", g.title())
                            };
                            let on = app.apps.group == g;
                            let count = if g.category().is_some() {
                                cleancraft_core::fsutil::items(n)
                            } else if n == 1 {
                                "1 app".to_string()
                            } else {
                                format!("{n} apps")
                            };
                            let size_text = if size > 0 {
                                human_size(size)
                            } else {
                                String::new()
                            };
                            let check = group_check(&app.apps, &app.settings, g);
                            // Empty groups stay listed (so the list does not jump) but step back.
                            let r = ui
                                .scope(|ui| {
                                    if n == 0 {
                                        ui.multiply_opacity(0.45);
                                    }
                                    super::plan_page::category_row(
                                        ui,
                                        &t,
                                        &text,
                                        g.short_title(),
                                        &size_text,
                                        &count,
                                        size as f32 / total as f32,
                                        kit::series_color(&t, k),
                                        on,
                                        check,
                                    )
                                })
                                .inner;
                            if let Some(v) = r.check_changed {
                                app.act();
                                select_group(app, g, v);
                            }
                            if r.row_clicked {
                                app.act();
                                app.apps.group = g;
                            }
                        }
                    });
            });
    });
    ui.scope_builder(egui::UiBuilder::new().max_rect(right), |ui| {
        ui.set_clip_rect(right.expand(24.0).intersect(ui.clip_rect()));
        kit::card(&t)
            .inner_margin(egui::Margin::symmetric(6, 8))
            .show(ui, |ui| {
                ui.set_min_size(egui::vec2(ui.available_width(), avail_h - 18.0));
                if file_group {
                    files_list(app, ui, avail_h - 18.0);
                } else {
                    apps_list(app, ui, avail_h - 18.0);
                }
            });
    });
}

/// Height of an app in the list.
const APP_ROW_H: f32 = 58.0;

fn apps_list(app: &mut CleanApp, ui: &mut Ui, h: f32) {
    let t = app.t;
    let rows = visible_apps(&app.apps, &app.settings);
    if rows.is_empty() {
        let searching = !app.apps.search.trim().is_empty();
        let filtering =
            kit_list::filtering(&app.apps.prefs, &app_filters(&app.apps, &app.settings));
        empty_state(
            ui,
            &t,
            Icon::Search,
            if filtering {
                "No app matches these filters"
            } else if searching {
                "No app matches your search"
            } else {
                "No apps here"
            },
            if filtering {
                "No app in this group passes the filters. Clear Filters shows every app again."
            } else if searching {
                "Search looks at app names, developers and bundle ids."
            } else if app.apps.group == Group::Unused {
                "Every app was opened in the last six months."
            } else {
                "Apps in this group show here once they are installed."
            },
        );
        return;
    }
    let now = now();
    let mut open = None;
    // (row index, how: plain toggle, or Shift range).
    let mut pick: Option<(usize, bool)> = None;
    let fade_out = egui::ScrollArea::vertical()
        .id_salt("apps-list")
        .max_height(h)
        .auto_shrink([false, false])
        .show_rows(ui, APP_ROW_H, rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (k, a) in rows.iter().enumerate().skip(range.start).take(range.len()) {
                let w = ui.available_width();
                // A stable id: the keyboard focus stays on the row when the selection bar
                // comes or goes.
                let (r, _) = ui.allocate_exact_size(vec2(w, APP_ROW_H), Sense::hover());
                let resp = ui.interact(r, egui::Id::new(("apps-row", &a.path)), Sense::click());
                resp.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, crate::tr(&a.name))
                });
                let picked = app.apps.picked.contains(&a.path);
                // The row opens the app; Cmd-click ticks it, Shift-click ticks a range, and
                // Space on the focused row ticks it (Return opens it).
                if resp.clicked() {
                    let (m, space) = ui.input(|i| (i.modifiers, i.key_pressed(Key::Space)));
                    if space && resp.has_focus() {
                        if pickable(a) {
                            pick = Some((k, false));
                        }
                    } else if m.shift && pickable(a) {
                        pick = Some((k, true));
                    } else if m.command && pickable(a) {
                        pick = Some((k, false));
                    } else {
                        open = Some(a.path.clone());
                    }
                }
                let feel = kit::feel_ring(ui, &t, &resp, r.shrink(2.0), 10.0);
                let hov = feel.hov;
                let p = ui.painter().clone();
                if picked {
                    p.rect_filled(
                        r.shrink(2.0),
                        10.0,
                        t.sec_a.gamma_multiply(0.07 + 0.04 * hov + 0.04 * feel.down),
                    );
                } else if hov > 0.0 || feel.down > 0.0 {
                    p.rect_filled(
                        r.shrink(2.0),
                        10.0,
                        Color32::from_white_alpha((10.0 * hov + 8.0 * feel.down) as u8),
                    );
                }
                // The checkbox; a padlock for an app that is part of macOS.
                let cb_rect =
                    Rect::from_center_size(pos2(r.left() + 20.0, r.center().y), vec2(22.0, 22.0));
                let mut on = picked;
                let cb = kit::check_or_lock(
                    ui,
                    &t,
                    cb_rect,
                    &mut on,
                    &format!("Select {}", a.name),
                    !pickable(a),
                );
                if cb.changed() {
                    let shift = ui.input(|i| i.modifiers.shift);
                    pick = Some((k, shift));
                }
                if !pickable(a) {
                    cb.on_disabled_hover_text(crate::tr(format!(
                        "{} is part of macOS and cannot be removed",
                        a.name
                    )));
                }
                let tile = Rect::from_center_size(
                    pos2(r.left() + 62.0, r.center().y),
                    egui::Vec2::splat(crate::theme::icon::APP_ROW + 4.0),
                );
                kit::app_icon(ui, tile, &a.name, Some(&a.path));
                let mut info = vec![];
                if !a.version.is_empty() {
                    info.push(a.version.clone());
                }
                info.push(a.vendor.clone());
                info.push(a.store.label().to_string());
                info.push(apps::opened_text(a, now));
                let info = info.join("  ·  ");
                let size_text = human_size(a.size);
                let size_font = font(size::BODY, Weight::SemiBold);
                let size_w = kit::galley(ui, &size_text, size_font.clone(), t.text)
                    .size()
                    .x;
                kit::text_at(
                    ui,
                    Rect::from_min_max(
                        pos2(r.right() - 30.0 - size_w, r.top()),
                        pos2(r.right() - 24.0, r.bottom()),
                    ),
                    &size_text,
                    &size_text,
                    size_font,
                    t.text,
                    Align::Max,
                );
                let chev =
                    Rect::from_center_size(pos2(r.right() - 12.0, r.center().y), vec2(12.0, 12.0));
                icons::paint(
                    &p,
                    chev,
                    Icon::Chevron,
                    icons::lerp(t.text_faint, t.text, hov),
                );
                let x0 = tile.right() + 12.0;
                let text_w = (r.right() - 40.0 - size_w - x0).max(60.0);
                let name_font = font(size::BODY, Weight::SemiBold);
                let name_s = kit::truncate_middle(ui.ctx(), &a.name, &name_font, text_w - 120.0);
                let name_w = kit::galley(ui, &name_s, name_font.clone(), t.text).size().x;
                // Painted only: the row button carries the name.
                p.text(
                    pos2(x0, r.top() + 20.0),
                    egui::Align2::LEFT_CENTER,
                    crate::tr(name_s),
                    name_font,
                    t.text,
                );
                if a.apple {
                    let tr = Rect::from_min_size(
                        pos2(x0 + name_w + 10.0, r.top() + 10.0),
                        vec2(120.0, 20.0),
                    );
                    kit::place(ui, tr, |ui| {
                        kit::tag(ui, "Part of macOS", t.warning);
                    });
                } else if let Some(main) = &a.part_of {
                    // A suite's helper: tied to the suite's app (it goes with it).
                    let tr = Rect::from_min_size(
                        pos2(x0 + name_w + 10.0, r.top() + 10.0),
                        vec2((text_w - name_w - 10.0).max(60.0), 20.0),
                    );
                    kit::place(ui, tr, |ui| {
                        kit::tag(ui, &format!("Part of {}", app_name(main)), t.text_dim);
                    });
                }
                let info_font = font(size::FOOTNOTE, Weight::Regular);
                let info_s = kit::truncate_middle(ui.ctx(), &info, &info_font, text_w);
                kit::text_at(
                    ui,
                    Rect::from_min_size(pos2(x0, r.top() + 31.0), vec2(text_w, 18.0)),
                    &info_s,
                    &info,
                    info_font,
                    t.text_faint,
                    Align::Min,
                );
            }
        });
    kit::scroll_fade(ui, &fade_out, t.glass_solid());
    if let Some((k, range)) = pick {
        app.act();
        let target = rows[k].path.clone();
        let anchor = app
            .apps
            .anchor
            .as_ref()
            .and_then(|a| rows.iter().position(|r| &r.path == a));
        match (range, anchor) {
            (true, Some(from)) => {
                // Shift: the range takes the anchor's state (ticked unless it was unticked).
                let on = app.apps.picked.contains(&rows[from].path) || from == k;
                let (lo, hi) = (from.min(k), from.max(k));
                let paths: Vec<PathBuf> = rows[lo..=hi]
                    .iter()
                    .filter(|a| pickable(a))
                    .map(|a| a.path.clone())
                    .collect();
                set_picked(app, &paths, on);
            }
            _ => {
                let on = !app.apps.picked.contains(&target);
                set_picked(app, std::slice::from_ref(&target), on);
                app.apps.anchor = Some(target);
            }
        }
    }
    if let Some(p) = open {
        app.act();
        open_app(app, &p);
    }
}

fn files_list(app: &mut CleanApp, ui: &mut Ui, h: f32) {
    let t = app.t;
    let ids = visible_files(&app.apps);
    // Notes of the scan (a list that could not be read) belong with the extensions.
    if app.apps.group == Group::Extensions
        && let Some(notes) = app.apps.extras.as_ref().map(|p| p.notes.clone())
    {
        for n in notes {
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                let (r, _) = ui.allocate_exact_size(egui::Vec2::splat(14.0), Sense::hover());
                icons::paint(ui.painter(), r, Icon::Warning, t.warning);
                kit::dim(ui, &t, &n);
            });
        }
    }
    if ids.is_empty() {
        let searching = !app.apps.search.trim().is_empty();
        empty_state(
            ui,
            &t,
            if searching { Icon::Search } else { Icon::Check },
            if searching {
                "No file matches your search"
            } else {
                "Nothing found"
            },
            if searching {
                "Try another word, or clear the search box."
            } else {
                match app.apps.group {
                    Group::Leftovers => "No files of removed apps were found.",
                    Group::Installers => "No app installers were found in your folders.",
                    _ => "No extensions were found.",
                }
            },
        );
    }
    let Some(plan) = app.apps.extras.clone() else {
        return;
    };
    let mut toggles = Vec::new();
    let fade_out = egui::ScrollArea::vertical()
        .id_salt("apps-files")
        .max_height(h - 20.0)
        .auto_shrink([false, false])
        .show_rows(ui, FILE_ROW_H, ids.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for &id in &ids[range] {
                if let Some(on) = file_row(app, ui, &plan.items[id], None, false) {
                    toggles.push((id, on));
                }
            }
        });
    kit::scroll_fade(ui, &fade_out, t.glass_solid());
    for (id, on) in toggles {
        app.act();
        if let Some(p) = app.apps.extras.as_mut() {
            p.set_selected(id, on);
        }
    }
}

/// A centred empty state: a badge, a line, and what to do about it.
/// The one empty state of the app (REVIEW-C C30), with the room above it these lists use.
fn empty_state(ui: &mut Ui, t: &crate::theme::Tokens, icon: Icon, title: &str, hint: &str) {
    ui.add_space(space::L);
    kit::empty_state(ui, t, icon, title, hint);
    ui.add_space(space::S);
}

/// Height of a file in the App Remover lists.
const FILE_ROW_H: f32 = 50.0;

/// "~/Library/Caches/com.example.App" -> ("~/Library/Caches", "com.example.App").
fn split_shown(shown: &str) -> (&str, &str) {
    match shown.trim_end_matches('/').rsplit_once('/') {
        Some(("", name)) => ("/", name),
        Some((folder, name)) => (folder, name),
        None => ("", shown),
    }
}

/// The kind part of a reason ("Cache: named after com.example.App" -> "Cache"); the group
/// heading above already says the rest.
fn reason_kind(reason: &str) -> &str {
    reason
        .split_once(": ")
        .map(|(k, _)| k)
        .unwrap_or(reason)
        .trim()
}

/// One file with its checkbox: the name prominent, its folder under it (dimmed, shortened in
/// the middle), why it belongs to the app, and its size. A click anywhere on the row toggles
/// it. `shared_lock` is the lock every row of the plan carries (a running app): the banner
/// says it once, so the rows show only a padlock. Returns the new state when toggled.
fn file_row(
    app: &CleanApp,
    ui: &mut Ui,
    it: &cleancraft_core::Item,
    shared_lock: Option<&str>,
    short_reason: bool,
) -> Option<bool> {
    let t = app.t;
    let is_app = it.category == uninstaller::CAT_APP;
    let locked = it.locked.is_some();
    // The app itself is always part of an uninstall; Reset is the way to keep it.
    let toggle = !locked && !is_app;
    let w = ui.available_width();
    let sense = if toggle {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (r, row) = ui.allocate_exact_size(vec2(w, FILE_ROW_H), sense);
    let shown = app.env.display(&it.path);
    let card = r.shrink2(vec2(2.0, 2.0));
    // Pointing hand, eased hover and press, keyboard focus ring (kit).
    let feel = kit::feel_ring(ui, &t, &row, card, 10.0);
    let hov = if toggle { feel.hov } else { 0.0 };
    let p = ui.painter().clone();
    if it.selected && !is_app {
        p.rect_filled(
            card,
            10.0,
            t.sec_a.gamma_multiply(0.06 + 0.04 * hov + 0.04 * feel.down),
        );
    } else if hov > 0.0 {
        p.rect_filled(
            card,
            10.0,
            Color32::from_white_alpha((9.0 * hov + 6.0 * feel.down) as u8),
        );
    }
    // Checkbox, named after the whole path (VoiceOver, tests); a padlock when locked.
    let mut on = it.selected;
    let cb_rect = Rect::from_center_size(pos2(r.left() + 20.0, r.center().y), vec2(22.0, 22.0));
    let cb = kit::place(ui, cb_rect, |ui| {
        if locked {
            ui.multiply_opacity(0.0);
        }
        ui.add_enabled_ui(toggle, |ui| kit::check(ui, &t, &mut on, &shown))
            .inner
    });
    let cb = if is_app && !locked {
        cb.on_disabled_hover_text(crate::tr(
            "The app is always part of an uninstall. Reset keeps it.",
        ))
    } else {
        cb
    };
    if locked {
        icons::paint(
            &p,
            Rect::from_center_size(cb_rect.center(), egui::Vec2::splat(16.0)),
            Icon::Lock,
            t.text_faint,
        );
    }
    let mut changed = cb.changed().then_some(on);
    if changed.is_none() && toggle && row.clicked() {
        changed = Some(!it.selected);
    }
    // Icon: the app's own for the bundle, the file's kind otherwise.
    let ir = Rect::from_center_size(pos2(r.left() + 54.0, r.center().y), egui::Vec2::splat(26.0));
    let (folder, name) = split_shown(&shown);
    // Your home folder reads as "~", like Finder's path bar.
    let home = app.env.display(app.env.home());
    let folder = match folder.strip_prefix(home.as_str()) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => folder.to_string(),
    };
    let folder = folder.as_str();
    if is_app {
        kit::app_icon(ui, ir, name.trim_end_matches(".app"), Some(&it.path));
    } else {
        crate::images::paint_file_icon(
            ui,
            ir.shrink(2.0),
            &it.path,
            crate::file_icons::FileKind::of(&it.path),
        );
    }
    let ink = if locked { t.text_dim } else { t.text };
    // Size, right aligned in a fixed column. Part of it unreadable: never a bare "0 KB".
    let partial = uninstaller::size_is_partial(it);
    let size_text = match (partial, it.size) {
        (true, 0) => "Unknown".to_string(),
        (true, n) => format!("at least {}", human_size(n)),
        (false, n) => human_size(n),
    };
    let size_font = font(size::BODY, Weight::SemiBold);
    let size_w = kit::galley(ui, &size_text, size_font.clone(), t.text)
        .size()
        .x
        .max(64.0);
    let size_r = Rect::from_min_max(
        pos2(r.right() - 12.0 - size_w, r.top()),
        pos2(r.right() - 10.0, r.bottom()),
    );
    let size_resp = kit::text_at(
        ui,
        size_r,
        &size_text,
        &size_text,
        size_font,
        if partial { t.text_dim } else { ink },
        Align::Max,
    );
    if partial {
        kit::row_tip(
            &size_resp,
            r,
            "Some of it could not be read (macOS keeps it private, or another user owns it), so it may be bigger than shown.",
        );
    }
    // Why it is listed. A lock that is not the plan's shared one is the most useful thing
    // to say, so it takes the place of the reason.
    let own_lock = it
        .locked
        .as_deref()
        .filter(|l| shared_lock != Some(*l) && !l.is_empty());
    let why_full = match own_lock {
        Some(l) => format!("{} ({l})", it.reason),
        None => super::plan_page::reason_text(it),
    };
    let why_shown = match own_lock {
        Some(l) => l.to_string(),
        // The app's name and version are the page's heading already.
        None if is_app => String::new(),
        None if short_reason && !is_app => {
            let mut s =
                reason_kind(it.reason.trim_end_matches(uninstaller::SIZE_PARTIAL)).to_string();
            if it.needs_admin {
                s.push_str(", needs password");
            }
            s
        }
        None => why_full.clone(),
    };
    let x0 = r.left() + 80.0;
    let right_end = size_r.left() - 16.0;
    let avail = (right_end - x0).max(80.0);
    let wf = font(size::FOOTNOTE, Weight::Medium);
    let why_cap = if short_reason {
        (avail * 0.34).min(240.0)
    } else {
        (avail * 0.45).min(360.0)
    };
    let why_w = kit::galley(ui, &why_shown, wf.clone(), t.text_dim)
        .size()
        .x
        .min(why_cap);
    let why_s = kit::truncate_end(ui.ctx(), &why_shown, &wf, why_w + 1.0);
    kit::text_at(
        ui,
        Rect::from_min_max(
            pos2(right_end - why_w - 2.0, r.top()),
            pos2(right_end, r.bottom()),
        ),
        &why_s,
        &why_full,
        wf,
        if own_lock.is_some() {
            t.warning
        } else {
            t.text_dim
        },
        Align::Max,
    );
    // Name (prominent) and folder (dim, shortened in the middle). Painted only: the checkbox
    // already carries the whole path.
    let text_w = (avail - why_w - 20.0).max(60.0);
    let nf = font(size::BODY, Weight::Medium);
    let name_s = kit::truncate_middle(ui.ctx(), name, &nf, text_w);
    p.text(
        pos2(x0, r.top() + 17.0),
        egui::Align2::LEFT_CENTER,
        crate::tr(&name_s),
        nf,
        ink,
    );
    let ff = font(size::FOOTNOTE, Weight::Regular);
    let folder_s = kit::truncate_middle(ui.ctx(), folder, &ff, text_w);
    p.text(
        pos2(x0, r.top() + 35.0),
        egui::Align2::LEFT_CENTER,
        crate::tr(&folder_s),
        ff,
        t.text_faint,
    );
    // The whole path when either line had to be shortened.
    if name_s != name || folder_s != folder {
        // Under the row (or above it at the bottom), never over its own text.
        kit::row_tip(&row, r, &shown);
    }
    changed
}

fn files_bottom_bar(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let (n, size) = app
        .apps
        .extras
        .as_ref()
        .map(|p| (p.selected_count(), p.selected_size()))
        .unwrap_or((0, 0));
    // Selected files the search, filters or group do not show still move: say so.
    // (The group column only navigates: a ticked file of another group is not hidden.)
    let hidden = {
        let st = &app.apps;
        let q = kit_list::query(&st.search);
        let defs = [
            Filter::any("Show", "selected", "", 0),
            Filter::any("Show", "unselected", "", 0),
            Filter::any("Show", "locked", "", 0),
        ];
        st.extras
            .as_ref()
            .map(|p| {
                p.items
                    .iter()
                    .filter(|i| i.selected)
                    .filter(|i| {
                        !kit_list::matches(&q, &[&i.path.to_string_lossy(), &i.reason])
                            || !kit_list::passes(&st.files_prefs, &defs, |k| file_has(i, k))
                    })
                    .count()
            })
            .unwrap_or(0)
    };
    let note = kit_list::hidden_note(hidden);
    kit::bottom_bar(ui, &t, egui::Id::new("apps-files-bottom"), |ui| {
        kit::bar_figure(
            ui,
            &t,
            &human_size(size),
            &format!("{} selected{note}", cleancraft_core::fsutil::items(n)),
            &format!("{n} selected{note}, {}", human_size(size)),
            false,
        );
        ui.add_space(space::M);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if n == 0 {
                kit::bar_hint(ui, &t, "Tick files to remove them.");
            }
            if n > 0 && kit::danger(ui, &t, "Remove Selected") {
                app.act();
                app.apps.pending = app.apps.extras.clone();
                app.apps.phase = Phase::Confirm(Action::RemoveFiles);
            }
            if n > 0 && kit::secondary(ui, &t, "Clear Selection") {
                app.act();
                if let Some(p) = app.apps.extras.as_mut() {
                    for i in 0..p.items.len() {
                        p.set_selected(i, false);
                    }
                }
            }
        });
    });
}

/// Why the open app cannot be uninstalled right now.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Block {
    /// The app is running: Quit it here.
    Running,
    /// Part of macOS.
    System(String),
    /// On the ignore list.
    Ignored(String),
}

fn block_of(app: &CleanApp, plan: &Plan, path: &Path) -> Option<Block> {
    if plan.notes.is_empty() {
        return None;
    }
    // An app already in the Trash (the Trash offer): only running blocks its leftovers.
    if cleancraft_core::watch::in_trash(&app.env, path) {
        return app.apps.running.then_some(Block::Running);
    }
    if app.settings.is_ignored(path) {
        return Some(Block::Ignored(plan.notes[0].clone()));
    }
    if app.apps.running {
        return Some(Block::Running);
    }
    Some(Block::System(plan.notes[0].clone()))
}

fn detail(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let Some(path) = app.apps.selected.clone() else {
        return;
    };
    let info = app
        .apps
        .list
        .iter()
        .flatten()
        .find(|a| a.path == path)
        .cloned();
    let esc = matches!(app.apps.phase, Phase::Browse)
        && !app.apps.confirm_force
        && !app.apps.confirm_admin
        && !app.apps.vendor_ask
        && ui.input(|i| i.key_pressed(Key::Escape));
    let back = kit::pill(ui, &t, "Back to Apps", kit::Kind::Ghost, Some(Icon::Back)).clicked();
    if esc || back {
        app.act();
        close_detail(app);
        return;
    }
    let Some(plan) = app.apps.detail.clone() else {
        ui.label(crate::rich("This app could not be read.").color(t.danger));
        return;
    };
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    ui.add_space(space::S);
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(
            egui::Vec2::splat(crate::theme::icon::APP_LARGE),
            Sense::hover(),
        );
        kit::app_icon(ui, r, &name, Some(&path));
        ui.add_space(space::S);
        let total = human_size(plan.total_size());
        let total_font = font(size::NUMBER, Weight::Bold);
        let total_w = kit::galley(ui, &total, total_font.clone(), t.text).size().x + 8.0;
        ui.vertical(|ui| {
            ui.set_max_width((ui.available_width() - total_w - 16.0).max(120.0));
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.add(
                egui::Label::new(
                    crate::rich(&name)
                        .font(font(size::TITLE, Weight::Bold))
                        .color(t.text),
                )
                .truncate(),
            );
            if let Some(a) = &info {
                let mut parts = vec![if a.version.is_empty() {
                    "Version unknown".to_string()
                } else {
                    format!("Version {}", a.version)
                }];
                parts.push(a.vendor.clone());
                parts.push(a.store.label().to_string());
                parts.push(apps::opened_text(a, now()));
                let line = parts.join("  ·  ");
                ui.add(
                    egui::Label::new(
                        crate::rich(&line)
                            .font(font(size::CALLOUT, Weight::Regular))
                            .color(t.text_dim),
                    )
                    .truncate(),
                )
                .on_hover_text(crate::tr(format!("{line}\n{}", a.bundle_id)));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(crate::rich(total).font(total_font).color(icons::lerp(
                t.sec_a,
                Color32::WHITE,
                0.15,
            )));
        });
    });
    ui.add_space(space::S);
    let block = block_of(app, &plan, &path);
    if let Some(b) = &block {
        block_banner(app, ui, b, &name, &path);
        ui.add_space(space::S);
    }
    let trashed = cleancraft_core::watch::in_trash(&app.env, &path);
    // The maker's own uninstaller is the recommended way (it also removes licensing services
    // and background helpers it installed).
    let vendor = info
        .as_ref()
        .filter(|_| !trashed && !matches!(block, Some(Block::System(_))))
        .and_then(|a| {
            Some((
                a.vendor_uninstaller.clone()?,
                apps::vendor_uninstaller_label(a)?,
            ))
        });
    if let Some((u, label)) = &vendor {
        let text = format!(
            "{name} comes with its own uninstaller, {}. It also removes the licensing services \
             and background helpers {name} installed, so it is the recommended way to remove it.",
            app_name(u)
        );
        if kit::banner(ui, &t, Icon::Remover, t.sec_a, &text, label) {
            app.act();
            open_vendor_uninstaller(app, u);
        }
        if let Some(n) = app.apps.vendor_note.clone() {
            ui.add_space(space::XS);
            kit::strip(ui, Icon::Check, t.text_dim, &n);
        }
        ui.add_space(space::S);
    }
    // The lock every row carries (the banner says it once).
    let shared_lock = plan
        .items
        .first()
        .and_then(|i| i.locked.clone())
        .filter(|l| {
            plan.items
                .iter()
                .all(|i| i.locked.as_deref() == Some(l.as_str()))
        });
    if trashed && let Some(note) = plan.notes.first() {
        kit::strip(ui, Icon::Trash, t.text_dim, note);
        ui.add_space(space::S);
    }
    let files_sel = plan
        .selected()
        .filter(|i| i.category != uninstaller::CAT_APP)
        .count();
    if trashed {
        trashed_bar(app, ui, &plan, block.is_some(), &name);
    } else {
        kit::bottom_bar(ui, &t, egui::Id::new("apps-detail-bottom"), |ui| {
            // The same bar as every page with a selection (REVIEW-C C19): the size, the count,
            // the hint in the room left, the buttons.
            let n = plan.selected_count();
            let size = plan.selected_size();
            kit::bar_figure(
                ui,
                &t,
                &human_size(size),
                &format!("{} selected", cleancraft_core::fsutil::items(n)),
                &format!("{n} selected, {}", human_size(size)),
                block.is_some(),
            );
            ui.add_space(space::M);
            let hint = match &block {
                Some(Block::Running) => Some(format!("Quit {name} to choose what to remove.")),
                Some(Block::System(_)) => Some("macOS keeps this app.".to_string()),
                Some(Block::Ignored(_)) => Some("On your ignore list.".to_string()),
                None if files_sel == 0 => Some("Select files to reset the app.".to_string()),
                None => None,
            };
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // A blocked action is a quiet, disabled button (a pale red one reads as broken).
                let can_uninstall = block.is_none()
                    && plan
                        .items
                        .iter()
                        .any(|i| i.category == uninstaller::CAT_APP && i.locked.is_none());
                let kind = if can_uninstall {
                    kit::Kind::Danger
                } else {
                    kit::Kind::Secondary
                };
                let un = ui
                    .add_enabled_ui(can_uninstall, |ui| {
                        kit::pill(ui, &t, "Uninstall", kind, None)
                    })
                    .inner;
                let why_not = match &block {
                    Some(Block::Running) => format!("Quit {name} first."),
                    Some(Block::System(s) | Block::Ignored(s)) => s.clone(),
                    None => String::new(),
                };
                let un = un.on_disabled_hover_text(crate::tr(&why_not));
                if un.clicked() && can_uninstall {
                    app.act();
                    if vendor.is_some() {
                        app.apps.vendor_ask = true;
                    } else {
                        app.apps.pending = app.apps.detail.clone();
                        app.apps.phase = Phase::Confirm(Action::Uninstall);
                    }
                }
                let can_reset = block.is_none() && files_sel > 0;
                let rs = ui.add_enabled_ui(can_reset, |ui| {
                    kit::pill(ui, &t, "Reset App...", kit::Kind::Secondary, None)
                });
                let rs = rs
                    .inner
                    .on_hover_text(crate::tr(
                        "Remove the app's settings and data, keep the app",
                    ))
                    .on_disabled_hover_text(crate::tr(if block.is_some() {
                        why_not.as_str()
                    } else {
                        "Select the files to remove first. The app stays."
                    }));
                if rs.clicked() && can_reset {
                    app.act();
                    match uninstaller::uninstall_plan(&app.env, &app.settings, &path, true) {
                        Ok(mut p) => {
                            copy_selection(&plan, &mut p);
                            app.apps.pending = Some(p);
                            app.apps.phase = Phase::Confirm(Action::Reset);
                        }
                        Err(e) => app.apps.error = Some(format!("{e:#}")),
                    }
                }
                if kit::pill(ui, &t, "Scan Again", kit::Kind::Secondary, None)
                    .on_hover_text(crate::tr("Look at the app's files again"))
                    .clicked()
                {
                    app.act();
                    app.apps.vendor_note = None;
                    if !path.exists() {
                        // Removed meanwhile (its maker's uninstaller): the list is read again.
                        close_detail(app);
                        start_load(app);
                        return;
                    }
                    rebuild_detail(app, Some(&plan));
                }
                if !app.settings.is_ignored(&path) && !matches!(block, Some(Block::System(_))) {
                    let ig = kit::secondary_resp(ui, &t, "Hide from List");
                    if ig
                        .on_hover_text(crate::tr(
                            "Never offer this app for removal (Settings > Ignore list)",
                        ))
                        .clicked()
                    {
                        app.act();
                        app.settings.ignore.push(path.clone());
                        app.save_settings();
                        close_detail(app);
                    }
                }
                if let Some(h) = &hint {
                    kit::bar_hint(ui, &t, h);
                }
            });
        });
    }
    ui.add_space(space::XS);
    let h = ui.available_height() - 4.0;
    let mut toggles = Vec::new();
    kit::card(&t)
        .inner_margin(egui::Margin::symmetric(8, 8))
        .show(ui, |ui| {
            ui.set_min_size(egui::vec2(ui.available_width(), h - 18.0));
            let fade_out = egui::ScrollArea::vertical()
                .id_salt("apps-detail")
                .max_height(h - 18.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (k, c) in plan.categories.iter().enumerate() {
                        let items: Vec<_> =
                            plan.items.iter().filter(|i| i.category == c.id).collect();
                        if items.is_empty() {
                            continue;
                        }
                        ui.add_space(if k == 0 { 4.0 } else { 12.0 });
                        category_head(app, ui, &plan, c, k, items.len());
                        for it in items {
                            if let Some(on) = file_row(app, ui, it, shared_lock.as_deref(), true) {
                                toggles.push((it.id, on));
                            }
                        }
                    }
                    ui.add_space(4.0);
                });
            kit::scroll_fade(ui, &fade_out, t.glass_solid());
        });
    for (id, on) in toggles {
        app.act();
        if let Some(p) = app.apps.detail.as_mut() {
            p.set_selected(id, on);
        }
    }
    if app.apps.confirm_force {
        confirm_force(app, ui, &name);
    }
    if app.apps.confirm_admin {
        confirm_admin(app, ui, &name);
    }
    if app.apps.vendor_ask
        && let Some((u, label)) = vendor
    {
        vendor_dialog(app, ui, &name, &path, &u, &label);
    }
}

/// Uninstall was asked for an app whose maker ships its own uninstaller: recommend it, and
/// still offer CleanCraft's own uninstall.
fn vendor_dialog(app: &mut CleanApp, ui: &mut Ui, name: &str, path: &Path, u: &Path, label: &str) {
    let t = app.t;
    let uname = app_name(u);
    let (mut open, mut ours, mut close) = (false, false, false);
    let resp = kit::modal(egui::Id::new("apps-vendor-uninstaller"), &t).show(ui.ctx(), |ui| {
        ui.set_width(520.0);
        crate::images::dialog_head_app(
            ui,
            &t,
            path,
            &format!("Use {uname} instead?"),
            &format!(
                "{name} comes with its own uninstaller. It also removes the licensing services, \
                 background helpers and system services {name} installed. CleanCraft moves the \
                 app and the files it finds in your Library, so those parts can stay behind."
            ),
        );
        ui.add_space(space::L);
        ui.horizontal(|ui| {
            ours = kit::secondary(ui, &t, "Uninstall with CleanCraft");
        });
        ui.add_space(space::S);
        let (g, c) = kit::dialog_actions(ui, &t, label, kit::Kind::Primary);
        open = g;
        close = c;
    });
    if resp.should_close() {
        close = true;
    }
    if open {
        app.act();
        app.apps.vendor_ask = false;
        open_vendor_uninstaller(app, u);
    } else if ours {
        app.act();
        app.apps.vendor_ask = false;
        app.apps.pending = app.apps.detail.clone();
        app.apps.phase = Phase::Confirm(Action::Uninstall);
    } else if close {
        app.act();
        app.apps.vendor_ask = false;
    }
}

/// The bar of an app already in the Trash: its leftovers only (Remove Leftovers).
fn trashed_bar(app: &mut CleanApp, ui: &mut Ui, plan: &Plan, blocked: bool, name: &str) {
    let t = app.t;
    let n = plan.selected_count();
    let size = plan.selected_size();
    kit::bottom_bar(ui, &t, egui::Id::new("apps-trashed-bottom"), |ui| {
        kit::bar_figure(
            ui,
            &t,
            &human_size(size),
            &format!("{} selected", cleancraft_core::fsutil::items(n)),
            &format!("{n} selected, {}", human_size(size)),
            blocked,
        );
        ui.add_space(space::M);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let can = !blocked && n > 0;
            let kind = if can {
                kit::Kind::Danger
            } else {
                kit::Kind::Secondary
            };
            let r = ui
                .add_enabled_ui(can, |ui| kit::pill(ui, &t, "Remove Leftovers", kind, None))
                .inner
                .on_disabled_hover_text(crate::tr(if blocked {
                    format!("Quit {name} first.")
                } else {
                    "Select the files to remove first.".to_string()
                }));
            if r.clicked() && can {
                app.act();
                app.apps.pending = app.apps.detail.clone();
                app.apps.phase = Phase::Confirm(Action::RemoveFiles);
            }
            if blocked {
                kit::bar_hint(ui, &t, &format!("Quit {name} to remove its files."));
            } else if n == 0 {
                kit::bar_hint(ui, &t, "Tick the files to remove.");
            }
        });
    });
}

/// A group of the app's files: a dot in the group's colour, its title, size and count, and
/// what the group means (no tooltip: it would cover the first row).
fn category_head(
    app: &CleanApp,
    ui: &mut Ui,
    plan: &Plan,
    c: &cleancraft_core::plan::Category,
    k: usize,
    n: usize,
) {
    let t = app.t;
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
        ui.painter()
            .circle_filled(r.center(), 4.0, kit::series_color(&t, k));
        ui.add_space(4.0);
        ui.label(
            crate::rich(&c.title)
                .font(font(size::BODY, Weight::SemiBold))
                .color(t.text),
        );
        let count = if c.id == uninstaller::CAT_APP {
            String::new()
        } else {
            format!("  ·  {}", cleancraft_core::fsutil::items(n))
        };
        ui.label(
            crate::rich(format!("{}{count}", human_size(plan.category_size(&c.id))))
                .font(font(size::CALLOUT, Weight::Medium))
                .color(t.text_dim),
        );
    });
    if c.id != uninstaller::CAT_APP && !c.description.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(32.0);
            ui.add(
                egui::Label::new(
                    crate::rich(&c.description)
                        .font(font(size::FOOTNOTE, Weight::Regular))
                        .color(t.text_faint),
                )
                .truncate(),
            );
        });
    }
    ui.add_space(2.0);
}

/// The reason the app cannot be removed now, with what to do about it.
fn block_banner(app: &mut CleanApp, ui: &mut Ui, b: &Block, name: &str, path: &Path) {
    let t = app.t;
    let parts = app.apps.parts.clone();
    let (color, icon) = match b {
        Block::Running => (t.warning, Icon::Warning),
        Block::System(_) | Block::Ignored(_) => (t.text_dim, Icon::Lock),
    };
    egui::Frame::new()
        .fill(color.gamma_multiply(0.10))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.35)))
        .corner_radius(egui::CornerRadius::same(crate::theme::radius::M))
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::Vec2::splat(22.0), Sense::hover());
                icons::paint(ui.painter(), r, icon, color);
                ui.add_space(space::XS);
                // Actions first (from the right), so long names are cut instead of pushing
                // the buttons out.
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let text = match b {
                        Block::Running => {
                            quit_actions(app, ui, name, path);
                            running_text(app, name)
                        }
                        Block::Ignored(why) => {
                            if kit::pill(
                                ui,
                                &t,
                                "Remove from Ignore List",
                                kit::Kind::Secondary,
                                None,
                            )
                            .clicked()
                            {
                                app.act();
                                app.settings.ignore.retain(|p| p != path);
                                app.save_settings();
                                rebuild_detail(app, None);
                            }
                            (
                                why.clone(),
                                "Remove it from the list to uninstall it.".into(),
                            )
                        }
                        Block::System(why) => (
                            why.clone(),
                            "macOS needs it. Updates of macOS keep it current.".into(),
                        ),
                    };
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.add(
                                egui::Label::new(
                                    crate::rich(&text.0)
                                        .font(font(size::BODY, Weight::SemiBold))
                                        .color(icons::lerp(color, Color32::WHITE, 0.55)),
                                )
                                .truncate(),
                            );
                            ui.add(
                                egui::Label::new(
                                    crate::rich(&text.1)
                                        .font(font(size::CALLOUT, Weight::Regular))
                                        .color(t.text_dim),
                                )
                                .wrap(),
                            );
                            // What runs, when it is more than the app itself (helpers,
                            // agents, services): "name · role".
                            if let (Block::Running, Some(st)) = (b, &parts) {
                                // The headline names the app; list what else runs.
                                let lines: Vec<String> = st
                                    .running()
                                    .filter(|p| p.kind != app_quit::PartKind::App)
                                    .map(|p| format!("{}  ·  {}", p.name, p.role()))
                                    .collect();
                                if !lines.is_empty() {
                                    ui.add_space(2.0);
                                    for l in lines.iter().take(4) {
                                        ui.add(
                                            egui::Label::new(
                                                crate::rich(l)
                                                    .font(font(size::FOOTNOTE, Weight::Medium))
                                                    .color(t.text_dim),
                                            )
                                            .truncate(),
                                        );
                                    }
                                    if lines.len() > 4 {
                                        kit::dim(ui, &t, &format!("and {} more", lines.len() - 4));
                                    }
                                }
                            }
                        });
                    });
                });
            });
        });
}

/// The banner's two lines while the app runs, by how quitting goes.
fn running_text(app: &CleanApp, name: &str) -> (String, String) {
    match &app.apps.quit {
        Quit::Idle => (
            app.apps
                .parts
                .as_ref()
                .map(|s| s.headline())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| format!("{name} is open")),
            "Quit it to uninstall or reset it. The files unlock as soon as it closes.".to_string(),
        ),
        Quit::Asking(job) => (
            format!("Waiting for {name} to quit..."),
            match job.elapsed_secs() {
                0..=2 => format!("{name} may ask you to save your work first."),
                _ => {
                    format!("Still open. If {name} shows a dialog, answer it, or force it to quit.")
                }
            },
        ),
        Quit::StillOpen(msg) => (format!("{name} did not quit"), msg.clone()),
        Quit::Failed(e) => (format!("{name} could not be quit"), e.clone()),
    }
}

/// Quit, Try Again and Force Quit, by how quitting goes (laid out right to left).
fn quit_actions(app: &mut CleanApp, ui: &mut Ui, name: &str, path: &Path) {
    let t = app.t;
    let asking = matches!(app.apps.quit, Quit::Asking(_));
    let late = matches!(&app.apps.quit, Quit::Asking(j) if j.elapsed_secs() >= 3);
    let offer_force = late || matches!(app.apps.quit, Quit::StillOpen(_) | Quit::Failed(_));
    if asking {
        let (r, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
        let time = ui.input(|i| i.time);
        kit::progress_ring(ui.painter(), r.center(), 18.0, &t, time);
        ui.ctx().request_repaint();
    } else {
        let label = if matches!(app.apps.quit, Quit::Idle) {
            format!("Quit {name}")
        } else {
            "Try Again".to_string()
        };
        if kit::pill(ui, &t, &label, kit::Kind::Primary, None).clicked() {
            app.act();
            if app.apps.parts.as_ref().is_some_and(|s| s.needs_admin()) {
                // A system service of the app: say so before the password dialog.
                app.apps.confirm_admin = true;
            } else {
                start_quit(app, path, false, false);
            }
        }
    }
    if offer_force {
        ui.add_space(space::XS);
        let f = kit::pill(ui, &t, "Force Quit", kit::Kind::Secondary, None);
        if f.on_hover_text(crate::tr(format!("Stop {name} at once")))
            .clicked()
        {
            app.act();
            app.apps.confirm_force = true;
        }
    }
}

/// A system service of the app runs as root: stopping it asks for the password. The person
/// chooses: quit everything (password), or quit what does not need it.
fn confirm_admin(app: &mut CleanApp, ui: &mut Ui, name: &str) {
    let t = app.t;
    let Some(path) = app.apps.selected.clone() else {
        app.apps.confirm_admin = false;
        return;
    };
    let question = app
        .apps
        .parts
        .as_ref()
        .and_then(|s| s.admin_question())
        .unwrap_or_default();
    let (mut all, mut rest, mut close) = (false, false, false);
    let resp = kit::modal(egui::Id::new("apps-quit-admin"), &t).show(ui.ctx(), |ui| {
        ui.set_width(520.0);
        crate::images::dialog_head_app(ui, &t, &path, &format!("Quit {name}?"), &question);
        ui.add_space(space::L);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if kit::primary(ui, &t, "Quit and Enter Password") {
                all = true;
            }
            if kit::secondary(ui, &t, "Quit the Rest") {
                rest = true;
            }
            if kit::secondary(ui, &t, "Cancel") {
                close = true;
            }
        });
    });
    if resp.should_close() {
        close = true;
    }
    if all || rest || close {
        app.act();
        app.apps.confirm_admin = false;
    }
    if all {
        start_quit(app, &path, false, true);
    } else if rest {
        start_quit(app, &path, false, false);
    }
}

fn confirm_force(app: &mut CleanApp, ui: &mut Ui, name: &str) {
    let t = app.t;
    let Some(path) = app.apps.selected.clone() else {
        app.apps.confirm_force = false;
        return;
    };
    let mut go = false;
    let mut close = false;
    let resp = kit::modal(egui::Id::new("apps-force-quit"), &t).show(ui.ctx(), |ui| {
        ui.set_width(480.0);
        crate::images::dialog_head_app(
            ui,
            &t,
            &path,
            &format!("Force quit {name}?"),
            "It stops at once, without asking to save. Work you have not saved in it is lost.",
        );
        ui.add_space(space::L);
        let (g, c) = kit::dialog_actions(ui, &t, "Force Quit", kit::Kind::Danger);
        go = g;
        close = c;
    });
    if resp.should_close() {
        close = true;
    }
    if go {
        app.act();
        app.apps.confirm_force = false;
        let admin = app.apps.parts.as_ref().is_some_and(|s| s.needs_admin());
        start_quit(app, &path, true, admin);
    } else if close {
        app.act();
        app.apps.confirm_force = false;
    }
}

fn confirm(app: &mut CleanApp, ui: &mut Ui, action: Action) {
    let t = app.t;
    let Some(plan) = app.apps.pending.clone() else {
        app.apps.phase = Phase::Browse;
        return;
    };
    let dest = match app.settings.method {
        Method::Trash => "the Trash",
        Method::Quarantine | Method::AdminQuarantine => "the CleanCraft quarantine",
    };
    let selected = app.apps.selected.clone();
    let name = selected
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let n = cleancraft_core::fsutil::items(plan.selected_count());
    let size = human_size(plan.selected_size());
    let verb = if plan.selected_count() == 1 {
        "moves"
    } else {
        "move"
    };
    let headline = match action {
        Action::Uninstall => format!("Uninstall {name}? {n} ({size}) {verb} to {dest}."),
        Action::Reset => format!("Reset {name}? {n} ({size}) {verb} to {dest}. The app stays."),
        Action::RemoveFiles | Action::UninstallApps => format!("Move {n} ({size}) to {dest}?"),
    };
    let mut go = false;
    let mut close = false;
    let width = (ui.ctx().content_rect().width() - 120.0).clamp(420.0, 660.0);
    let max_list = (ui.ctx().content_rect().height() - 330.0).clamp(120.0, 360.0);
    let resp = kit::modal(egui::Id::new("apps-confirm"), &t).show(ui.ctx(), |ui| {
        ui.set_width(width);
        let sub = "These exact files will be moved. You can put them back from History.";
        match (&selected, action) {
            (Some(p), Action::Uninstall | Action::Reset) => {
                crate::images::dialog_head_app(ui, &t, p, &headline, sub)
            }
            _ => kit::dialog_head(
                ui,
                &t,
                if action == Action::RemoveFiles {
                    Icon::Trash
                } else {
                    Icon::Remover
                },
                t.sec_a,
                &headline,
                sub,
            ),
        }
        ui.add_space(space::M);
        if let Some(note) =
            super::plan_page::admin_note(cleancraft_core::features::admin_count(&plan))
        {
            kit::strip(ui, Icon::Warning, t.warning, &note);
            ui.add_space(space::S);
        }
        let rows: Vec<&cleancraft_core::Item> = plan.selected().collect();
        kit::well(&t).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt("apps-confirm-list")
                .max_height(max_list)
                .show_rows(ui, 26.0, rows.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for it in &rows[range] {
                        kit::confirm_line(
                            ui,
                            &t,
                            &it.path,
                            &app.env.display(&it.path),
                            &it.reason,
                            &human_size(it.size),
                        );
                    }
                });
        });
        ui.add_space(space::L);
        let label = match app.settings.method {
            Method::Trash => "Move to Trash",
            Method::Quarantine | Method::AdminQuarantine => "Move to Quarantine",
        };
        let (g, c) = kit::dialog_actions(ui, &t, label, kit::Kind::Danger);
        go = g;
        close = c;
    });
    if resp.should_close() {
        close = true;
    }
    if go {
        app.act();
        start_action(app, action);
        poll(app);
    } else if close {
        app.act();
        app.apps.pending = None;
        app.apps.phase = Phase::Browse;
    }
}

fn done(app: &mut CleanApp, ui: &mut Ui) {
    let Phase::Done(rep, action) = &app.apps.phase else {
        return;
    };
    let (rep, action) = (rep.clone(), *action);
    // The app itself may have stayed (macOS protects it, the password was not given): then
    // the screen says so with what to do, never "Uninstalled" (fix7, App Store apps).
    let kept = rep
        .skipped
        .iter()
        .find(|(p, _)| is_app_bundle(p) && action == Action::Uninstall)
        .map(|(p, why)| (p.clone(), why.to_string()));
    let what = match (action, &kept) {
        // The reason is in the "Not moved" card below; here what it means and what to do.
        (Action::Uninstall, Some((p, why))) => {
            format!("{} was not removed. {} ", app_name(p), app_kept_help(why))
        }
        (Action::Uninstall, None) => "Uninstalled. ".into(),
        (Action::Reset, _) => "Reset done. ".into(),
        (Action::RemoveFiles | Action::UninstallApps, _) => String::new(),
    };
    let summary = format!(
        "{what}{}",
        kit::moved_summary(rep.moved.len(), rep.skipped.len())
    );
    super::plan_page::done_view(
        app,
        ui,
        FeatureId::Uninstaller,
        &rep,
        &summary,
        |app, ui| {
            let t = app.t;
            if let Some((p, why)) = &kept {
                kept_app_buttons(app, ui, p, why);
            }
            if !rep.moved.is_empty()
                && kit::pill(ui, &t, "Undo", kit::Kind::Secondary, Some(Icon::Undo)).clicked()
            {
                app.act();
                let (n, errs) = executor::undo_plan(&app.env, &rep.plan_id);
                app.files_back(&CleanApp::moved_paths(&rep));
                if errs.is_empty() {
                    app.tell(format!("{} put back.", cleancraft_core::fsutil::items(n)));
                } else {
                    app.warn(format!(
                        "{} put back, {} could not be: {}",
                        cleancraft_core::fsutil::items(n),
                        errs.len(),
                        errs[0].1
                    ));
                }
                app.apps.phase = Phase::Browse;
                let old = app.apps.detail.clone();
                rebuild_detail(app, old.as_ref());
                start_load(app);
            }
            if kit::pill(
                ui,
                &t,
                "Show History",
                kit::Kind::Secondary,
                Some(Icon::History),
            )
            .clicked()
            {
                app.act();
                app.apps.phase = Phase::Browse;
                app.navigate("history");
            }
            if kit::primary(ui, &t, "Done") {
                app.act();
                app.apps.phase = Phase::Browse;
                let old = app.apps.detail.clone();
                rebuild_detail(app, old.as_ref());
            }
        },
    );
}

/// An app bundle (the app itself, not one of its files).
fn is_app_bundle(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("app"))
}

/// What to do when the app itself stayed, by the reason it stayed.
pub fn app_kept_help(why: &str) -> &'static str {
    let w = why.to_lowercase();
    if w.contains("password was not given") {
        "Uninstall it again and enter the administrator password when macOS asks."
    } else if w.contains("refused") || w.contains("not permitted") || w.contains("permission") {
        "macOS protects this app. Drag it to the Trash in Finder, or turn on CleanCraft in System Settings > Privacy & Security > App Management, then uninstall it again."
    } else if w.contains("running") || w.contains("in use") || w.contains("open") {
        "Quit it, then uninstall it again."
    } else {
        "It is still installed. Uninstall it again, or drag it to the Trash in Finder."
    }
}

/// The System Settings pane that lets an app change other apps (macOS 13 and later).
pub const APP_MANAGEMENT_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AppBundles";

/// The ways forward for an app that stayed: Show in Finder (to drag it to the Trash) and,
/// when macOS refused the move, App Management in System Settings.
fn kept_app_buttons(app: &mut CleanApp, ui: &mut Ui, p: &Path, why: &str) {
    let t = app.t;
    let name = app_name(p);
    let r = kit::pill(
        ui,
        &t,
        "Show in Finder",
        kit::Kind::Secondary,
        Some(Icon::Folder),
    );
    r.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            true,
            crate::tr(format!("Show {name} in Finder")),
        )
    });
    if r.clicked() {
        app.act();
        let shown = app.env.display(p);
        if let Err(e) = app.env.sys.spawn("/usr/bin/open", &["-R", &shown]) {
            app.warn(format!("Could not open Finder: {e}"));
        }
    }
    if app_kept_help(why).contains("App Management")
        && kit::secondary(ui, &t, "Open App Management Settings")
    {
        app.act();
        if let Err(e) = app.env.sys.open_url(APP_MANAGEMENT_URL) {
            app.warn(format!("Could not open System Settings: {e}"));
        }
    }
}

// ---------- several apps: review, confirmation, result ----------

/// The review of the ticked apps: every app's files in its own group (folded on request),
/// a Quit banner for each running app, and one Uninstall for the apps that can go.
fn batch_review(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let quiet = matches!(app.apps.phase, Phase::Browse)
        && app
            .apps
            .batch
            .as_ref()
            .is_some_and(|b| b.confirm_force.is_none() && b.confirm_admin.is_none());
    let esc = quiet && ui.input(|i| i.key_pressed(Key::Escape));
    let back = kit::pill(ui, &t, "Back to Apps", kit::Kind::Ghost, Some(Icon::Back)).clicked();
    if esc || back {
        app.act();
        app.apps.batch = None;
        return;
    }
    let Some(b) = app.apps.batch.as_ref() else {
        return;
    };
    let plan = b.plan.clone();
    let apps = b.apps.clone();
    let running: Vec<PathBuf> = apps
        .iter()
        .filter(|a| a.running)
        .map(|a| a.path.clone())
        .collect();
    let quitting: Vec<PathBuf> = b.quitting.as_ref().map(|q| q.0.clone()).unwrap_or_default();
    let free = apps.len() - running.len();
    ui.add_space(space::S);
    // Headline: how many apps, what moves.
    ui.horizontal(|ui| {
        let total = human_size(plan.selected_size());
        let total_font = font(size::NUMBER, Weight::Bold);
        let total_w = kit::galley(ui, &total, total_font.clone(), t.text).size().x + 8.0;
        ui.vertical(|ui| {
            ui.set_max_width((ui.available_width() - total_w - 16.0).max(120.0));
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.label(
                crate::rich(if apps.len() == 1 {
                    "Uninstall 1 app".to_string()
                } else {
                    format!("Uninstall {} apps", apps.len())
                })
                .font(font(size::TITLE, Weight::Bold))
                .color(t.text),
            );
            ui.add(
                egui::Label::new(
                    crate::rich(
                        "Each app with the files named after it, selected for you. Check the rest before you tick it.",
                    )
                    .font(font(size::CALLOUT, Weight::Regular))
                    .color(t.text_dim),
                )
                .wrap(),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(crate::rich(total).font(total_font).color(icons::lerp(
                t.sec_a,
                Color32::WHITE,
                0.15,
            )));
        });
    });
    ui.add_space(space::S);
    // Running apps: said once at the top, with Quit All; each app's group has its own banner.
    if running.len() > 1 {
        let names: Vec<String> = apps
            .iter()
            .filter(|a| a.running)
            .map(|a| a.name.clone())
            .collect();
        let text = format!(
            "{} are running. {}",
            join_names(&names),
            if free > 0 {
                "Quit them to uninstall them too. The other apps can be uninstalled now."
            } else {
                "Quit them to uninstall them."
            }
        );
        let quit_all = if quitting.is_empty() {
            kit::banner(ui, &t, Icon::Warning, t.warning, &text, "Quit All")
        } else {
            kit::strip(ui, Icon::Warning, t.warning, &text);
            false
        };
        if quit_all {
            app.act();
            ask_batch_quit(app, running.clone());
        }
        ui.add_space(space::S);
    }
    let selected_apps = apps
        .iter()
        .filter(|a| {
            plan.selected().any(|i| {
                i.category == uninstaller::CAT_APP && i.group.as_deref() == Some(a.group.as_str())
            })
        })
        .count();
    kit::bottom_bar(ui, &t, egui::Id::new("apps-batch-bottom"), |ui| {
        let n = plan.selected_count();
        kit::bar_figure(
            ui,
            &t,
            &human_size(plan.selected_size()),
            &format!(
                "{} selected from {}",
                cleancraft_core::fsutil::items(n),
                if selected_apps == 1 {
                    "1 app".to_string()
                } else {
                    format!("{selected_apps} apps")
                }
            ),
            &format!(
                "{n} selected from {selected_apps} apps, {}",
                human_size(plan.selected_size())
            ),
            selected_apps == 0,
        );
        ui.add_space(space::M);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Ticking an app is always enough: if it still runs, Uninstall quits it first,
            // then carries on, instead of blocking until a separate Quit All.
            let ticked = apps.len();
            let can = ticked > 0 && quitting.is_empty();
            let label = if ticked == 1 {
                "Uninstall 1 App".to_string()
            } else {
                format!("Uninstall {ticked} Apps")
            };
            let kind = if can {
                kit::Kind::Danger
            } else {
                kit::Kind::Secondary
            };
            let un = ui
                .add_enabled_ui(can, |ui| kit::pill(ui, &t, &label, kind, None))
                .inner
                .on_disabled_hover_text(crate::tr("Quitting the running apps..."));
            if un.clicked() && can {
                app.act();
                if running.is_empty() {
                    app.apps.pending = app.apps.batch.as_ref().map(|b| b.plan.clone());
                    app.apps.phase = Phase::Confirm(Action::UninstallApps);
                } else {
                    if let Some(b) = app.apps.batch.as_mut() {
                        b.auto_uninstall = true;
                    }
                    ask_batch_quit(app, running.clone());
                }
            }
            if kit::pill(ui, &t, "Scan Again", kit::Kind::Secondary, None)
                .on_hover_text(crate::tr("Look at the apps' files again"))
                .clicked()
            {
                app.act();
                rebuild_batch(app);
            }
            if !running.is_empty() {
                kit::bar_hint(
                    ui,
                    &t,
                    &format!(
                        "{} {} running. Uninstall quits {} first.",
                        join_names(&running.iter().map(|p| app_name(p)).collect::<Vec<_>>()),
                        if running.len() == 1 { "is" } else { "are" },
                        if running.len() == 1 { "it" } else { "them" }
                    ),
                );
            }
        });
    });
    ui.add_space(space::XS);
    let h = ui.available_height() - 4.0;
    let mut toggles = Vec::new();
    let mut fold = None;
    let mut quit_one = None;
    let mut force_one = None;
    let mut leave = None;
    kit::card(&t)
        .inner_margin(egui::Margin::symmetric(8, 8))
        .show(ui, |ui| {
            ui.set_min_size(egui::vec2(ui.available_width(), h - 18.0));
            let fade_out = egui::ScrollArea::vertical()
                .id_salt("apps-batch")
                .max_height(h - 18.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (k, a) in apps.iter().enumerate() {
                        let items: Vec<&cleancraft_core::Item> = plan
                            .items
                            .iter()
                            .filter(|i| i.group.as_deref() == Some(a.group.as_str()))
                            .collect();
                        ui.add_space(if k == 0 { 4.0 } else { 14.0 });
                        let open = !app
                            .apps
                            .batch
                            .as_ref()
                            .is_some_and(|b| b.collapsed.contains(&a.path));
                        let acts =
                            batch_app_head(app, ui, a, &items, open, quitting.contains(&a.path));
                        if acts.fold {
                            fold = Some(a.path.clone());
                        }
                        if acts.quit {
                            quit_one = Some(a.path.clone());
                        }
                        if acts.force {
                            force_one = Some(a.path.clone());
                        }
                        if acts.leave {
                            leave = Some(a.path.clone());
                        }
                        if !open {
                            continue;
                        }
                        for it in items {
                            if let Some(on) = file_row(app, ui, it, a.lock.as_deref(), true) {
                                toggles.push((it.id, on));
                            }
                        }
                    }
                    ui.add_space(4.0);
                });
            kit::scroll_fade(ui, &fade_out, t.glass_solid());
        });
    for (id, on) in toggles {
        app.act();
        if let Some(b) = app.apps.batch.as_mut() {
            b.plan.set_selected(id, on);
        }
    }
    if let Some(p) = fold {
        app.act();
        if let Some(b) = app.apps.batch.as_mut()
            && !b.collapsed.remove(&p)
        {
            b.collapsed.insert(p);
        }
    }
    if let Some(p) = quit_one {
        app.act();
        ask_batch_quit(app, vec![p]);
    }
    if let Some(p) = force_one {
        app.act();
        if let Some(b) = app.apps.batch.as_mut() {
            b.confirm_force = Some(p);
        }
    }
    if let Some(p) = leave {
        app.act();
        leave_out(app, &p);
    }
    batch_force_dialog(app, ui);
    batch_admin_dialog(app, ui);
}

#[derive(Default)]
struct HeadActs {
    fold: bool,
    quit: bool,
    force: bool,
    leave: bool,
}

/// An app's group in the review: fold button, icon, name, what is selected, its state, and
/// (while it runs) its Quit banner.
fn batch_app_head(
    app: &CleanApp,
    ui: &mut Ui,
    a: &uninstaller::BatchApp,
    items: &[&cleancraft_core::Item],
    open: bool,
    quitting: bool,
) -> HeadActs {
    let t = app.t;
    let mut acts = HeadActs::default();
    let sel: Vec<&&cleancraft_core::Item> = items
        .iter()
        .filter(|i| i.selected && i.locked.is_none())
        .collect();
    let sel_size: u64 = sel.iter().map(|i| i.size).sum();
    let admin = sel.iter().any(|i| i.needs_admin);
    ui.horizontal(|ui| {
        ui.set_min_height(44.0);
        ui.add_space(4.0);
        let label = if open {
            format!("Hide files of {}", a.name)
        } else {
            format!("Show files of {}", a.name)
        };
        let (fr, fresp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
        fresp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, crate::tr(&label))
        });
        let feel = kit::feel_ring(ui, &t, &fresp, fr, 6.0);
        if feel.hov > 0.0 {
            ui.painter()
                .rect_filled(fr, 6.0, Color32::from_white_alpha((12.0 * feel.hov) as u8));
        }
        // The chevron points right when folded, down when open.
        let cr = Rect::from_center_size(fr.center(), vec2(12.0, 12.0));
        let ink = icons::lerp(t.text_dim, t.text, feel.hov);
        if open {
            let c = cr.center();
            ui.painter().line(
                vec![
                    pos2(c.x - 4.0, c.y - 2.0),
                    pos2(c.x, c.y + 2.5),
                    pos2(c.x + 4.0, c.y - 2.0),
                ],
                egui::Stroke::new(1.8, ink),
            );
        } else {
            icons::paint(ui.painter(), cr, Icon::Chevron, ink);
        }
        if fresp.clicked() {
            acts.fold = true;
        }
        ui.add_space(4.0);
        let (ir, _) = ui.allocate_exact_size(egui::Vec2::splat(32.0), Sense::hover());
        kit::app_icon(ui, ir, &a.name, Some(&a.path));
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(
                crate::rich(&a.name)
                    .font(font(size::BODY, Weight::SemiBold))
                    .color(t.text),
            );
            let line = if a.running {
                "Running: its files stay until it quits".to_string()
            } else {
                format!(
                    "{} selected, {}{}",
                    cleancraft_core::fsutil::items(sel.len()),
                    human_size(sel_size),
                    if admin { "  ·  needs password" } else { "" }
                )
            };
            ui.label(
                crate::rich(line)
                    .font(font(size::FOOTNOTE, Weight::Regular))
                    .color(if a.running { t.warning } else { t.text_dim }),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let r = kit::ghost(ui, &t, "Leave Out", None);
            r.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    true,
                    crate::tr(format!("Leave Out {}", a.name)),
                )
            });
            if r.on_hover_text(crate::tr(format!(
                "Keep {} and remove it from this list",
                a.name
            )))
            .clicked()
            {
                acts.leave = true;
            }
        });
    });
    if a.running {
        let note = app
            .apps
            .batch
            .as_ref()
            .and_then(|b| b.quit_notes.get(&a.path).cloned());
        let parts = app
            .apps
            .batch
            .as_ref()
            .and_then(|b| b.parts.get(&a.path).cloned());
        let (head, line) = match (&note, quitting) {
            (_, true) => (
                format!("Waiting for {} to quit...", a.name),
                format!("{} may ask you to save your work first.", a.name),
            ),
            (Some((h, l)), false) => (h.clone(), l.clone()),
            (None, false) => (
                parts
                    .as_ref()
                    .map(|s| s.headline())
                    .filter(|h| !h.is_empty())
                    .unwrap_or_else(|| format!("{} is open", a.name)),
                "Quit it to uninstall it with the others. Its files unlock as soon as it closes."
                    .to_string(),
            ),
        };
        ui.horizontal(|ui| {
            ui.add_space(36.0);
            egui::Frame::new()
                .fill(t.warning.gamma_multiply(0.10))
                .stroke(egui::Stroke::new(1.0, t.warning.gamma_multiply(0.35)))
                .corner_radius(egui::CornerRadius::same(crate::theme::radius::M))
                .inner_margin(egui::Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width() - 8.0);
                    ui.horizontal(|ui| {
                        let (r, _) =
                            ui.allocate_exact_size(egui::Vec2::splat(18.0), Sense::hover());
                        icons::paint(ui.painter(), r, Icon::Warning, t.warning);
                        ui.add_space(space::XS);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if quitting {
                                let (r, _) =
                                    ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                                let time = ui.input(|i| i.time);
                                kit::progress_ring(ui.painter(), r.center(), 16.0, &t, time);
                                ui.ctx().request_repaint();
                            } else {
                                let label = if note.is_some() {
                                    format!("Try Again to Quit {}", a.name)
                                } else {
                                    format!("Quit {}", a.name)
                                };
                                if kit::pill(ui, &t, &label, kit::Kind::Primary, None).clicked() {
                                    acts.quit = true;
                                }
                                if note.is_some() {
                                    ui.add_space(space::XS);
                                    let f =
                                        kit::pill(ui, &t, "Force Quit", kit::Kind::Secondary, None);
                                    f.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::Button,
                                            true,
                                            crate::tr(format!("Force Quit {}", a.name)),
                                        )
                                    });
                                    if f.on_hover_text(crate::tr(format!(
                                        "Stop {} at once",
                                        a.name
                                    )))
                                    .clicked()
                                    {
                                        acts.force = true;
                                    }
                                }
                            }
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.add(
                                        egui::Label::new(
                                            crate::rich(&head)
                                                .font(font(size::CALLOUT, Weight::SemiBold))
                                                .color(icons::lerp(
                                                    t.warning,
                                                    Color32::WHITE,
                                                    0.55,
                                                )),
                                        )
                                        .truncate(),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            crate::rich(&line)
                                                .font(font(size::FOOTNOTE, Weight::Regular))
                                                .color(t.text_dim),
                                        )
                                        .wrap(),
                                    );
                                    if let Some(st) = &parts {
                                        let lines: Vec<String> = st
                                            .running()
                                            .filter(|p| p.kind != app_quit::PartKind::App)
                                            .map(|p| format!("{}  ·  {}", p.name, p.role()))
                                            .collect();
                                        for l in lines.iter().take(3) {
                                            kit::dim(ui, &t, l);
                                        }
                                        if lines.len() > 3 {
                                            kit::dim(
                                                ui,
                                                &t,
                                                &format!("and {} more", lines.len() - 3),
                                            );
                                        }
                                    }
                                });
                            });
                        });
                    });
                });
        });
        ui.add_space(4.0);
    }
    acts
}

/// Force Quit one app of the review, after its confirmation.
fn batch_force_dialog(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let Some(path) = app
        .apps
        .batch
        .as_ref()
        .and_then(|b| b.confirm_force.clone())
    else {
        return;
    };
    let name = app_name(&path);
    let (mut go, mut close) = (false, false);
    let resp = kit::modal(egui::Id::new("apps-batch-force"), &t).show(ui.ctx(), |ui| {
        ui.set_width(480.0);
        crate::images::dialog_head_app(
            ui,
            &t,
            &path,
            &format!("Force quit {name}?"),
            "It stops at once, without asking to save. Work you have not saved in it is lost.",
        );
        ui.add_space(space::L);
        let (g, c) = kit::dialog_actions(ui, &t, "Force Quit", kit::Kind::Danger);
        go = g;
        close = c;
    });
    if resp.should_close() {
        close = true;
    }
    if go || close {
        app.act();
        if let Some(b) = app.apps.batch.as_mut() {
            b.confirm_force = None;
        }
    }
    if go {
        let admin = app
            .apps
            .batch
            .as_ref()
            .and_then(|b| b.parts.get(&path))
            .is_some_and(|s| s.needs_admin());
        start_batch_quit(app, vec![path], true, admin);
    }
}

/// A system service of an app to quit runs as root: stopping it asks for the password.
fn batch_admin_dialog(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let Some(paths) = app
        .apps
        .batch
        .as_ref()
        .and_then(|b| b.confirm_admin.clone())
    else {
        return;
    };
    let questions: Vec<String> = app
        .apps
        .batch
        .as_ref()
        .map(|b| {
            paths
                .iter()
                .filter_map(|p| b.parts.get(p).and_then(|s| s.admin_question()))
                .collect()
        })
        .unwrap_or_default();
    let names: Vec<String> = paths.iter().map(|p| app_name(p)).collect();
    let (mut all, mut rest, mut close) = (false, false, false);
    let resp = kit::modal(egui::Id::new("apps-batch-admin"), &t).show(ui.ctx(), |ui| {
        ui.set_width(520.0);
        kit::dialog_head(
            ui,
            &t,
            Icon::Warning,
            t.warning,
            &format!("Quit {}?", join_names(&names)),
            &questions.join(" "),
        );
        ui.add_space(space::L);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if kit::primary(ui, &t, "Quit and Enter Password") {
                all = true;
            }
            if kit::secondary(ui, &t, "Quit the Rest") {
                rest = true;
            }
            if kit::secondary(ui, &t, "Cancel") {
                close = true;
            }
        });
    });
    if resp.should_close() {
        close = true;
    }
    if all || rest || close {
        app.act();
        if let Some(b) = app.apps.batch.as_mut() {
            b.confirm_admin = None;
        }
    }
    if all || rest {
        start_batch_quit(app, paths, false, all);
    }
}

/// The one confirmation for several apps: every file, under its app, and the password note
/// once (one dialog for all of them).
fn batch_confirm(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let (Some(plan), Some(apps)) = (
        app.apps.pending.clone(),
        app.apps.batch.as_ref().map(|b| b.apps.clone()),
    ) else {
        app.apps.phase = Phase::Browse;
        return;
    };
    let dest = match app.settings.method {
        Method::Trash => "the Trash",
        Method::Quarantine | Method::AdminQuarantine => "the CleanCraft quarantine",
    };
    let going: Vec<&uninstaller::BatchApp> = apps
        .iter()
        .filter(|a| {
            plan.selected().any(|i| {
                i.category == uninstaller::CAT_APP && i.group.as_deref() == Some(a.group.as_str())
            })
        })
        .collect();
    let n = cleancraft_core::fsutil::items(plan.selected_count());
    let size = human_size(plan.selected_size());
    let verb = if plan.selected_count() == 1 {
        "moves"
    } else {
        "move"
    };
    let headline = format!(
        "Uninstall {}? {n} ({size}) {verb} to {dest}.",
        if going.len() == 1 {
            going[0].name.clone()
        } else {
            format!("{} apps", going.len())
        }
    );
    let staying: Vec<String> = apps
        .iter()
        .filter(|a| a.running)
        .map(|a| a.name.clone())
        .collect();
    let (mut go, mut close) = (false, false);
    let width = (ui.ctx().content_rect().width() - 120.0).clamp(420.0, 680.0);
    let max_list = (ui.ctx().content_rect().height() - 360.0).clamp(120.0, 380.0);
    let resp = kit::modal(egui::Id::new("apps-batch-confirm"), &t).show(ui.ctx(), |ui| {
        ui.set_width(width);
        kit::dialog_head(
            ui,
            &t,
            Icon::Remover,
            t.sec_a,
            &headline,
            "These exact files will be moved. You can put them back from History, all at once or app by app.",
        );
        ui.add_space(space::M);
        if let Some(note) =
            super::plan_page::admin_note(cleancraft_core::features::admin_count(&plan))
        {
            kit::strip(ui, Icon::Warning, t.warning, &note);
            ui.add_space(space::S);
        }
        if !staying.is_empty() {
            kit::strip(
                ui,
                Icon::Lock,
                t.text_dim,
                &format!(
                    "{} {} running and {} until you quit {}.",
                    join_names(&staying),
                    if staying.len() == 1 { "is" } else { "are" },
                    if staying.len() == 1 { "stays" } else { "stay" },
                    if staying.len() == 1 { "it" } else { "them" }
                ),
            );
            ui.add_space(space::S);
        }
        kit::well(&t).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt("apps-batch-confirm-list")
                .max_height(max_list)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for a in &going {
                        let rows: Vec<&cleancraft_core::Item> = plan
                            .selected()
                            .filter(|i| i.group.as_deref() == Some(a.group.as_str()))
                            .collect();
                        let sz: u64 = rows.iter().map(|i| i.size).sum();
                        ui.add_space(6.0);
                        ui.label(
                            crate::rich(format!(
                                "{}  ·  {}, {}",
                                a.name,
                                cleancraft_core::fsutil::items(rows.len()),
                                human_size(sz)
                            ))
                            .font(font(size::CALLOUT, Weight::SemiBold))
                            .color(t.text),
                        );
                        for it in rows {
                            kit::confirm_line(
                                ui,
                                &t,
                                &it.path,
                                &app.env.display(&it.path),
                                &it.reason,
                                &human_size(it.size),
                            );
                        }
                    }
                });
        });
        ui.add_space(space::L);
        let label = match app.settings.method {
            Method::Trash => "Move to Trash",
            Method::Quarantine | Method::AdminQuarantine => "Move to Quarantine",
        };
        let (g, c) = kit::dialog_actions(ui, &t, label, kit::Kind::Danger);
        go = g;
        close = c;
    });
    if resp.should_close() {
        close = true;
    }
    if go {
        app.act();
        start_action(app, Action::UninstallApps);
        poll(app);
    } else if close {
        app.act();
        app.apps.pending = None;
        app.apps.phase = Phase::Browse;
    }
}

/// The result of several apps: what moved, per app, with Undo All and Undo per app.
fn batch_done(app: &mut CleanApp, ui: &mut Ui) {
    let Phase::Done(rep, _) = &app.apps.phase else {
        return;
    };
    let rep = rep.clone();
    let results = app
        .apps
        .batch
        .as_ref()
        .map(|b| b.results.clone())
        .unwrap_or_default();
    let undone = app
        .apps
        .batch
        .as_ref()
        .map(|b| b.undone.clone())
        .unwrap_or_default();
    let running: BTreeSet<PathBuf> = app
        .apps
        .batch
        .as_ref()
        .map(|b| {
            b.apps
                .iter()
                .filter(|a| a.running)
                .map(|a| a.path.clone())
                .collect()
        })
        .unwrap_or_default();
    let removed = results.iter().filter(|r| r.removed).count();
    let summary = format!(
        "{} {} uninstalled. {}",
        removed,
        if removed == 1 { "app" } else { "apps" },
        kit::moved_summary(rep.moved.len(), rep.skipped.len())
    );
    let t = app.t;
    let started = kit::first_seen(ui, ("done", &rep.plan_id));
    let id = egui::Id::new(("batch-done", &rep.plan_id));
    let mut undo_all = false;
    let mut undo_one: Option<PathBuf> = None;
    let mut history = false;
    let mut finish = false;
    kit::centered_block(ui, id, |ui| {
        let d = if ui.available_height() < 560.0 {
            110.0
        } else {
            150.0
        };
        kit::done_hero(ui, &t, FeatureId::Uninstaller, d, started);
        ui.add_space(space::M);
        ui.label(
            crate::rich(if removed == 0 {
                "No app was uninstalled".to_string()
            } else {
                features::moved_text([&rep])
            })
            .font(font(size::LARGE_TITLE, Weight::Bold))
            .color(t.text),
        );
        ui.add_space(space::XS);
        ui.label(
            crate::rich(&summary)
                .font(font(size::BODY_L, Weight::Regular))
                .color(t.text_dim),
        );
        ui.add_space(space::L);
        kit::centered_row(ui, id.with("buttons"), |ui| {
            let any = rep.moved.len()
                > results
                    .iter()
                    .filter(|r| undone.contains(&r.path))
                    .map(|r| r.moved.len())
                    .sum::<usize>();
            if any
                && kit::pill(ui, &t, "Undo All", kit::Kind::Secondary, Some(Icon::Undo))
                    .on_hover_text(crate::tr("Put back every app and file of this uninstall"))
                    .clicked()
            {
                undo_all = true;
            }
            if kit::pill(
                ui,
                &t,
                "Show History",
                kit::Kind::Secondary,
                Some(Icon::History),
            )
            .clicked()
            {
                history = true;
            }
            if kit::primary(ui, &t, "Done") {
                finish = true;
            }
        });
        ui.add_space(space::L);
        let h = (ui.available_height() - 40.0).max(160.0);
        kit::card(&t)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width().min(760.0));
                egui::ScrollArea::vertical()
                    .id_salt("apps-batch-results")
                    .max_height(h)
                    .show(ui, |ui| {
                        for r in &results {
                            if result_row(
                                app,
                                ui,
                                r,
                                undone.contains(&r.path),
                                running.contains(&r.path),
                            ) {
                                undo_one = Some(r.path.clone());
                            }
                        }
                    });
            });
    });
    if undo_all || history || finish || undo_one.is_some() {
        app.act();
    }
    if let Some(p) = undo_one {
        let (n, errs) = uninstaller::undo_batch_app(&app.env, &rep.plan_id, &p);
        report_undo(app, n, &errs);
        if let Some(b) = app.apps.batch.as_mut() {
            b.undone.insert(p);
        }
        app.files_back(&[]);
        start_load(app);
    }
    if undo_all {
        let (n, errs) = executor::undo_plan(&app.env, &rep.plan_id);
        report_undo(app, n, &errs);
        app.files_back(&CleanApp::moved_paths(&rep));
        app.apps.phase = Phase::Browse;
        app.apps.batch = None;
        start_load(app);
    }
    if history {
        app.apps.phase = Phase::Browse;
        app.apps.batch = None;
        app.navigate("history");
    }
    if finish {
        app.apps.phase = Phase::Browse;
        app.apps.batch = None;
    }
}

fn report_undo(app: &mut CleanApp, n: usize, errs: &[(String, executor::UndoError)]) {
    if errs.is_empty() {
        app.tell(format!("{} put back.", cleancraft_core::fsutil::items(n)));
    } else {
        app.warn(format!(
            "{} put back, {} could not be: {}",
            cleancraft_core::fsutil::items(n),
            errs.len(),
            errs[0].1
        ));
    }
}

/// One app's result: uninstalled or not, what moved, every file that stayed with the
/// reason, and Undo for it. Returns true when its Undo was clicked.
fn result_row(
    app: &CleanApp,
    ui: &mut Ui,
    r: &uninstaller::AppResult,
    undone: bool,
    was_running: bool,
) -> bool {
    let t = app.t;
    let mut undo = false;
    ui.horizontal(|ui| {
        ui.set_min_height(42.0);
        let (ir, _) = ui.allocate_exact_size(egui::Vec2::splat(30.0), Sense::hover());
        kit::app_icon(ui, ir, &r.name, Some(&r.path));
        ui.add_space(6.0);
        let (state, color) = if undone {
            ("Put back".to_string(), t.text_dim)
        } else if r.removed {
            (
                format!(
                    "Uninstalled  ·  {} moved, {}",
                    cleancraft_core::fsutil::items(r.moved.len()),
                    human_size(r.moved_size)
                ),
                t.success,
            )
        } else if r.moved.is_empty() && was_running {
            (
                "Not removed: it was running. Quit it, then uninstall it.".to_string(),
                t.warning,
            )
        } else if r.moved.is_empty() {
            ("Not removed".to_string(), t.warning)
        } else {
            (
                format!(
                    "Kept: the app itself did not move  ·  {} moved",
                    cleancraft_core::fsutil::items(r.moved.len())
                ),
                t.warning,
            )
        };
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(
                crate::rich(&r.name)
                    .font(font(size::BODY, Weight::SemiBold))
                    .color(t.text),
            );
            ui.label(
                crate::rich(&state)
                    .font(font(size::FOOTNOTE, Weight::Medium))
                    .color(color),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if !undone && !r.moved.is_empty() {
                let b = kit::pill(ui, &t, "Undo", kit::Kind::Secondary, Some(Icon::Undo));
                b.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        crate::tr(format!("Undo {}", r.name)),
                    )
                });
                if b.on_hover_text(crate::tr(format!("Put back {} and its files", r.name)))
                    .clicked()
                {
                    undo = true;
                }
            }
        });
    });
    // The app itself stayed: what to do next, in full (fix7).
    if !undone
        && !r.removed
        && let Some((_, why)) = r.skipped.iter().find(|(p, _)| is_app_bundle(p))
    {
        ui.horizontal(|ui| {
            ui.add_space(42.0);
            ui.add(
                egui::Label::new(
                    crate::rich(format!(
                        "Still installed. {}",
                        app_kept_help(&why.to_string())
                    ))
                    .font(font(size::FOOTNOTE, Weight::Medium))
                    .color(t.warning),
                )
                .wrap(),
            );
        });
    }
    for (p, why) in &r.skipped {
        ui.horizontal(|ui| {
            ui.add_space(42.0);
            let (ir, _) = ui.allocate_exact_size(egui::Vec2::splat(14.0), Sense::hover());
            icons::paint(ui.painter(), ir, Icon::Warning, t.warning);
            let shown = app.env.display(p);
            let line = format!("{}: {why}", short_name(&shown));
            let r = ui.add(
                egui::Label::new(
                    crate::rich(&line)
                        .font(font(size::FOOTNOTE, Weight::Regular))
                        .color(t.text_dim),
                )
                .truncate(),
            );
            r.on_hover_text(crate::tr(format!("{shown}: {why}")));
        });
    }
    ui.add_space(6.0);
    undo
}

fn short_name(shown: &str) -> &str {
    split_shown(shown).1
}

/// In History, an uninstall of several apps: one line per app with its own Undo (the card's
/// Undo All puts back everything at once).
pub fn history_batch_rows(app: &mut CleanApp, ui: &mut Ui, plan_id: &str) {
    if !plan_id.starts_with(uninstaller::BATCH_PREFIX) {
        return;
    }
    let t = app.t;
    let entries = uninstaller::batch_entries(&app.env, plan_id);
    let mut undo = None;
    for e in &entries {
        let ids = uninstaller::batch_app_records(&app.env, plan_id, &e.path);
        ui.horizontal(|ui| {
            ui.add_space(36.0);
            let (ir, _) = ui.allocate_exact_size(egui::Vec2::splat(22.0), Sense::hover());
            kit::app_icon(ui, ir, &e.name, Some(&e.path));
            ui.label(
                crate::rich(&e.name)
                    .font(font(size::CALLOUT, Weight::Medium))
                    .color(t.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ids.is_empty() {
                    kit::dim(ui, &t, "Put back");
                    return;
                }
                let b = kit::pill(ui, &t, "Undo", kit::Kind::Ghost, Some(Icon::Undo));
                b.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        crate::tr(format!("Undo {}", e.name)),
                    )
                });
                if b.on_hover_text(crate::tr(format!("Put back {} and its files", e.name)))
                    .clicked()
                {
                    undo = Some(e.path.clone());
                }
            });
        });
    }
    if let Some(p) = undo {
        app.act();
        let (n, errs) = uninstaller::undo_batch_app(&app.env, plan_id, &p);
        report_undo(app, n, &errs);
        app.files_back(&[]);
        app.history.reload(&app.env);
    }
}

// ---------- updates ----------

fn updates_page(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    if let Some(job) = &app.apps.upd_job {
        let s = job.status();
        let step = if s.is_empty() {
            String::new()
        } else {
            format!("Checking {s}...")
        };
        kit::progress(
            ui,
            &t,
            FeatureId::Updater,
            "Checking for updates",
            &step,
            &job.meter.clone(),
            false,
        );
        return;
    }
    let Some(r) = app.apps.updates.clone() else {
        if kit::hero(
            ui,
            &t,
            FeatureId::Updater,
            kit::HeroText {
                title: FeatureId::Updater.title(),
                desc: "Newer versions of your apps and of macOS. CleanCraft opens the right place to install them.",
                covers: super::covers(FeatureId::Updater),
                action: "Check for Updates",
                note: "CleanCraft asks each app's update feed and the App Store, then hands the update to the app or the App Store.",
                enabled: true,
            },
            |_| {},
        ) {
            app.act();
            start_updates(app);
            poll(app);
        }
        return;
    };
    let all_avail: Vec<UpdateInfo> = r.available().cloned().collect();
    let avail_total = all_avail.len();
    let mut prefs = kit_list::prefs(&app.settings, UPDATES_LIST);
    let filters = update_filters(&all_avail);
    let avail = visible_updates(app, &all_avail, &prefs, &filters);
    ui.horizontal(|ui| {
        ui.label(
            crate::rich(match avail_total {
                0 => "All checked apps are up to date".to_string(),
                1 => "1 update available".to_string(),
                n => format!("{n} updates available"),
            })
            .font(font(size::TITLE2, Weight::Bold))
            .color(t.text),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if kit::secondary(ui, &t, "Check Again") {
                app.act();
                start_updates(app);
                poll(app);
            }
        });
    });
    ui.add_space(space::S);
    if avail_total > 1 {
        let mut changed = false;
        let mut search = std::mem::take(&mut app.apps.upd_search);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = space::S;
            let (_, ch) = kit_list::search(
                ui,
                &t,
                &mut search,
                "Search updates",
                "Search updates",
                240.0,
            );
            if ch {
                app.act();
            }
            changed |= kit_list::menus(ui, &t, UPDATES_LIST, &filters, &UPDATE_SORTS, &mut prefs);
        });
        let st = kit_list::status(
            ui,
            &t,
            avail.len(),
            avail_total,
            ("update", "updates"),
            &filters,
            &mut prefs,
            &mut search,
        );
        changed |= st.prefs;
        app.apps.upd_search = search;
        if changed {
            app.act();
            if kit_list::store(&mut app.settings, UPDATES_LIST, prefs) {
                app.save_settings();
            }
        }
        ui.add_space(space::S);
    }
    let h = ui.available_height() - 8.0;
    let fade_out = egui::ScrollArea::vertical()
        .id_salt("updates")
        .max_height(h)
        .show(ui, |ui| {
            for u in &avail {
                update_row(app, ui, u);
            }
            let failed: Vec<&UpdateInfo> = r.apps.iter().filter(|u| u.error.is_some()).collect();
            let current: Vec<&UpdateInfo> = r
                .apps
                .iter()
                .filter(|u| !u.available && u.error.is_none())
                .collect();
            ui.add_space(space::S);
            // Two cards side by side: your apps that need nothing, and macOS, of one height
            // (the taller one's, from the frame before: REVIEW-C C36).
            let h_key = egui::Id::new("updates-cards-h");
            let same_h: f32 = ui.data(|d| d.get_temp(h_key)).unwrap_or(0.0);
            let mut tallest = 0.0f32;
            ui.columns(2, |cols| {
                // Columns are justified; text in a card is not.
                cols[0].with_layout(egui::Layout::top_down(Align::Min), |ui| {
                kit::card(&t).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(same_h);
                    kit::card_head(ui, &t, Icon::Check, t.success, "Up to date");
                    ui.add_space(space::XS);
                    if current.is_empty() {
                        kit::dim(ui, &t, "No other app was checked.");
                    }
                    for u in &current {
                        kit::dim(
                            ui,
                            &t,
                            &format!("{}  {}  ({})", u.name, u.installed, u.source.label()),
                        );
                    }
                    if r.not_checked > 0 {
                        ui.add_space(space::XS);
                        kit::dim(
                            ui,
                            &t,
                            &format!(
                                "{} apps have no update feed CleanCraft can read; they update themselves.",
                                r.not_checked
                            ),
                        );
                    }
                    if !failed.is_empty() {
                        ui.add_space(space::S);
                        ui.label(
                            crate::rich("Could not check")
                                .font(font(size::BODY, Weight::SemiBold)),
                        );
                        for u in &failed {
                            ui.label(
                                crate::rich(format!(
                                    "{}: {}",
                                    u.name,
                                    u.error.clone().unwrap_or_default()
                                ))
                                .font(font(size::CALLOUT, Weight::Regular))
                                .color(t.warning),
                            );
                        }
                    }
                    tallest = tallest.max(ui.min_rect().height());
                });
                });
                // Columns are justified; text in a card is not.
                cols[1].with_layout(egui::Layout::top_down(Align::Min), |ui| {
                kit::card(&t).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(same_h);
                    kit::card_head(ui, &t, Icon::Gear, t.sec_a, "macOS");
                    ui.add_space(space::XS);
                    if let Some(e) = &r.macos_error {
                        ui.label(crate::rich(e).color(t.warning));
                    } else if r.macos.is_empty() {
                        kit::dim(ui, &t, "macOS is up to date.");
                    }
                    for m in &r.macos {
                        // Shown: the title (with the version once) and the size, then a
                        // restart tag. Announced: the whole line.
                        let mut line = format!("{}  {}", m.title, m.version);
                        let mut shown = if m.title.contains(&m.version) {
                            m.title.clone()
                        } else {
                            format!("{}  {}", m.title, m.version)
                        };
                        if let Some(b) = m.size_bytes {
                            line.push_str(&format!("  ·  {}", human_size(b)));
                            shown.push_str(&format!("  ·  {}", human_size(b)));
                        }
                        if m.restart {
                            line.push_str("  ·  needs a restart");
                        }
                        let fid = font(size::BODY, Weight::Regular);
                        let w = kit::galley(ui, &shown, fid.clone(), t.text).size().x + 2.0;
                        let (rr, _) = ui.allocate_exact_size(
                            vec2(w.min(ui.available_width()), 20.0),
                            Sense::hover(),
                        );
                        kit::text_at(ui, rr, &shown, &line, fid, t.text, Align::Min);
                        if m.restart {
                            kit::tag(ui, "Restart required", t.warning);
                        }
                    }
                    if !r.macos.is_empty() || r.macos_error.is_some() {
                        ui.add_space(space::XS);
                        if kit::secondary(ui, &t, "Open Software Update") {
                            app.act();
                            if let Err(e) = updater::open_software_update(&app.env) {
                                app.warn(format!("Could not open System Settings: {e}"));
                            }
                        }
                    }
                    tallest = tallest.max(ui.min_rect().height());
                });
                });
            });
            if (tallest - same_h).abs() > 0.5 {
                ui.data_mut(|d| d.insert_temp(h_key, tallest));
                ui.ctx().request_discard("update cards take one height");
            }
        });
    kit::scroll_fade_page(ui, &fade_out, &t);
}

/// App Updates' list in the settings.
pub const UPDATES_LIST: &str = "updates";
/// App Updates' sort orders.
pub const UPDATE_SORTS: [kit_list::SortKey; 2] = [kit_list::BY_NAME, kit_list::BY_SIZE];

fn update_has(u: &UpdateInfo, key: &str) -> bool {
    match key {
        "app_store" => u.source == Source::AppStore,
        "developer" => u.source == Source::Sparkle,
        _ => false,
    }
}

fn update_filters(all: &[UpdateInfo]) -> Vec<Filter> {
    let n = |k: &str| all.iter().filter(|u| update_has(u, k)).count();
    vec![
        Filter::any("Source", "app_store", "App Store", n("app_store")),
        Filter::any("Source", "developer", "From the developer", n("developer")),
    ]
}

/// The updates shown: searched, filtered and sorted (size: the installed app's size).
fn visible_updates(
    app: &CleanApp,
    all: &[UpdateInfo],
    prefs: &ListPrefs,
    filters: &[Filter],
) -> Vec<UpdateInfo> {
    let q = kit_list::query(&app.apps.upd_search);
    let size_of = |u: &UpdateInfo| {
        app.apps
            .list
            .iter()
            .flatten()
            .find(|a| a.path == u.app)
            .map(|a| a.size)
            .unwrap_or(0)
    };
    let mut v: Vec<UpdateInfo> = all
        .iter()
        .filter(|u| kit_list::matches(&q, &[&u.name, &u.bundle_id]))
        .filter(|u| kit_list::passes(prefs, filters, |k| update_has(u, k)))
        .cloned()
        .collect();
    let (key, rev) = kit_list::sort_of(prefs, &UPDATE_SORTS);
    v.sort_by(|a, b| {
        let o = match key.key {
            "size" => size_of(b).cmp(&size_of(a)),
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        };
        if rev { o.reverse() } else { o }
    });
    v
}

fn update_row(app: &mut CleanApp, ui: &mut Ui, u: &UpdateInfo) {
    let t = app.t;
    kit::card(&t)
        .inner_margin(egui::Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(
                    egui::Vec2::splat(crate::theme::icon::APP_ROW + 8.0),
                    Sense::hover(),
                );
                kit::app_icon(ui, r, &u.name, Some(&u.app));
                ui.add_space(space::XS);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        crate::rich(&u.name)
                            .font(font(size::TITLE3, Weight::SemiBold))
                            .color(t.text),
                    );
                    ui.horizontal(|ui| {
                        ui.label(
                            crate::rich(format!(
                                "{} -> {}",
                                u.installed,
                                u.latest.clone().unwrap_or_default()
                            ))
                            .font(font(size::FOOTNOTE, Weight::Medium))
                            .color(icons::lerp(
                                t.sec_a,
                                Color32::WHITE,
                                0.2,
                            )),
                        );
                        kit::dim(ui, &t, u.source.label());
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let r = kit::pill(ui, &t, "Update", kit::Kind::Primary, None).on_hover_text(
                        crate::tr(match u.source {
                            Source::Sparkle => "Open the app so its own updater installs the new version",
                            _ => "Open the App Store page to install the new version",
                        }),
                    );
                    r.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            crate::tr(format!("Update {}", u.name)),
                        )
                    });
                    if r.clicked() {
                        app.act();
                        match updater::start_update(&app.env, u) {
                            Ok(msg) => app.tell(msg),
                            Err(e) => app.warn(format!("Could not open {}: {e}", u.name)),
                        }
                    }
                    if u.source == Source::Sparkle && u.download_url.is_some() {
                        let r = kit::secondary_resp(ui, &t, "Download Only").on_hover_text(
                            crate::tr("Open the new version's download in your browser; you install it yourself"),
                        );
                        r.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                crate::tr(format!("Download {}", u.name)),
                            )
                        });
                        if r.clicked() {
                            app.act();
                            match updater::open_download(&app.env, u) {
                                Ok(()) => app.tell(format!(
                                    "The download of {} {} opened in your browser.",
                                    u.name,
                                    u.latest.clone().unwrap_or_default()
                                )),
                                Err(e) => app.warn(format!("Could not open the download: {e}")),
                            }
                        }
                    }
                    if !u.notes.is_empty() {
                        let open = app.apps.notes_open.contains(&u.app);
                        let r = kit::secondary_resp(
                            ui,
                            &t,
                            if open { "Hide Notes" } else { "What's New" },
                        );
                        r.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                crate::tr(format!("What's new in {}", u.name)),
                            )
                        });
                        if r.clicked() {
                            app.act();
                            if open {
                                app.apps.notes_open.remove(&u.app);
                            } else {
                                app.apps.notes_open.insert(u.app.clone());
                            }
                        }
                    }
                });
            });
            if app.apps.notes_open.contains(&u.app) {
                ui.label(crate::rich(&u.notes).color(t.text_dim));
            }
        });
    ui.add_space(space::S);
}
