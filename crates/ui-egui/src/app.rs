//! The window: sidebar, the Full Disk Access banner, notices, and the current page.

use std::collections::BTreeMap;

use cleancraft_core::permissions::{self, Access};
use cleancraft_core::route::AppRoute;
use cleancraft_core::{Env, FeatureId, Settings};
use egui::{Color32, CornerRadius, Sense, Stroke, Ui, vec2};

use crate::icons::{self, Icon};
use crate::kit;
use crate::pages::{self, plan_page::PlanState};
use crate::section_icons::{self, SectionIcon};
use crate::theme::{self, Section, Tokens, Weight, font, icon as isz, motion, size, space};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Feature(FeatureId),
    /// Mac Health, a tool of the Live Monitor section ([`EXTRA_TOOLS`]).
    Health,
    History,
    Settings,
    /// The guide "Connect an AI agent" (docs/guide-ai-agents.md), opened from Settings.
    AgentGuide,
    /// "Use CleanCraft from your AI apps?": the last step of the first launch (when an AI app
    /// is found), later from Settings.
    ConnectAi,
    /// The first launch's welcome and Full Disk Access steps (U7-5).
    Welcome,
}

/// The sidebar's entries, in order: one per section, each opening one or more tools. Names
/// are CleanCraft's own. A section with several tools shows them as a switcher at the top of
/// its page.
pub const GROUPS: &[(&str, &[FeatureId])] = &[
    ("Quick Sweep", &[FeatureId::SmartCare]),
    (
        "Clean",
        &[
            FeatureId::SystemJunk,
            FeatureId::MailAttachments,
            FeatureId::TrashBins,
            FeatureId::DevClutter,
        ],
    ),
    ("Protect", &[FeatureId::Malware, FeatureId::Privacy]),
    ("Speed", &[FeatureId::Maintenance, FeatureId::LoginItems]),
    ("Apps", &[FeatureId::Uninstaller, FeatureId::Updater]),
    (
        "Files",
        &[
            FeatureId::LargeOldFiles,
            FeatureId::Duplicates,
            FeatureId::SimilarPhotos,
            FeatureId::SpaceLens,
        ],
    ),
    ("Live Monitor", &[FeatureId::Monitor]),
];

/// The sidebar entry (index in [`GROUPS`]) a tool belongs to.
pub fn group_of(f: FeatureId) -> usize {
    GROUPS
        .iter()
        .position(|(_, fs)| fs.contains(&f))
        .unwrap_or(0)
}

/// Tools of a section that are not modules, shown after its modules in the switcher:
/// (section index in [`GROUPS`], page, title).
pub const EXTRA_TOOLS: &[(usize, Page, &str)] = &[(6, Page::Health, "Mac Health")];

/// The section a page belongs to (None for History, Settings and the guides).
pub fn page_group(p: Page) -> Option<usize> {
    match p {
        Page::Feature(f) => Some(group_of(f)),
        other => EXTRA_TOOLS
            .iter()
            .find(|(_, x, _)| *x == other)
            .map(|(i, _, _)| *i),
    }
}

/// Every tool of a section, in switcher order: its modules, then its extra tools.
pub fn section_pages(i: usize) -> Vec<Page> {
    let mut v: Vec<Page> = GROUPS[i].1.iter().map(|f| Page::Feature(*f)).collect();
    v.extend(
        EXTRA_TOOLS
            .iter()
            .filter(|(g, _, _)| *g == i)
            .map(|(_, p, _)| *p),
    );
    v
}

/// A tool's switcher label and icon.
pub fn page_look(t: &Tokens, p: Page) -> (Icon, &'static str) {
    match p {
        Page::Feature(f) => (kit::feature_look(t, f).0, f.title()),
        Page::Health => (Icon::Check, "Mac Health"),
        Page::History => (Icon::History, "History"),
        Page::Settings => (Icon::Settings, "Settings"),
        Page::AgentGuide => (Icon::File, "Connect an AI Agent"),
        Page::ConnectAi => (Icon::App, "AI Apps"),
        Page::Welcome => (Icon::Check, "Welcome"),
    }
}

/// The icon of a sidebar entry (motion3's small 3D section icons).
pub fn group_icon(i: usize) -> SectionIcon {
    match i {
        0 => SectionIcon::QuickSweep,
        1 => SectionIcon::Clean,
        2 => SectionIcon::Protect,
        3 => SectionIcon::Speed,
        4 => SectionIcon::Apps,
        5 => SectionIcon::Files,
        _ => SectionIcon::Monitor,
    }
}

pub struct CleanApp {
    pub env: Env,
    pub settings: Settings,
    pub page: Page,
    pub t: Tokens,
    /// Run work on the UI thread (tests).
    pub sync: bool,
    pub plans: BTreeMap<FeatureId, PlanState>,
    pub history: pages::history::State,
    pub settings_page: pages::settings::State,
    pub threat_rules: pages::threat_rules::State,
    pub sweep: pages::sweep::State,
    pub apps: pages::apps::State,
    pub startup: pages::startup::State,
    pub tasks: pages::tasks::State,
    pub disk_map: pages::disk_map::State,
    pub monitor: pages::monitor::State,
    pub health: pages::health::State,
    pub guide: pages::guide::State,
    pub welcome: pages::welcome::State,
    /// CleanCraft's own updates (Settings > Updates and the banner).
    pub updates: pages::updates::State,
    pub fda: Access,
    /// A message at the top of the page: (text, is_error).
    pub notice: Option<(String, bool)>,
    /// What the notice's Undo button does, when it has one (REVIEW-U N3, U5).
    notice_undo: Option<NoticeUndo>,
    actions: u64,
    fonts_installed: bool,
    fonts_pass: u64,
    /// The page the transition animation is running for, and when it started.
    shown_page: Page,
    page_since: f64,
    /// The tool each sidebar section showed last (by index in [`GROUPS`]), so a section
    /// opens where the person left it.
    pub section_tool: BTreeMap<usize, Page>,
    /// Grows by one each time files leave their place or come back (a cleanup, an uninstall,
    /// a move from Disk Map, an Undo). Pages that keep their own lists compare it with the
    /// value they last saw and reload or drop what moved.
    pub disk_epoch: u64,
    /// The original paths the last cleanup moved (empty after an Undo).
    pub last_moved: Vec<std::path::PathBuf>,
    /// CleanCraft's action log as last seen: moves and Undos from any page (App Remover,
    /// Disk Map, History, Startup) reach the plan pages and Quick Sweep through it.
    journal: pages::history::JournalWatch,
    /// "Not Now" on the Full Disk Access banner (for this session).
    pub fda_dismissed: bool,
    /// The Threat Check finding a route asked to select once the scan shows it.
    pub pending_threat: Option<std::path::PathBuf>,
    /// What Mac Health has noted already: scans (by plan id and what is left), the last
    /// Disk Map tree, the last update check, and whether launch work ran.
    observed: Observed,
    /// egui's clock (seconds) at the last frame: schedules use it, so tests can move time.
    pub now: f64,
    /// The window is minimised or fully covered: nothing ambient runs, Live Monitor pauses.
    pub window_hidden: bool,
    /// The egui context, so background work can wake the window when it finishes (no
    /// polling frames while it runs).
    pub egui_ctx: Option<egui::Context>,
}

#[derive(Default)]
struct Observed {
    plans: BTreeMap<FeatureId, (String, u64, usize)>,
    map: Option<(std::path::PathBuf, u64, u64)>,
    /// A Disk Map summary waiting to be written (when the person leaves Disk Map: writing
    /// into the mapped folder while it is shown would make the map one file out of date).
    map_pending: Option<cleancraft_core::health::MapSummary>,
    updates: Option<usize>,
    launched: bool,
}

/// The action behind a notice's Undo button.
/// Its accessible name says what it takes back ("Undo Ignore").
struct NoticeUndo(String, Box<dyn FnOnce(&mut CleanApp)>);

/// Sidebar entry height.
const NAV_ROW: f32 = 44.0;

impl CleanApp {
    pub fn new(env: Env) -> CleanApp {
        let (settings, settings_warning) = Settings::load_checked(&env);
        cleancraft_core::i18n::apply(&env, &settings);
        let fda = permissions::full_disk_access(&env);
        CleanApp {
            env,
            settings,
            page: Page::Feature(FeatureId::SmartCare),
            t: Tokens::dark(),
            sync: false,
            plans: BTreeMap::new(),
            history: Default::default(),
            settings_page: Default::default(),
            threat_rules: Default::default(),
            sweep: Default::default(),
            apps: Default::default(),
            startup: Default::default(),
            tasks: Default::default(),
            disk_map: Default::default(),
            monitor: Default::default(),
            health: Default::default(),
            guide: Default::default(),
            welcome: Default::default(),
            updates: Default::default(),
            fda,
            notice: settings_warning.map(|m| (m, true)),
            notice_undo: None,
            actions: 0,
            fonts_installed: false,
            fonts_pass: 0,
            shown_page: Page::Feature(FeatureId::SmartCare),
            page_since: f64::NEG_INFINITY,
            section_tool: BTreeMap::new(),
            disk_epoch: 0,
            last_moved: Vec::new(),
            journal: Default::default(),
            fda_dismissed: false,
            pending_threat: None,
            observed: Observed::default(),
            now: 0.0,
            window_hidden: false,
            egui_ctx: None,
        }
    }

    /// Work runs inline (for tests).
    pub fn with_sync(mut self) -> CleanApp {
        self.sync = true;
        self
    }

    /// Count a handled user action. The every-control test uses this to find dead clicks.
    pub fn act(&mut self) {
        self.actions += 1;
    }

    pub fn action_count(&self) -> u64 {
        self.actions
    }

    pub fn tell(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), false));
        self.notice_undo = None;
    }

    /// A notice with an Undo button that runs `undo` (and closes the notice). The button
    /// reads "Undo" and is announced "Undo <what>".
    pub fn tell_undo(
        &mut self,
        msg: impl Into<String>,
        what: &str,
        undo: impl FnOnce(&mut CleanApp) + 'static,
    ) {
        self.notice = Some((msg.into(), false));
        self.notice_undo = Some(NoticeUndo(format!("Undo {what}"), Box::new(undo)));
    }

    pub fn warn(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), true));
        self.notice_undo = None;
        crate::sound::play(&self.env, crate::sound::Cue::Error);
    }

    /// Go to a page by name: a feature id ("system_junk"), "history", "settings", or any
    /// [`AppRoute`] string ("health", "scan:system_junk", "guide:ai_agents", ...).
    pub fn navigate(&mut self, name: &str) {
        if let Some(r) = AppRoute::parse(name) {
            self.open_route(&r);
        }
    }

    /// Open a route: a page, a page that starts its scan, App Remover on one app. A route only
    /// opens and looks; nothing is ever cleaned from here.
    pub fn open_route(&mut self, r: &AppRoute) {
        if *r == AppRoute::BatterySettings {
            // Outside the app: the page stays.
            let url = cleancraft_core::route::BATTERY_SETTINGS_URL;
            if let Err(e) = self.env.sys.open_url(url) {
                self.warn(format!("Could not open System Settings: {e}"));
            }
            return;
        }
        self.flush_health_notes();
        // A notice belongs to the page it was about (REVIEW-U U14).
        self.notice = None;
        self.page = match r {
            AppRoute::Feature(f) | AppRoute::Scan(f) => Page::Feature(*f),
            AppRoute::Health => Page::Health,
            AppRoute::History => Page::History,
            AppRoute::Settings | AppRoute::FullDiskAccess => Page::Settings,
            AppRoute::ThreatReview(_) => Page::Feature(FeatureId::Malware),
            AppRoute::AgentGuide => Page::AgentGuide,
            AppRoute::ConnectAi => Page::ConnectAi,
            AppRoute::UnusedApps | AppRoute::UninstallApp(_) => {
                Page::Feature(FeatureId::Uninstaller)
            }
            AppRoute::BatterySettings => self.page,
            AppRoute::Task(_) => Page::Feature(FeatureId::Maintenance),
        };
        match r {
            AppRoute::History => self.history.reload(&self.env),
            AppRoute::Scan(f) => self.start_scan_if_idle(*f),
            AppRoute::UnusedApps => {
                self.apps.group = pages::apps::Group::Unused;
                self.apps.selected = None;
                if self.apps.list.is_none() && !self.apps.is_loading() {
                    pages::apps::start_load(self);
                }
            }
            AppRoute::UninstallApp(p) => {
                let real = self.route_path(p);
                pages::apps::open_app(self, &real);
            }
            AppRoute::ThreatReview(p) => {
                self.pending_threat = Some(self.route_path(p));
                self.start_scan_if_idle(FeatureId::Malware);
                self.apply_pending_threat();
            }
            AppRoute::FullDiskAccess => {
                // Read it again: it may have been granted since launch.
                self.fda = permissions::full_disk_access(&self.env);
                self.fda_dismissed = false;
                self.settings_page.focus_fda = true;
            }
            AppRoute::Task(id) => self.tasks.ask = Some(id.clone()),
            AppRoute::ConnectAi => pages::welcome::refresh(self),
            AppRoute::Health => pages::health::refresh_if_old(self),
            _ => {}
        }
    }

    /// A path from a route: the agent and the watchers send paths as they found them (already
    /// on this Mac, or inside the fake Mac of the tests); a typed route names a macOS path.
    fn route_path(&self, p: &std::path::Path) -> std::path::PathBuf {
        if std::fs::symlink_metadata(p).is_ok() {
            p.to_path_buf()
        } else {
            self.env.sys_path(&p.to_string_lossy())
        }
    }

    /// Select the finding a "Threat Check with this finding" route named, once Threat Check
    /// shows its results. Selecting only ticks the item for review; nothing is removed.
    fn apply_pending_threat(&mut self) {
        let Some(path) = self.pending_threat.clone() else {
            return;
        };
        if self.plan_state(FeatureId::Malware).is_busy() {
            return;
        }
        self.pending_threat = None;
        let st = self.plan_state(FeatureId::Malware);
        let Some(plan) = st.plan.as_mut() else {
            return;
        };
        let found = cleancraft_core::watch::preselect_threat(plan, &path);
        if let Some(i) = found {
            st.category = None;
            let it = &plan.items[i];
            let name = it
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let msg = match &it.locked {
                Some(why) => format!("{name} is listed but cannot be selected: {why}"),
                None => format!(
                    "{name} is selected for review. Read why it was found before you remove anything."
                ),
            };
            self.tell(msg);
        } else {
            let shown = self.env.display(&path);
            self.tell(format!(
                "Threat Check did not list {shown} this time: it may be gone or fixed already."
            ));
        }
    }

    /// Start a module's scan unless it is scanning or shows results the person may be
    /// reviewing (those stay; Scan Again is on the page).
    fn start_scan_if_idle(&mut self, f: FeatureId) {
        if f == FeatureId::SmartCare {
            pages::sweep::start_scan_if_idle(self);
            return;
        }
        // Pages with their own lists load them; plan pages scan (Quick Sweep, Tune-up,
        // Startup Items, Disk Map and Live Monitor keep their own start buttons).
        if matches!(f, FeatureId::Uninstaller | FeatureId::Updater) {
            if self.apps.list.is_none() && !self.apps.is_loading() {
                pages::apps::start_load(self);
            }
            return;
        }
        if f == FeatureId::SpaceLens {
            pages::disk_map::start_if_idle(self, cleancraft_core::space::MapLocation::Home);
            return;
        }
        let own_page = matches!(
            f,
            FeatureId::SmartCare
                | FeatureId::LoginItems
                | FeatureId::Maintenance
                | FeatureId::SpaceLens
                | FeatureId::Monitor
        );
        if !own_page && cleancraft_core::features::get(f).is_some() {
            let st = self.plan_state(f);
            if st.plan.is_none() && !st.is_busy() {
                pages::plan_page::start_scan(self, f);
            }
        }
    }

    pub fn plan_state(&mut self, f: FeatureId) -> &mut PlanState {
        self.plans.entry(f).or_default()
    }

    /// Files left their place (moved to the Trash or quarantine): every list that still shows
    /// them drops them at once, on every page. Plan pages and Quick Sweep's tiles lose the
    /// moved items (and anything inside them); an item that contained a moved file is
    /// measured again; a duplicate set left with one copy is no longer a set. Pages with their
    /// own lists (App Remover, Disk Map, History) follow [`CleanApp::disk_epoch`].
    pub fn files_moved(&mut self, moved: &[std::path::PathBuf]) {
        if moved.is_empty() {
            return;
        }
        for st in self.plans.values_mut() {
            if st.is_busy() {
                continue;
            }
            if let Some(plan) = st.plan.as_mut() {
                pages::plan_page::drop_paths(plan, moved);
            }
        }
        if let Some(res) = self.sweep.result.as_mut() {
            for tile in &mut res.tiles {
                if let cleancraft_core::sweep::TileData::Plan(plan) = &mut tile.data {
                    let had = !plan.items.is_empty();
                    pages::plan_page::drop_paths(plan, moved);
                    // Emptied by this cleanup: the tile says so (REVIEW-U N1).
                    if had && plan.items.is_empty() && !self.sweep.cleaned.contains(&tile.id) {
                        self.sweep.cleaned.push(tile.id);
                    }
                }
            }
        }
        // What moved may now sit in the Trash: the Trash tile and the Trash Bins list are
        // measured again (REVIEW-U N2).
        self.sweep.refresh_tile(FeatureId::TrashBins);
        if let Some(st) = self.plans.get_mut(&FeatureId::TrashBins)
            && st.plan.is_some()
            && !st.is_busy()
        {
            st.stale = true;
        }
        self.disk_epoch += 1;
        self.last_moved = moved.to_vec();
    }

    /// Apply what the action log says changed since the last look: items moved away by any
    /// page leave every list; items put back make the lists scan again.
    fn follow_journal(&mut self) {
        let env = self.env.clone();
        let Some(changed) = self.journal.changes(&env) else {
            if !self.journal_primed() {
                self.journal.prime(&env);
            }
            return;
        };
        let moved = pages::history::moved_paths(&changed);
        let gone: Vec<std::path::PathBuf> =
            moved.iter().filter(|m| !m.2).map(|m| m.0.clone()).collect();
        let back: Vec<std::path::PathBuf> =
            moved.iter().filter(|m| m.2).map(|m| m.0.clone()).collect();
        if !gone.is_empty() {
            self.files_moved(&gone);
        }
        if !back.is_empty() {
            self.files_back(&back);
        }
    }

    fn journal_primed(&self) -> bool {
        self.journal.is_primed()
    }

    /// Notes for Mac Health, from what the pages already did: each scan result (what it
    /// found, then what is left after cleanups), each finished Disk Map tree, the last update
    /// check. At the first frame of a real launch: a free space sample and, once, the AI apps
    /// question. Writes only CleanCraft's own small files; never runs a scan.
    fn observe(&mut self) {
        let env = self.env.clone();
        let mut plans: Vec<cleancraft_core::Plan> = Vec::new();
        for st in self.plans.values() {
            if let Some(p) = &st.plan
                && !st.is_busy()
            {
                plans.push(p.clone());
            }
        }
        if let Some(res) = &self.sweep.result {
            for tile in &res.tiles {
                if let Some(p) = tile.plan() {
                    plans.push(p.clone());
                }
            }
        }
        for p in plans {
            let Some(f) = p.feature else { continue };
            let key = (
                p.id.clone(),
                p.items.iter().map(|i| i.size).sum::<u64>(),
                p.items.len(),
            );
            if self.observed.plans.get(&f) != Some(&key) {
                self.observed.plans.insert(f, key);
                let _ = cleancraft_core::health::note_plan(&env, &p);
            }
        }
        if let Some(tree) = self.disk_map.tree()
            && let Some(root) = &tree.root
            && !tree.stopped
        {
            let key = (root.path.clone(), root.size, root.files);
            if self.observed.map.as_ref() != Some(&key) {
                self.observed.map = Some(key);
                self.observed.map_pending =
                    cleancraft_core::health::summarize(tree, chrono::Utc::now().timestamp());
            }
        }
        if self.page != Page::Feature(FeatureId::SpaceLens) {
            self.flush_health_notes();
        }
        let updates = self
            .apps
            .updates
            .as_ref()
            .map(|r| r.available().count() + r.macos.len());
        if updates.is_some() && updates != self.observed.updates {
            let _ = cleancraft_core::health::note_count(
                &env,
                FeatureId::Updater,
                updates.unwrap_or(0),
                chrono::Utc::now().timestamp(),
            );
        }
        self.observed.updates = updates;
        if !self.observed.launched && !self.sync {
            self.observed.launched = true;
            self.launch_checks();
        }
    }

    /// Write what Mac Health keeps that is still waiting (the last Disk Map summary).
    pub fn flush_health_notes(&mut self) {
        if let Some(s) = self.observed.map_pending.take() {
            let _ = cleancraft_core::health::keep_map_summary(&self.env, s);
        }
    }

    /// Work done once per launch of the real app (never in tests, which call it directly):
    /// a free space sample and, at most once a day, a light measure of the home folder for
    /// Mac Health's "What grew" (U7-1), on a background thread; the first-launch steps once
    /// ([`Settings::setup_done`]).
    pub fn launch_checks(&mut self) {
        let env = self.env.clone();
        let work = move || {
            let _ = cleancraft_core::space_history::record(&env);
            let _ = cleancraft_core::health::record_baseline(&env, chrono::Utc::now().timestamp());
        };
        if self.sync {
            work();
        } else {
            std::thread::spawn(work);
            // Mac Health's state for the line on Quick Sweep's start screen (U7-4).
            pages::health::refresh(self);
        }
        // A new CleanCraft on GitHub (at most once a day, shared with the menu bar agent).
        pages::updates::launch_check(self);
        if !self.settings.setup_done {
            // Welcome, then Full Disk Access, then AI apps (when one is found) (U7-5). Seen
            // as soon as it shows: from the second launch on the app opens on Quick Sweep,
            // whatever happened here (Done, Not Now, Skip or quitting midway).
            self.settings.setup_done = true;
            self.save_settings();
            pages::welcome::start(self);
        }
    }

    /// Standard Mac shortcuts (REVIEW-U U10): Cmd+, opens Settings, Cmd+1 to Cmd+7 the
    /// sidebar sections. (Cmd+F is the page's own: it focuses its search field.) None while
    /// a dialog is open.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.top_modal_layer()).is_some() {
            return;
        }
        let cmd = egui::Modifiers::COMMAND;
        if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::Comma)) {
            self.act();
            self.navigate("settings");
        }
        const KEYS: [egui::Key; 7] = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
        ];
        for (i, k) in KEYS.iter().enumerate() {
            if i < GROUPS.len() && ctx.input_mut(|inp| inp.consume_key(cmd, *k)) {
                self.act();
                self.page = self
                    .section_tool
                    .get(&i)
                    .copied()
                    .unwrap_or(Page::Feature(GROUPS[i].1[0]));
            }
        }
    }

    /// The original paths a cleanup report moved.
    pub fn moved_paths(rep: &cleancraft_core::executor::Report) -> Vec<std::path::PathBuf> {
        rep.moved
            .iter()
            .filter_map(|r| match &r.kind {
                cleancraft_core::executor::ActionKind::Moved { original, .. } => {
                    Some(original.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Files came back (an Undo): lists made before are out of date. Plan pages with results
    /// and Quick Sweep's results scan again the next time they are shown.
    pub fn files_back(&mut self, back: &[std::path::PathBuf]) {
        let _ = back;
        for st in self.plans.values_mut() {
            if st.plan.is_some() && !st.is_busy() {
                st.stale = true;
            }
        }
        if self.sweep.result.is_some() {
            self.sweep.stale = true;
        }
        self.disk_epoch += 1;
        self.last_moved.clear();
    }

    pub fn save_settings(&mut self) {
        if let Err(e) = self.settings.save(&self.env) {
            self.warn(format!("Settings could not be saved: {e}"));
        } else {
            crate::sound::apply(&self.env, &self.settings);
        }
    }

    /// Per-frame work that does not draw: finished jobs.
    pub fn logic(&mut self, ctx: &egui::Context) {
        self.now = ctx.input(|i| i.time);
        self.window_hidden = crate::motion::window_hidden(ctx);
        if self.egui_ctx.is_none() {
            self.egui_ctx = Some(ctx.clone());
        }
        if !self.fonts_installed {
            // macOS "Reduce motion" (Accessibility > Display), read once through env.sys.
            // Tests run with `sync` and keep motion on without asking.
            if !self.sync {
                crate::motion::detect_reduce_motion(self.env.sys.as_ref());
            }
            theme::install_fonts(ctx);
            theme::apply(ctx, &self.t);
            self.fonts_installed = true;
            self.fonts_pass = ctx.cumulative_pass_nr();
            ctx.request_repaint();
        }
        crate::images::ensure(ctx, &self.env, self.sync);
        let mut busy = false;
        let ids: Vec<FeatureId> = self.plans.keys().copied().collect();
        for f in ids {
            busy |= pages::plan_page::poll(self, f);
        }
        if self.pending_threat.is_some() {
            self.apply_pending_threat();
        }
        busy |= pages::sweep::poll(self);
        busy |= pages::apps::poll(self);
        busy |= pages::startup::poll(self);
        busy |= pages::tasks::poll(self);
        busy |= pages::disk_map::poll(self);
        busy |= pages::monitor::poll(self);
        busy |= pages::health::poll(self);
        busy |= pages::welcome::poll(self);
        busy |= pages::updates::poll(self);
        self.follow_journal();
        // "Open at this page" requests from the menu bar agent and `cleancraft open` (tray6).
        pages::settings::follow_open_requests(self, ctx);
        self.observe();
        if busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// The section of the page on screen.
    pub fn section(&self) -> Section {
        match self.page {
            Page::Feature(f) => Section::of(f),
            Page::Health => Section::of(FeatureId::Monitor),
            Page::History | Page::Settings | Page::AgentGuide | Page::ConnectAi | Page::Welcome => {
                Section::Neutral
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        self.shortcuts(ui.ctx());
        // A switch or checkbox toggled last frame: its click sound (when turned on).
        if kit::take_tap(ui.ctx()) {
            crate::sound::play(&self.env, crate::sound::Cue::Tap);
        }
        self.t = self.t.for_section(self.section());
        let t = self.t;
        kit::set_tokens(ui.ctx(), &t);
        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0.0, t.bg);
        // Inter is registered in logic() but only usable from the next pass.
        if !self.fonts_installed || ui.ctx().cumulative_pass_nr() <= self.fonts_pass {
            ui.ctx().request_repaint();
            return;
        }
        if let Some(g) = page_group(self.page) {
            self.section_tool.insert(g, self.page);
        }
        let mut content = full;
        content.min.x += space::SIDEBAR;
        kit::backdrop(ui.painter(), content, &t);
        egui::Panel::left("sidebar")
            .show_separator_line(false)
            .resizable(false)
            .exact_size(space::SIDEBAR)
            .frame(
                egui::Frame::new()
                    .fill(t.sidebar)
                    // No top margin: the sidebar's first 28 pt are the title band (traffic
                    // lights and the logo), see sidebar().
                    .inner_margin(egui::Margin {
                        left: space::SIDEBAR_X,
                        right: space::SIDEBAR_X,
                        top: 0,
                        bottom: 8,
                    }),
            )
            .show(ui, |ui| self.sidebar(ui));
        // Page change: fade in with a short slide up.
        let now = ui.input(|i| i.time);
        if self.shown_page != self.page {
            self.shown_page = self.page;
            self.page_since = now;
        }
        let k = motion::ease_out(motion::progress(now, self.page_since, motion::PAGE));
        if k < 1.0 {
            ui.ctx().request_repaint();
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(space::PAGE_X, space::PAGE_Y)),
            )
            .show(ui, |ui| {
                // A section with several tools: its switcher, the same in every phase (it
                // stands in for the compact page header).
                let tools = page_group(self.page).map(section_pages).unwrap_or_default();
                let multi = tools.len() > 1;
                kit::set_switcher_shown(ui.ctx(), multi);
                if multi {
                    let items: Vec<(Icon, &str)> =
                        tools.iter().map(|x| page_look(&t, *x)).collect();
                    let sel = tools.iter().position(|x| *x == self.page).unwrap_or(0);
                    if let Some(i) = kit::segmented(ui, &t, &items, sel) {
                        self.act();
                        if tools[i] != self.page {
                            self.notice = None;
                            self.page = tools[i];
                            if self.page == Page::Health {
                                pages::health::refresh_if_old(self);
                            }
                        }
                    }
                    ui.add_space(space::L);
                }
                self.banners(ui);
                ui.multiply_opacity(0.25 + 0.75 * k);
                let shift = egui::emath::TSTransform::from_translation(vec2(
                    0.0,
                    (1.0 - k) * motion::PAGE_SLIDE,
                ));
                ui.with_visual_transform(shift, |ui| match self.page {
                    Page::Feature(f) => pages::feature_page(self, ui, f),
                    Page::Health => pages::health::ui(self, ui),
                    Page::History => pages::history::ui(self, ui),
                    Page::Settings => pages::settings::ui(self, ui),
                    Page::AgentGuide => pages::guide::ui(self, ui),
                    Page::ConnectAi => pages::welcome::ui(self, ui),
                    Page::Welcome => pages::welcome::intro_ui(self, ui),
                });
            });
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let t = self.t;
        let side = ui
            .max_rect()
            .expand2(vec2(space::SIDEBAR_X as f32, space::SIDEBAR_Y as f32));
        icons::vertical_gradient(
            ui.painter(),
            side,
            Color32::from_rgb(0x0C, 0x16, 0x29),
            t.sidebar,
        );
        ui.painter().vline(
            side.right() - 0.5,
            side.y_range(),
            Stroke::new(1.0, Color32::from_white_alpha(10)),
        );
        // The traffic lights' band (titlebar.rs), then the logo under it.
        ui.add_space(crate::titlebar::SIDEBAR_TOP - 8.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            crate::logo::show(ui, crate::logo::SIDEBAR_PT);
            ui.add_space(6.0);
            // The wordmark: "Clean" in text white, "Craft" in the wordmark lime (BRAND.md).
            let mut job = egui::text::LayoutJob::default();
            for (part, color) in [("Clean", t.text), ("Craft", theme::WORDMARK_LIME)] {
                job.append(
                    part,
                    0.0,
                    egui::TextFormat {
                        font_id: font(size::TITLE2, Weight::Bold),
                        color,
                        extra_letter_spacing: -0.3,
                        ..Default::default()
                    },
                );
            }
            ui.label(job);
        });
        ui.add_space(space::L);
        let current = page_group(self.page);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = space::XS;
                for (i, (name, feats)) in GROUPS.iter().enumerate() {
                    if self.nav_row(ui, group_icon(i), name, current == Some(i)) {
                        self.act();
                        self.notice = None;
                        self.page = self
                            .section_tool
                            .get(&i)
                            .copied()
                            .unwrap_or(Page::Feature(feats[0]));
                    }
                }
                ui.add_space(space::S);
                let y = ui.cursor().min.y;
                ui.painter().hline(
                    ui.max_rect().x_range().shrink(6.0),
                    y,
                    Stroke::new(1.0, Color32::from_white_alpha(14)),
                );
                ui.add_space(space::S);
                if self.nav_row(
                    ui,
                    SectionIcon::History,
                    "History",
                    self.page == Page::History,
                ) {
                    self.act();
                    self.navigate("history");
                }
                if self.nav_row(
                    ui,
                    SectionIcon::Settings,
                    "Settings",
                    self.page == Page::Settings,
                ) {
                    self.act();
                    self.navigate("settings");
                }
            });
    }

    /// One sidebar entry: a 28 pt icon tile in the section's colour and a 15 pt label in a
    /// 44 pt row; the selected entry sits on a soft pill.
    fn nav_row(&self, ui: &mut Ui, icon: SectionIcon, label: &str, active: bool) -> bool {
        let t = self.t;
        let color = icon.section().colors().0;
        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), NAV_ROW), Sense::click());
        resp.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::Button, true, active, crate::tr(label))
        });
        let feel = kit::feel_ring(ui, &t, &resp, r, f32::from(theme::radius::M));
        let hov = feel.hov;
        let on = kit::anim(ui.ctx(), resp.id.with("on"), active, motion::PAGE);
        let p = ui.painter();
        let round = CornerRadius::same(theme::radius::M);
        if on > 0.0 {
            p.rect_filled(
                r,
                round,
                icons::lerp(
                    Color32::from_white_alpha(0),
                    icons::lerp(Color32::from_white_alpha(14), color, 0.14),
                    on,
                ),
            );
            p.rect_stroke(
                r,
                round,
                Stroke::new(1.0, Color32::from_white_alpha((16.0 * on) as u8)),
                egui::StrokeKind::Inside,
            );
        }
        if (hov > 0.0 || feel.down > 0.0) && !active {
            p.rect_filled(
                r,
                round,
                Color32::from_white_alpha((9.0 * hov + 8.0 * feel.down) as u8),
            );
        }
        let tile = egui::Rect::from_center_size(
            r.left_center() + vec2(8.0 + 14.0, 0.0),
            egui::Vec2::splat(28.0),
        );
        // Rest, half lit on hover, fully lit when selected (section_icons.rs).
        section_icons::paint(ui, tile, icon, on.max(0.5 * hov));
        p.text(
            egui::pos2(tile.right() + 12.0, r.center().y),
            egui::Align2::LEFT_CENTER,
            crate::tr(label),
            // One weight in both states, so the label does not jump when selected.
            font(size::BODY, Weight::Medium),
            if active {
                t.text
            } else {
                icons::lerp(t.text_dim, t.text, hov)
            },
        );
        resp.clicked()
    }

    /// Pages whose results depend on Full Disk Access (Mail, Safari and Trash data).
    fn needs_fda(&self) -> bool {
        matches!(
            self.page,
            Page::Feature(
                FeatureId::SmartCare
                    | FeatureId::SystemJunk
                    | FeatureId::MailAttachments
                    | FeatureId::TrashBins
                    | FeatureId::Privacy
                    | FeatureId::Malware
            )
        )
    }

    fn banners(&mut self, ui: &mut Ui) {
        let t = self.t;
        pages::updates::banner(self, ui);
        // Only where it matters, and it can be put away (REVIEW-U U14).
        if self.fda == Access::Missing && !self.fda_dismissed && self.needs_fda() {
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(0x3A, 0x2C, 0x12, 200))
                .stroke(Stroke::new(1.0, t.warning.gamma_multiply(0.45)))
                .corner_radius(CornerRadius::same(theme::radius::M))
                .inner_margin(egui::Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        let (r, _) = ui.allocate_exact_size(egui::Vec2::splat(isz::SIDEBAR), Sense::hover());
                        icons::paint(ui.painter(), r, Icon::Warning, t.warning);
                        ui.label(
                            crate::rich(
                                "CleanCraft cannot see Mail, Safari and Trash data. Turn on Full Disk Access for CleanCraft to include them.",
                            )
                            .font(font(size::CALLOUT, Weight::Regular))
                            .color(t.text),
                        );
                        if kit::secondary(ui, &t, "Open Privacy Settings") {
                            self.act();
                            if let Err(e) = permissions::open_fda_settings(&self.env) {
                                self.warn(format!("Could not open System Settings: {e}"));
                            }
                        }
                        if kit::secondary(ui, &t, "Check Again") {
                            self.act();
                            self.fda = permissions::full_disk_access(&self.env);
                            if self.fda != Access::Missing {
                                self.tell("Full Disk Access is on.");
                            } else {
                                self.warn("Full Disk Access is still off.");
                            }
                        }
                        if kit::ghost(ui, &t, "Not Now", None).clicked() {
                            self.act();
                            self.fda_dismissed = true;
                        }
                    });
                });
            ui.add_space(8.0);
        }
        if self.notice.is_none() {
            self.notice_undo = None;
        }
        if let Some((msg, err)) = self.notice.clone() {
            let color = if err { t.danger } else { t.success };
            egui::Frame::new()
                .fill(icons::lerp(t.card, color, 0.14))
                .stroke(Stroke::new(1.0, color.gamma_multiply(0.4)))
                .corner_radius(CornerRadius::same(theme::radius::M))
                .inner_margin(egui::Margin::symmetric(14, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (r, _) =
                            ui.allocate_exact_size(egui::Vec2::splat(isz::SIDEBAR), Sense::hover());
                        icons::paint(
                            ui.painter(),
                            r,
                            if err { Icon::Warning } else { Icon::Check },
                            color,
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if kit::ghost(ui, &t, "Dismiss", None).clicked() {
                                self.act();
                                self.notice = None;
                                self.notice_undo = None;
                            }
                            if let Some(name) = self.notice_undo.as_ref().map(|u| u.0.clone()) {
                                let r = kit::pill(
                                    ui,
                                    &t,
                                    "Undo",
                                    kit::Kind::Secondary,
                                    Some(Icon::Undo),
                                );
                                r.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::Button,
                                        true,
                                        crate::tr(&name),
                                    )
                                });
                                if r.clicked() {
                                    self.act();
                                    self.notice = None;
                                    if let Some(NoticeUndo(_, undo)) = self.notice_undo.take() {
                                        undo(self);
                                    }
                                }
                            }
                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            crate::rich(&msg)
                                                .font(font(size::CALLOUT, Weight::Regular))
                                                .color(t.text),
                                        )
                                        .wrap(),
                                    );
                                },
                            );
                        });
                    });
                });
            ui.add_space(8.0);
        }
    }
}

impl Drop for CleanApp {
    /// A Disk Map summary not yet written is kept when the app quits.
    fn drop(&mut self) {
        self.flush_health_notes();
    }
}
