//! The page every cleanup module shares: Scan, review the exact list, confirm, move, undo.
//!
//! Flow: Idle -> Scanning -> Results (categories, items, search, sort, select) -> Confirm (a
//! dialog with every selected file, its size and reason) -> Cleaning -> Done (freed, skipped
//! with reasons, Undo).

use cleancraft_core::executor::{self, Report};
use cleancraft_core::features;
use cleancraft_core::features::duplicates;
use cleancraft_core::fsutil::human_size;
use cleancraft_core::{FeatureId, Plan};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use egui::{Align, Color32, Layout, Rect, Sense, Ui, pos2, vec2};

use crate::app::CleanApp;
use crate::icons::{self, Icon};
use crate::jobs::{Job, Progress};
use crate::kit;
use crate::kit_list::{self, Filter, SortKey};
use crate::theme::{Weight, font, hit, icon as isz, size, space};
use cleancraft_core::settings::ListPrefs;

#[derive(Default)]
pub enum Phase {
    #[default]
    Idle,
    Scanning(Job<Result<Plan, String>>),
    Results,
    Confirm,
    Cleaning(Job<Result<Report, String>>),
    Done(Report),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Size,
    Name,
    /// Oldest first (Large & Old Files: find what has not been touched for longest).
    Date,
}

#[derive(Default)]
pub struct PlanState {
    pub phase: Phase,
    pub plan: Option<Plan>,
    /// The category shown on the right; None = all.
    pub category: Option<String>,
    pub search: String,
    pub sort: Sort,
    pub error: Option<String>,
    /// Set to stop the running scan.
    pub cancel: Arc<AtomicBool>,
    /// Files came back since this list was made (an Undo): scan again when shown.
    pub stale: bool,
    /// The list's filters and sort (a copy of `Settings::lists["plan.<module>"]`, read each
    /// frame).
    pub prefs: ListPrefs,
    /// The home folder, for the Location filter.
    pub home: std::path::PathBuf,
    /// Each item's kind and location (by item id), worked out once per list.
    facets: Facets,
}

/// Each item's (kind, location), for the plan with this id and item count.
#[derive(Default)]
struct Facets {
    of: (String, usize),
    items: Vec<(&'static str, String)>,
}

impl PlanState {
    /// Work out each item's kind and location when the list changed.
    pub fn refresh_facets(&mut self) {
        let Some(plan) = &self.plan else {
            return;
        };
        let of = (plan.id.clone(), plan.items.len());
        if self.facets.of == of && self.facets.items.len() == plan.items.len() {
            return;
        }
        let file = by_file(plan.feature);
        self.facets.items = plan
            .items
            .iter()
            .map(|i| {
                if file {
                    (
                        features::large_old::item_kind(&i.path),
                        location(&self.home, &i.path),
                    )
                } else {
                    ("", String::new())
                }
            })
            .collect();
        self.facets.of = of;
    }

    fn facet(&self, it: &cleancraft_core::plan::Item) -> (&'static str, String) {
        match self.facets.items.get(it.id) {
            Some(f)
                if self.facets.items.len() == self.plan.as_ref().map_or(0, |p| p.items.len()) =>
            {
                f.clone()
            }
            _ => (
                features::large_old::item_kind(&it.path),
                location(&self.home, &it.path),
            ),
        }
    }
}

/// A results list's name in the settings: "plan.system_junk".
pub fn list_id(f: FeatureId) -> String {
    format!("plan.{}", f.as_str())
}

/// Modules whose items are your own files: these filter by kind and by location.
fn by_file(f: Option<FeatureId>) -> bool {
    matches!(
        f,
        Some(FeatureId::LargeOldFiles | FeatureId::Duplicates | FeatureId::SimilarPhotos)
    )
}

/// The folder an item is in, for the Location filter: the folder of your home it is in
/// ("Downloads"), the disk for an external one, else "Elsewhere".
pub fn location(home: &std::path::Path, p: &std::path::Path) -> String {
    if let Ok(rel) = p.strip_prefix(home) {
        let mut c = rel.components();
        return match (c.next(), c.next()) {
            (Some(first), Some(_)) => first.as_os_str().to_string_lossy().into_owned(),
            _ => "Home".to_string(),
        };
    }
    let parts: Vec<String> = p
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if let Some(k) = parts.iter().position(|c| c == "Volumes")
        && let Some(name) = parts.get(k + 1)
    {
        return name.clone();
    }
    "Elsewhere".to_string()
}

/// Does item `it` match the filter `key`?
fn item_has(st: &PlanState, it: &cleancraft_core::plan::Item, key: &str) -> bool {
    match key {
        "selected" => it.selected,
        "unselected" => !it.selected && it.locked.is_none(),
        "locked" => it.locked.is_some(),
        k => {
            if let Some(kind) = k.strip_prefix("kind:") {
                st.facet(it).0 == kind
            } else if let Some(loc) = k.strip_prefix("loc:") {
                st.facet(it).1 == loc
            } else {
                false
            }
        }
    }
}

/// A results list's filters with how many items each matches: selection state, and for
/// your own files their kind and location.
pub fn plan_filters(st: &PlanState) -> Vec<Filter> {
    let Some(plan) = &st.plan else {
        return Vec::new();
    };
    let n = |k: &str| plan.items.iter().filter(|i| item_has(st, i, k)).count();
    let mut v = Vec::new();
    if plan.items.iter().any(|i| i.locked.is_none()) {
        v.push(Filter::any("Show", "selected", "Selected", n("selected")));
        v.push(Filter::any(
            "Show",
            "unselected",
            "Not selected",
            n("unselected"),
        ));
    }
    // Locked items are worth a filter only beside items that can be ticked.
    if !v.is_empty() && plan.items.iter().any(|i| i.locked.is_some()) {
        v.push(Filter::any("Show", "locked", "Locked", n("locked")));
    }
    if by_file(plan.feature) {
        let facets: Vec<(&'static str, String)> = plan.items.iter().map(|i| st.facet(i)).collect();
        let mut kinds: Vec<&'static str> = facets.iter().map(|f| f.0).collect();
        kinds.sort_by_key(|k| features::large_old::kind_title(k));
        kinds.dedup();
        for k in kinds {
            let c = facets.iter().filter(|f| f.0 == k).count();
            v.push(Filter::any(
                "Kind",
                format!("kind:{k}"),
                features::large_old::kind_title(k),
                c,
            ));
        }
        let mut locs: Vec<&String> = facets.iter().map(|f| &f.1).collect();
        locs.sort();
        locs.dedup();
        for l in locs {
            let c = facets.iter().filter(|f| &f.1 == l).count();
            // Home folders read as paths ("~/Movies"), so they never look like a kind.
            let label = match l.as_str() {
                "Home" => "~ (home folder)".to_string(),
                "Elsewhere" => l.clone(),
                _ if plan
                    .items
                    .iter()
                    .any(|i| i.path.starts_with(st.home.join(l))) =>
                {
                    format!("~/{l}")
                }
                _ => l.clone(),
            };
            v.push(Filter::any("Location", format!("loc:{l}"), label, c));
        }
    }
    v
}

/// A results list's sort orders: size and name, and a date where the items have one.
pub fn plan_sorts(plan: &Plan) -> Vec<SortKey> {
    let mut v = vec![kit_list::BY_SIZE, kit_list::BY_NAME];
    if plan.items.iter().any(|i| i.modified.is_some()) {
        v.push(if plan.feature == Some(FeatureId::LargeOldFiles) {
            SortKey {
                key: "date",
                label: "Sort by last opened",
                first: "Longest unopened first",
                second: "Recently opened first",
                descending: false,
            }
        } else {
            kit_list::BY_DATE
        });
    }
    v
}

/// Does item `it` pass the search and the filters (the category column aside)?
fn passes(st: &PlanState, filters: &[Filter], q: &str, it: &cleancraft_core::plan::Item) -> bool {
    kit_list::matches(q, &[&it.path.to_string_lossy(), &it.reason])
        && kit_list::passes(&st.prefs, filters, |k| item_has(st, it, k))
}

/// Selected items the search or the filters do not show (they still move: the bottom bar
/// counts them and the confirmation lists them).
pub fn hidden_selected(st: &PlanState) -> usize {
    let Some(plan) = &st.plan else {
        return 0;
    };
    let filters = plan_filters(st);
    let q = kit_list::query(&st.search);
    plan.items
        .iter()
        .filter(|i| i.selected && !passes(st, &filters, &q, i))
        .count()
}

impl PlanState {
    pub fn is_busy(&self) -> bool {
        matches!(self.phase, Phase::Scanning(_) | Phase::Cleaning(_))
    }
}

/// Drop from `plan` every item at or inside a path in `gone`; an item that contained one is
/// measured again. Item ids are renumbered (they are indexes). In a plan of sets
/// (duplicates, look-alike pictures) a set left with one file is dropped too: one copy is not
/// a duplicate. Returns true when anything changed.
pub fn drop_paths(plan: &mut Plan, gone: &[std::path::PathBuf]) -> bool {
    let before = plan.items.len();
    let mut resized = false;
    plan.items
        .retain(|it| !gone.iter().any(|g| it.path.starts_with(g)));
    for it in &mut plan.items {
        if gone.iter().any(|g| g.starts_with(&it.path)) {
            it.size = cleancraft_core::fsutil::tree_size(&it.path).0;
            resized = true;
        }
    }
    let sets = matches!(
        plan.feature,
        Some(FeatureId::Duplicates | FeatureId::SimilarPhotos)
    );
    if sets {
        let mut count: std::collections::HashMap<String, usize> = Default::default();
        for g in plan.items.iter().filter_map(|i| i.group.clone()) {
            *count.entry(g).or_default() += 1;
        }
        plan.items.retain(|i| {
            i.group
                .as_ref()
                .is_none_or(|g| count.get(g).copied().unwrap_or(0) > 1)
        });
    }
    for (i, it) in plan.items.iter_mut().enumerate() {
        it.id = i;
    }
    plan.items.len() != before || resized
}

/// Short text about each module, shown before the first scan.
pub fn blurb(f: FeatureId) -> &'static str {
    match f {
        FeatureId::SystemJunk => {
            "Caches, logs and leftovers that macOS and your apps rebuild on their own."
        }
        FeatureId::MailAttachments => {
            "Copies of attachments Mail keeps on disk. The originals stay on your mail server."
        }
        FeatureId::TrashBins => "What is waiting in the Trash of every disk.",
        FeatureId::LargeOldFiles => {
            "Big files and files you have not opened for a long time. Nothing is selected for you."
        }
        FeatureId::Duplicates => "Files with exactly the same content in more than one place.",
        FeatureId::SimilarPhotos => "Pictures that look alike: bursts, edits, resized copies.",
        FeatureId::Malware => {
            "Startup programs and apps that are unsigned, tampered with or known to be unwanted."
        }
        FeatureId::Privacy => "Browser history, cookies and recent-item lists.",
        // dev6's module, named by its id so this builds before it is merged.
        _ if f.as_str() == "dev_clutter" => {
            "Build folders, packages and environments of projects you have not touched for a while."
        }
        _ => "",
    }
}

/// Scans that look at `ScanCtx::cancel` while they work: these get a Stop button.
pub fn can_stop(f: FeatureId) -> bool {
    matches!(
        f,
        FeatureId::SystemJunk
            | FeatureId::MailAttachments
            | FeatureId::LargeOldFiles
            | FeatureId::Duplicates
            | FeatureId::SimilarPhotos
            | FeatureId::Privacy
            | FeatureId::Malware
    ) || f.as_str() == "dev_clutter"
}

pub fn start_scan(app: &mut CleanApp, f: FeatureId) {
    let env = app.env.clone();
    let settings = app.settings.clone();
    let sync = app.sync;
    let flag = Arc::new(AtomicBool::new(false));
    app.plan_state(f).cancel = flag.clone();
    let job = Job::start_metered(sync, move |meter| {
        let cancel = flag;
        let depth = features::large_old::ScanOptions::load(&env).threat_depth;
        let Some(feat) = features::get_at(f, depth) else {
            return Err(format!("{} has no scan yet", f.title()));
        };
        let ctx = features::ScanCtx {
            env: &env,
            settings: &settings,
            cancel: &cancel,
            progress: &|s| meter.set_step(s),
            meter,
        };
        features::scan_with(feat.as_ref(), &ctx).map_err(|e| format!("{e:#}"))
    });
    let st = app.plan_state(f);
    st.phase = Phase::Scanning(job);
    st.error = None;
    crate::sound::play(&app.env, crate::sound::Cue::ScanStart);
}

fn start_clean(app: &mut CleanApp, f: FeatureId) {
    let Some(plan) = app.plan_state(f).plan.clone() else {
        return;
    };
    let env = app.env.clone();
    let settings = app.settings.clone();
    let digest = plan.digest();
    let job = Job::start_metered(app.sync, move |meter| {
        features::run_plan(&env, &settings, &plan, &digest, &mut |n, total| {
            meter.set(n as u64, total as u64);
            meter.set_step(&kit::moving_text(n, total));
        })
        .map_err(|e| format!("{e:#}"))
    });
    app.plan_state(f).phase = Phase::Cleaning(job);
}

/// Finish jobs. Returns true while something is running.
pub fn poll(app: &mut CleanApp, f: FeatureId) -> bool {
    let mut cue = None;
    let mut moved = Vec::new();
    let st = app.plan_state(f);
    let busy = match &mut st.phase {
        Phase::Scanning(job) => {
            if let Some(r) = job.poll() {
                match r {
                    Ok(plan) => {
                        cue = Some(crate::sound::Cue::ScanDone);
                        st.plan = Some(plan);
                        st.category = None;
                        st.phase = Phase::Results;
                    }
                    Err(e) => {
                        cue = Some(crate::sound::Cue::Error);
                        st.error = Some(e);
                        st.phase = Phase::Idle;
                    }
                }
                false
            } else if job.is_dead() {
                st.error = Some("The scan stopped unexpectedly.".into());
                st.phase = Phase::Idle;
                false
            } else {
                true
            }
        }
        Phase::Cleaning(job) => {
            if let Some(r) = job.poll() {
                match r {
                    Ok(rep) => {
                        // A cleanup that moved nothing because every item failed is an
                        // error to the ear; anything moved is a finished cleanup.
                        cue = Some(if rep.moved.is_empty() && !rep.skipped.is_empty() {
                            crate::sound::Cue::Error
                        } else {
                            crate::sound::Cue::CleanDone
                        });
                        moved = CleanApp::moved_paths(&rep);
                        st.phase = Phase::Done(rep);
                    }
                    Err(e) => {
                        cue = Some(crate::sound::Cue::Error);
                        st.error = Some(e);
                        st.phase = Phase::Results;
                    }
                }
                false
            } else if job.is_dead() {
                st.error = Some(
                    "The cleanup stopped unexpectedly. History shows what was moved before it stopped."
                        .into(),
                );
                st.phase = Phase::Results;
                false
            } else {
                true
            }
        }
        _ => false,
    };
    // What moved leaves this list and every other list that showed it.
    app.files_moved(&moved);
    if let Some(c) = cue {
        crate::sound::play(&app.env, c);
    }
    busy
}

/// Stop the running scan: the scan is told to stop and its result is dropped.
pub fn stop_scan(app: &mut CleanApp, f: FeatureId) {
    let st = app.plan_state(f);
    st.cancel.store(true, Ordering::Relaxed);
    st.phase = Phase::Idle;
    app.tell(format!("{} scan stopped. Nothing changed.", f.title()));
}

pub fn ui(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    // Files came back since this list was made: the list is out of date.
    let st = app.plan_state(f);
    if st.stale && !st.is_busy() {
        st.stale = false;
        match st.phase {
            Phase::Results | Phase::Confirm => start_scan(app, f),
            Phase::Idle => st.plan = None,
            _ => {}
        }
    }
    let t = app.t;
    let (icon, _) = kit::feature_look(&t, f);
    let phase = match &app.plan_state(f).phase {
        Phase::Idle => 0,
        Phase::Scanning(_) => 1,
        Phase::Results => 2,
        Phase::Confirm => 3,
        Phase::Cleaning(_) => 4,
        Phase::Done(_) => 5,
    };
    // The start screen has its own large title; every other phase has the compact header.
    if phase != 0 {
        kit::header(ui, &t, icon, f.title(), blurb(f));
    }
    if let Some(e) = app.plan_state(f).error.clone() {
        let (text, needs_access) = friendly_error(&e);
        if needs_access {
            if kit::banner(
                ui,
                &t,
                Icon::Warning,
                t.warning,
                &text,
                "Open Privacy Settings",
            ) {
                app.act();
                if let Err(e) = cleancraft_core::permissions::open_fda_settings(&app.env) {
                    app.warn(format!("Could not open System Settings: {e}"));
                }
            }
        } else {
            kit::strip(ui, Icon::Warning, t.warning, &text);
        }
        ui.add_space(space::S);
    }
    match phase {
        0 => idle(app, ui, f),
        1 => {
            let (status, meter) = match &app.plan_state(f).phase {
                Phase::Scanning(j) => (j.status(), j.meter.clone()),
                _ => (String::new(), Progress::new()),
            };
            let place = if status.is_empty() {
                "Getting ready...".to_string()
            } else {
                format!("Looking in {status}")
            };
            if kit::progress(
                ui,
                &t,
                f,
                &format!("Scanning {}", f.title()),
                &place,
                &meter,
                can_stop(f),
            ) {
                app.act();
                stop_scan(app, f);
            }
        }
        2 => results(app, ui, f),
        3 => {
            results(app, ui, f);
            confirm(app, ui, f);
        }
        4 => {
            let (status, meter) = match &app.plan_state(f).phase {
                Phase::Cleaning(j) => (j.status(), j.meter.clone()),
                _ => (String::new(), Progress::new()),
            };
            kit::progress(
                ui,
                &t,
                f,
                "Moving the selected items...",
                &status,
                &meter,
                false,
            );
        }
        _ => done(app, ui, f),
    }
}

fn idle(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    let t = app.t;

    let go = kit::hero(
        ui,
        &t,
        f,
        kit::HeroText {
            title: f.title(),
            desc: blurb(f),
            covers: super::covers(f),
            action: "Scan",
            note: "Scanning only looks. Nothing is removed until you review and confirm the list.",
            enabled: true,
        },
        |ui| super::extras::before_scan(app, ui, f),
    );
    if go {
        app.act();
        start_scan(app, f);
    }
}

fn visible_items(st: &PlanState) -> Vec<usize> {
    let Some(plan) = &st.plan else {
        return Vec::new();
    };
    let q = kit_list::query(&st.search);
    let filters = plan_filters(st);
    let mut v: Vec<usize> = plan
        .items
        .iter()
        .filter(|i| st.category.as_ref().is_none_or(|c| &i.category == c))
        .filter(|i| passes(st, &filters, &q, i))
        .map(|i| i.id)
        .collect();
    let (key, rev) = kit_list::sort_of(&st.prefs, &plan_sorts(plan));
    let name = |i: usize| {
        plan.items[i]
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    v.sort_by(|a, b| {
        let (x, y) = (&plan.items[*a], &plan.items[*b]);
        match key.key {
            // Oldest first; unknown dates last either way.
            "date" => match (x.modified, y.modified) {
                (Some(p), Some(q)) => {
                    let o = p.cmp(&q);
                    if rev { o.reverse() } else { o }
                }
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            },
            "name" => {
                let o = name(*a).cmp(&name(*b));
                if rev { o.reverse() } else { o }
            }
            _ => {
                let o = y.size.cmp(&x.size);
                if rev { o.reverse() } else { o }
            }
        }
    });
    v
}

/// Store a results list's filters and sort (and save the settings).
fn set_prefs(app: &mut CleanApp, f: FeatureId, prefs: ListPrefs) {
    if kit_list::store(&mut app.settings, &list_id(f), prefs.clone()) {
        app.save_settings();
    }
    let st = app.plan_state(f);
    st.sort = match st
        .plan
        .as_ref()
        .map(|p| kit_list::sort_of(&prefs, &plan_sorts(p)).0.key)
    {
        Some("name") => Sort::Name,
        Some("date") => Sort::Date,
        _ => Sort::Size,
    };
    st.prefs = prefs;
}

/// Height of a line in the item list.
const ROW_H: f32 = hit::ROW;

fn results(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    let t = app.t;
    let Some(plan) = app.plan_state(f).plan.clone() else {
        return;
    };
    // The remembered filters and sort (Settings is the one place they live).
    let prefs = kit_list::prefs(&app.settings, &list_id(f));
    let home = app.env.home().to_path_buf();
    let st = app.plan_state(f);
    st.home = home;
    st.refresh_facets();
    st.sort = match kit_list::sort_of(&prefs, &plan_sorts(&plan)).0.key {
        "name" => Sort::Name,
        "date" => Sort::Date,
        _ => Sort::Size,
    };
    st.prefs = prefs;
    // Nothing found: the module's object at rest, one line, the notes, Scan Again.
    if plan.items.is_empty() {
        let started = kit::first_seen(ui, ("empty", &plan.id));
        kit::centered_block(ui, egui::Id::new(("empty-block", f.as_str())), |ui| {
            kit::done_hero(ui, &t, f, 150.0, started);
            ui.add_space(space::M);
            ui.label(
                crate::rich("Nothing to clean here")
                    .font(font(size::TITLE2, Weight::SemiBold))
                    .color(t.text),
            );
            ui.add_space(space::XS);
            ui.label(
                crate::rich("Your Mac has nothing this module would clean right now.")
                    .font(font(size::CALLOUT, Weight::Regular))
                    .color(t.text_dim),
            );
            // Developer Clutter says where it looked, with a way to add a folder (U7-6).
            if f == FeatureId::DevClutter {
                super::dev_clutter::empty_extra(app, ui);
            }
            // What could not be looked at stays one click away, not in the way.
            if !plan.notes.is_empty() {
                ui.add_space(space::S);
                let open_id = egui::Id::new(("empty-details", &plan.id));
                let open: bool = ui.data(|d| d.get_temp(open_id)).unwrap_or(false);
                let label = if open { "Hide Details" } else { "Details" };
                if kit::ghost(ui, &t, label, Some(Icon::Chevron)).clicked() {
                    app.act();
                    ui.data_mut(|d| d.insert_temp(open_id, !open));
                }
                if open {
                    ui.add_space(space::XS);
                    for note in &plan.notes {
                        ui.label(
                            crate::rich(sentence(note))
                                .font(font(size::FOOTNOTE, Weight::Regular))
                                .color(t.text_dim),
                        );
                    }
                }
            }
            ui.add_space(space::L);
            if kit::secondary(ui, &t, "Scan Again") {
                app.act();
                start_scan(app, f);
            }
        });
        return;
    }
    // Summary: the big total and the sentence under it.
    // Duplicates count what can be freed: every copy but the one each set keeps
    // (REVIEW-C C39).
    let (total, line) = if f == FeatureId::Duplicates {
        let (bytes, n) = duplicate_copies(&plan);
        (
            bytes,
            format!(
                "in {n} duplicate {}",
                if n == 1 { "copy" } else { "copies" }
            ),
        )
    } else {
        (
            plan.total_size(),
            format!(
                "found in {}",
                cleancraft_core::fsutil::items(plan.items.len())
            ),
        )
    };
    ui.horizontal_top(|ui| {
        kit::display_count(ui, &t, ("plan-total", &plan.id), total);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.add_space(kit::baseline_offset(size::DISPLAY, size::TITLE3));
            ui.label(
                crate::rich(line)
                    .font(font(size::TITLE3, Weight::Medium))
                    .color(t.text_dim),
            );
            for note in &plan.notes {
                kit::dim(ui, &t, note);
            }
        });
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), kit::DISPLAY_ROW_H),
            Layout::right_to_left(Align::Center),
            |ui| {
                if kit::secondary(ui, &t, "Scan Again") {
                    app.act();
                    start_scan(app, f);
                }
            },
        );
    });

    // The bar's place is kept at the bottom of the window; it is laid out after the
    // toolbar and the lists so Tab and VoiceOver meet it last, in reading order (REVIEW-C
    // C21).
    let any_selectable = app
        .plan_state(f)
        .plan
        .as_ref()
        .is_some_and(|p| p.items.iter().any(|i| i.locked.is_none()));
    let (sel_count, sel_size) = app
        .plan_state(f)
        .plan
        .as_ref()
        .map(|p| (p.selected_count(), p.selected_size()))
        .unwrap_or((0, 0));
    let full = ui.available_rect_before_wrap();
    let bar_h = 12.0 + kit::BAR_H;
    let content = Rect::from_min_max(full.min, pos2(full.right(), full.bottom() - bar_h));
    let bar = Rect::from_min_max(pos2(full.left(), full.bottom() - bar_h), full.max);
    ui.scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
        results_body(app, ui, f, &plan);
    });
    if f == FeatureId::TrashBins {
        super::extras_cleanup::trash_frame(app, ui);
    }
    ui.scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
        ui.add_space(12.0);
        ui.set_clip_rect(ui.ctx().content_rect());
        kit::bar_frame(&t).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.set_min_height(44.0);
                // Trash Bins: nothing can be ticked there, so the bar shows what the
                // Trash holds, not "0 items selected" (REVIEW-C D1, REVIEW-U U3).
                let trash_only = f == FeatureId::TrashBins && !any_selectable;
                let (figure, line, full) = if trash_only {
                    // What Finder empties: Mail's deleted messages only go with Erase in Mail
                    // (U7-13).
                    let finder: u64 = plan
                        .items
                        .iter()
                        .filter(|i| i.category != "mail_trash")
                        .map(|i| i.size)
                        .sum();
                    (
                        human_size(finder),
                        "in the Trash".to_string(),
                        format!("{} in the Trash", human_size(finder)),
                    )
                } else {
                    // Selected items the search or filters hide still move: say so.
                    let note = kit_list::hidden_note(hidden_selected(app.plan_state(f)));
                    (
                        human_size(sel_size),
                        format!(
                            "{} selected{note}",
                            cleancraft_core::fsutil::items(sel_count)
                        ),
                        format!("{sel_count} selected{note}, {}", human_size(sel_size)),
                    )
                };
                kit::bar_figure(ui, &t, &figure, &line, &full, false);
                ui.add_space(space::M);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if sel_count > 0 && kit::primary(ui, &t, "Review and Clean") {
                        app.act();
                        app.plan_state(f).phase = Phase::Confirm;
                    } else if sel_count == 0 && any_selectable {
                        kit::dim(ui, &t, "Tick the items to clean.");
                    } else if sel_count == 0 && f == FeatureId::TrashBins {
                        // The page's one action sits where every other page has its
                        // main button (REVIEW-U U3).
                        super::extras_cleanup::trash_bar(app, ui);
                        // The hint gets only the room left between the figure and the
                        // button, shortened at its end (never under the figure).
                        kit::bar_hint(ui, &t, &super::extras_cleanup::trash_hint(&plan));
                    }
                });
            });
        });
    });
    ui.advance_cursor_after_rect(full);
}

/// The toolbar and the two columns of a results page.
fn results_body(app: &mut CleanApp, ui: &mut Ui, f: FeatureId, plan: &Plan) {
    let t = app.t;
    ui.add_space(space::M);
    // One toolbar over both columns: search, sort and selection. A module's own buttons
    // (Wi-Fi Settings, Reset Permissions, Select Copies...) join the row when they fit and
    // get a second row, aligned right, when they do not: nothing ever overlaps.
    let extras_id = egui::Id::new(("plan-extras-w", f.as_str()));
    let extras_w: f32 = ui.data(|d| d.get_temp(extras_id)).unwrap_or(0.0);
    let list = list_id(f);
    let sorts = plan_sorts(plan);
    // Filter about 100 when the list has filters.
    let filter_w = if plan_filters(app.plan_state(f)).is_empty() {
        0.0
    } else {
        100.0 + space::S
    };
    // Search 220 (160 at least), sort about 150, Select All and Select None about 210.
    let core_w = 160.0 + 150.0 + filter_w + 210.0 + 4.0 * space::S;
    let extras_inline = extras_w <= 0.5 || ui.available_width() - core_w >= extras_w + space::S;
    let search_w = (ui.available_width()
        - 150.0
        - filter_w
        - 210.0
        - if extras_inline {
            extras_w
        } else if extras_w > 0.5 {
            110.0
        } else {
            0.0
        }
        - 5.0 * space::S)
        .clamp(160.0, 220.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = space::S;
        let st = app.plan_state(f);
        let (_, changed) = kit_list::search(ui, &t, &mut st.search, "Search", "Search", search_w);
        if changed {
            app.act();
        }
        let filters = plan_filters(app.plan_state(f));
        let mut prefs = app.plan_state(f).prefs.clone();
        if kit_list::menus(ui, &t, &list, &filters, &sorts, &mut prefs) {
            app.act();
            set_prefs(app, f, prefs);
        }
        // Right-aligned, in reading order for Tab and VoiceOver: the module's own buttons
        // (or "More" holding them when they do not fit), Select All, Select None. Where
        // nothing can be selected (Trash Bins) there is no selection to offer.
        let selectable = app
            .plan_state(f)
            .plan
            .as_ref()
            .is_some_and(|p| p.items.iter().any(|i| i.locked.is_none()));
        let gap = space::S;
        let all_w = kit::pill_size(ui, "Select All", kit::Kind::Secondary, false).x;
        let none_w = kit::pill_size(ui, "Select None", kit::Kind::Secondary, false).x;
        let has_extras = extras_w > 0.5;
        let more_w = kit::pill_size(ui, "More", kit::Kind::Secondary, false).x + 24.0;
        let lead_w = if extras_inline {
            extras_w
        } else if has_extras {
            more_w + gap
        } else {
            0.0
        };
        let sel_w = if selectable {
            all_w + none_w + gap
        } else {
            0.0
        };
        ui.add_space((ui.available_width() - lead_w - sel_w).max(0.0));
        if extras_inline {
            extras_row(app, ui, f, extras_id);
        } else if has_extras {
            // The window is too narrow for one row: the module's buttons go in a menu
            // instead of a second toolbar row (REVIEW-C C17).
            let r = kit::menu_pill(ui, &t, "More", |ui| {
                ui.set_min_width(200.0);
                super::extras::results_toolbar(app, ui, f);
            });
            if r.response.clicked() {
                app.act();
            }
        }
        if selectable && kit::secondary(ui, &t, "Select All") {
            app.act();
            let ids = visible_items(app.plan_state(f));
            if let Some(p) = app.plan_state(f).plan.as_mut() {
                for id in ids {
                    p.set_selected(id, true);
                }
            }
            keep_a_copy(app, f);
        }
        if selectable && kit::secondary(ui, &t, "Select None") {
            app.act();
            let ids = visible_items(app.plan_state(f));
            if let Some(p) = app.plan_state(f).plan.as_mut() {
                for id in ids {
                    p.set_selected(id, false);
                }
            }
        }
    });
    // "Showing 12 of 135 items", the filters that are on, Clear Filters.
    let filters = plan_filters(app.plan_state(f));
    let total = plan.items.len();
    let shown = {
        let st = app.plan_state(f);
        let q = kit_list::query(&st.search);
        plan.items
            .iter()
            .filter(|i| passes(st, &filters, &q, i))
            .count()
    };
    let mut prefs = app.plan_state(f).prefs.clone();
    let mut search = std::mem::take(&mut app.plan_state(f).search);
    let ch = kit_list::status(
        ui,
        &t,
        shown,
        total,
        ("item", "items"),
        &filters,
        &mut prefs,
        &mut search,
    );
    app.plan_state(f).search = search;
    if ch.any() {
        app.act();
    }
    if ch.prefs {
        set_prefs(app, f, prefs);
    }
    ui.add_space(space::M);
    let avail_h = ui.available_height() - 4.0;
    let body =
        egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), avail_h));
    let left_w = if body.width() < 760.0 { 260.0 } else { 272.0 };
    let left = egui::Rect::from_min_size(body.min, egui::vec2(left_w, avail_h));
    let right = egui::Rect::from_min_max(
        egui::pos2(left.right() + space::GUTTER, body.top()),
        body.max,
    );
    ui.allocate_rect(body, egui::Sense::hover());
    // Categories.
    ui.scope_builder(egui::UiBuilder::new().max_rect(left), |ui| {
        ui.set_clip_rect(left.expand(24.0).intersect(ui.clip_rect()));
        kit::card(&t)
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.set_min_size(egui::vec2(left_w - 18.0, avail_h - 18.0));
                ui.set_max_width(left_w - 18.0);
                let out = egui::ScrollArea::vertical()
                    .id_salt("cats")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        categories(app, ui, f, plan);
                    });
                kit::scroll_fade(ui, &out, t.glass_solid());
            });
    });
    // Items.
    ui.scope_builder(egui::UiBuilder::new().max_rect(right), |ui| {
        ui.set_clip_rect(right.expand(24.0).intersect(ui.clip_rect()));
        let rows = rows(app.plan_state(f));
        let list_h = (ui.available_height() - 18.0).max(60.0);
        kit::card(&t)
            .inner_margin(egui::Margin::symmetric(6, 8))
            .show(ui, |ui| {
                ui.set_min_size(egui::vec2(ui.available_width(), list_h));
                if rows.is_empty() {
                    let st = app.plan_state(f);
                    let filtering = kit_list::filtering(&st.prefs, &plan_filters(st));
                    kit::empty_state(
                        ui,
                        &t,
                        Icon::Search,
                        "No items match",
                        if filtering {
                            "Clear Filters shows every item again."
                        } else {
                            "Try other words, or clear the search to see every item."
                        },
                    );
                    return;
                }
                let out = egui::ScrollArea::vertical()
                    .id_salt("items")
                    .max_height(list_h)
                    .auto_shrink([false, false])
                    .show_rows(ui, ROW_H, rows.len(), |ui, range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let first = range.start;
                        for (i, row) in rows[range].iter().enumerate() {
                            // Rows fade in one after the other when the results arrive.
                            let (k, _) = kit::appear(ui, ("plan-rows", &plan.id), first + i);
                            ui.scope(|ui| {
                                // Present from the first frame, fading up (REVIEW-C C10).
                                ui.multiply_opacity(0.3 + 0.7 * k);
                                match row {
                                    Row::Item(id) => item_row(app, ui, f, plan, *id),
                                    Row::Group(g) => group_row(app, ui, f, g),
                                }
                            });
                        }
                    });
                kit::scroll_fade(ui, &out, t.glass_solid());
            });
    });
}

/// The module's own toolbar buttons, measured so the toolbar can place them next frame.
fn extras_row(app: &mut CleanApp, ui: &mut Ui, f: FeatureId, id: egui::Id) {
    let start = ui.cursor().min.x;
    super::extras::results_toolbar(app, ui, f);
    let w = (ui.cursor().min.x - start).max(0.0);
    let last: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(0.0);
    if (w - last).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(id, w));
        ui.ctx().request_discard("toolbar buttons changed width");
    }
}

/// The category column: "All items" and one card per category with a share bar.
fn categories(app: &mut CleanApp, ui: &mut Ui, f: FeatureId, plan: &Plan) {
    let t = app.t;
    let total = plan.total_size().max(1);
    let all_sel = app.plan_state(f).category.is_none();
    let all_label = format!("All items  {}", human_size(plan.total_size()));
    let r = category_row(
        ui,
        &t,
        &all_label,
        "All items",
        &human_size(plan.total_size()),
        &cleancraft_core::fsutil::items(plan.items.len()),
        1.0,
        t.sec_a,
        all_sel,
        None,
    );
    if r.row_clicked {
        app.act();
        app.plan_state(f).category = None;
    }
    ui.add_space(2.0);
    let mut k = 0;
    for c in &plan.categories {
        let count = plan.category_count(&c.id);
        if count == 0 {
            continue;
        }
        let color = kit::series_color(&t, k + 1);
        k += 1;
        let sel_items = plan
            .items
            .iter()
            .filter(|i| i.category == c.id && i.locked.is_none())
            .collect::<Vec<_>>();
        let all_on = !sel_items.is_empty() && sel_items.iter().all(|i| i.selected);
        let some_on = sel_items.iter().any(|i| i.selected);
        let size = plan.category_size(&c.id);
        let current = app.plan_state(f).category.as_deref() == Some(c.id.as_str());
        let label = format!("{}  {}", c.title, human_size(size));
        let r = category_row(
            ui,
            &t,
            &label,
            &c.title,
            &human_size(size),
            &cleancraft_core::fsutil::items(count),
            size as f32 / total as f32,
            color,
            current,
            // No checkbox where nothing in the whole list can be selected (Trash Bins:
            // CleanCraft never empties a Trash itself; REVIEW-U U3).
            plan.items.iter().any(|i| i.locked.is_none()).then(|| {
                (
                    !sel_items.is_empty(),
                    all_on,
                    some_on && !all_on,
                    format!("Select {}", c.title),
                )
            }),
        );
        r.row.on_hover_text(crate::tr(&c.description));
        if let Some(on) = r.check_changed {
            app.act();
            if let Some(p) = app.plan_state(f).plan.as_mut() {
                p.set_category_selected(&c.id, on);
            }
            keep_a_copy(app, f);
        }
        if r.row_clicked {
            app.act();
            app.plan_state(f).category = Some(c.id.clone());
        }
    }
}

pub struct CatResp {
    pub row: egui::Response,
    pub row_clicked: bool,
    pub check_changed: Option<bool>,
}

/// One category card: an optional checkbox, the title and size, the item count and a bar
/// with the category's share of the total. The row's accessible name is `label`.
pub fn category_row(
    ui: &mut Ui,
    t: &crate::theme::Tokens,
    label: &str,
    title: &str,
    size_text: &str,
    count_text: &str,
    share: f32,
    color: Color32,
    current: bool,
    check: Option<(bool, bool, bool, String)>,
) -> CatResp {
    let w = ui.available_width();
    // One Tab stop per category (REVIEW-C C21): with a checkbox the checkbox is the stop
    // (Space ticks it, Return shows the category); without one, the row.
    let sense = if check.is_some() {
        Sense::CLICK
    } else {
        Sense::click()
    };
    let (r, resp) = ui.allocate_exact_size(vec2(w, hit::CATEGORY), sense);
    resp.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            true,
            current,
            crate::tr(label),
        )
    });
    let feel = kit::feel_ring(ui, t, &resp, r, 12.0);
    let hov = feel.hov;
    let p = ui.painter().clone();
    if current {
        p.rect_filled(r, 12.0, color.gamma_multiply(0.16));
        p.rect_stroke(
            r,
            12.0,
            egui::Stroke::new(1.0, color.gamma_multiply(0.45)),
            egui::StrokeKind::Inside,
        );
    } else if hov > 0.0 || feel.down > 0.0 {
        p.rect_filled(
            r,
            12.0,
            Color32::from_white_alpha((10.0 * hov + 8.0 * feel.down) as u8),
        );
    }
    let mut x = r.left() + 10.0;
    let mut check_changed = None;
    let mut cb_enter = false;
    if let Some((enabled, on, mixed, name)) = check {
        let cb_rect = Rect::from_center_size(pos2(x + 10.0, r.top() + 17.0), vec2(22.0, 22.0));
        let mut v = on;
        let cb = kit::place(ui, cb_rect, |ui| {
            ui.add_enabled_ui(enabled, |ui| kit::check_mixed(ui, t, &mut v, mixed, &name))
                .inner
        });
        if cb.changed() {
            check_changed = Some(v);
        }
        cb_enter = cb.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        x += 28.0;
    } else {
        p.circle_filled(pos2(x + 10.0, r.top() + 17.0), 5.0, color);
        x += 28.0;
    }
    // Names are never cut in the middle (REVIEW-C C4): when the name and the size do not fit
    // on line 1 together, the size moves to line 2 (after the bar) and the name gets the
    // whole line; only a name longer than the whole line ends in an ellipsis.
    let name_font = font(size::BODY, Weight::SemiBold);
    let size_g = kit::galley(ui, size_text, name_font.clone(), t.text);
    let size_w = size_g.size().x;
    let full_name_w = kit::galley(ui, title, name_font.clone(), t.text).size().x;
    let title_w = r.right() - 18.0 - size_w - x;
    // In the narrow column (small windows) every row puts its size on line 2, so sizes line
    // up in one place down the list instead of jumping between the two lines.
    let narrow = r.width() < 250.0;
    let size_on_line1 = !narrow && full_name_w <= title_w;
    let small_size = if size_on_line1 {
        None
    } else {
        Some(kit::galley(
            ui,
            size_text,
            font(size::FOOTNOTE, Weight::SemiBold),
            t.text,
        ))
    };
    let title_s = if size_on_line1 {
        title.to_string()
    } else {
        kit::truncate_end(ui.ctx(), title, &name_font, r.right() - 10.0 - x)
    };
    if size_on_line1 {
        p.galley(
            pos2(
                r.right() - 10.0 - size_w,
                r.top() + 17.0 - size_g.size().y / 2.0,
            ),
            size_g,
            t.text,
        );
    }
    p.text(
        pos2(x, r.top() + 17.0),
        egui::Align2::LEFT_CENTER,
        crate::tr(title_s),
        name_font,
        if current {
            t.text
        } else {
            icons::lerp(t.text_dim, t.text, 0.5 + 0.5 * hov)
        },
    );
    let count_g = kit::galley(
        ui,
        count_text,
        font(size::FOOTNOTE, Weight::Regular),
        t.text_faint,
    );
    let count_w = count_g.size().x;
    p.galley(
        pos2(x, r.top() + 35.0 - count_g.size().y / 2.0),
        count_g,
        t.text_faint,
    );
    let mut bar_right = r.right() - 10.0;
    if let Some(g) = small_size {
        let w = g.size().x;
        p.galley(
            pos2(r.right() - 10.0 - w, r.top() + 35.0 - g.size().y / 2.0),
            g,
            t.text,
        );
        bar_right -= w + 8.0;
    }
    let bar = Rect::from_min_max(
        pos2(x + count_w.max(56.0) + 10.0, r.top() + 32.5),
        pos2(bar_right, r.top() + 37.5),
    );
    if share > 0.0 && bar.width() > 12.0 {
        kit::paint_bar(&p, bar, t, share, color);
    }
    CatResp {
        row_clicked: resp.clicked() || cb_enter,
        row: resp,
        check_changed,
    }
}

/// A line of the item list: a set header (duplicates, look-alike pictures) or an item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    Group(String),
    Item(usize),
}

/// The visible items, with a header above each set when items belong to sets. Sets keep the
/// order of their first item in the sorted list; items without a set come last.
pub fn rows(st: &PlanState) -> Vec<Row> {
    let ids = visible_items(st);
    let Some(plan) = &st.plan else {
        return Vec::new();
    };
    if !ids.iter().any(|i| plan.items[*i].group.is_some()) {
        return ids.into_iter().map(Row::Item).collect();
    }
    let mut order: Vec<String> = Vec::new();
    let mut sets: std::collections::HashMap<String, Vec<usize>> = Default::default();
    let mut loose = Vec::new();
    for id in ids {
        match &plan.items[id].group {
            Some(g) => {
                if !sets.contains_key(g) {
                    order.push(g.clone());
                }
                sets.entry(g.clone()).or_default().push(id);
            }
            None => loose.push(id),
        }
    }
    // Sets of copies always get a header; other groupings only when it groups something.
    let copies = plan.feature.is_some_and(keeps_a_copy);
    let mut out = Vec::new();
    for g in order {
        let ids = sets.remove(&g).unwrap_or_default();
        // A Mail account's trash always gets its header: it carries "Erase in Mail...".
        let mail = ids.iter().any(|i| plan.items[*i].category == "mail_trash");
        if copies || mail || ids.len() > 1 {
            out.push(Row::Group(g.clone()));
        }
        out.extend(ids.into_iter().map(Row::Item));
    }
    out.extend(loose.into_iter().map(Row::Item));
    out
}

/// Features whose sets are copies of one another: one item per set must always stay.
/// Other features may use `group` only to show headers (e.g. one set per app).
pub fn keeps_a_copy(f: FeatureId) -> bool {
    matches!(f, FeatureId::Duplicates | FeatureId::SimilarPhotos)
}

/// Bytes and number of the copies a duplicates plan could free: in each set, every copy but
/// one (the copies are the same size; the largest is the one counted as kept).
pub fn duplicate_copies(plan: &Plan) -> (u64, usize) {
    let mut sets: std::collections::BTreeMap<&str, Vec<u64>> = Default::default();
    let (mut bytes, mut n) = (0u64, 0usize);
    for it in &plan.items {
        match &it.group {
            Some(g) => sets.entry(g.as_str()).or_default().push(it.size),
            None => {
                bytes += it.size;
                n += 1;
            }
        }
    }
    for sizes in sets.values() {
        let max = sizes.iter().copied().max().unwrap_or(0);
        bytes += sizes.iter().sum::<u64>() - max;
        n += sizes.len().saturating_sub(1);
    }
    (bytes, n)
}

/// After a bulk selection: every set keeps one copy (the suggested one).
fn keep_a_copy(app: &mut CleanApp, f: FeatureId) {
    if !keeps_a_copy(f) {
        return;
    }
    let env = app.env.clone();
    let fixed = app
        .plan_state(f)
        .plan
        .as_mut()
        .map(|p| duplicates::keep_one_per_group(&env, p))
        .unwrap_or(0);
    if fixed > 0 {
        app.tell(format!(
            "One file in each set stays unselected, so a copy is always kept ({fixed} sets)."
        ));
    }
}

fn group_row(app: &mut CleanApp, ui: &mut Ui, f: FeatureId, g: &str) {
    let t = app.t;
    let Some(plan) = app.plan_state(f).plan.as_ref() else {
        return;
    };
    let members: Vec<_> = plan
        .items
        .iter()
        .filter(|i| i.group.as_deref() == Some(g))
        .collect();
    let n = members.len();
    let kept = members
        .iter()
        .filter(|i| !(i.selected && i.locked.is_none()))
        .count();
    let freed: u64 = members
        .iter()
        .filter(|i| i.selected && i.locked.is_none())
        .map(|i| i.size)
        .sum();
    let text = match f {
        FeatureId::SimilarPhotos => {
            format!(
                "{n} similar pictures: {kept} kept, {} to remove",
                human_size(freed)
            )
        }
        FeatureId::Duplicates => {
            format!(
                "{n} identical files: {kept} kept, {} to remove",
                human_size(freed)
            )
        }
        // Locked rows: no "0 KB selected" (user report); what the group holds, and its
        // next step beside it.
        FeatureId::TrashBins => {
            let total: u64 = members.iter().map(|i| i.size).sum();
            format!("{g}: {}", human_size(total))
        }
        _ => format!(
            "{g}: {}, {} selected",
            cleancraft_core::fsutil::items(n),
            human_size(freed)
        ),
    };
    let mail_group =
        f == FeatureId::TrashBins && members.iter().any(|i| i.category == "mail_trash");
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::hover());
    let line = Rect::from_min_max(
        pos2(r.left() + 8.0, r.bottom() - 10.0),
        pos2(r.right() - 8.0, r.bottom() - 9.0),
    );
    ui.painter()
        .rect_filled(line, 0.0, t.sec_a.gamma_multiply(0.25));
    let ir = Rect::from_center_size(
        pos2(r.left() + 20.0, r.center().y + 1.0),
        egui::Vec2::splat(isz::INLINE),
    );
    icons::paint(ui.painter(), ir, Icon::Duplicates, t.sec_a);
    let mut text_right = r.right() - 8.0;
    if mail_group {
        // The group's next step, always visible (never only on hover).
        let bw = kit::pill_size(
            ui,
            super::extras_cleanup::ERASE_BUTTON,
            kit::Kind::Secondary,
            false,
        )
        .x;
        let br = Rect::from_min_max(
            pos2(r.right() - 8.0 - bw, r.top() + 2.0),
            pos2(r.right() - 8.0, r.bottom() - 12.0),
        );
        text_right = br.left() - 8.0;
        let g = g.to_string();
        kit::place_with(ui, br, Layout::right_to_left(Align::Center), |ui| {
            super::extras_cleanup::erase_button(app, ui, &g);
        });
    }
    let shown = kit::truncate_end(
        ui.ctx(),
        &text,
        &font(size::CALLOUT, Weight::SemiBold),
        (text_right - r.left() - 38.0).max(40.0),
    );
    kit::text_at(
        ui,
        Rect::from_min_max(
            pos2(r.left() + 38.0, r.top() + 8.0),
            pos2(text_right, r.bottom() - 10.0),
        ),
        &shown,
        &text,
        font(size::CALLOUT, Weight::SemiBold),
        t.text_dim,
        Align::Min,
    );
}

fn item_row(app: &mut CleanApp, ui: &mut Ui, f: FeatureId, plan: &Plan, id: usize) {
    let t = app.t;
    let it = &plan.items[id];
    let selected_now = app
        .plan_state(f)
        .plan
        .as_ref()
        .map(|p| p.items[id].selected)
        .unwrap_or(false);
    let name = item_name(f, it);
    let w = ui.available_width();
    let locked = it.locked.is_some();
    // The whole row is a click target that toggles it (its checkbox, Reveal and Ignore sit on
    // top and keep their own clicks). Not a Tab stop: the checkbox is the row's one stop.
    let (r, row) = ui.allocate_exact_size(
        vec2(w, ROW_H),
        if locked { Sense::hover() } else { Sense::CLICK },
    );
    let feel = kit::feel(ui, &row);
    // Over the row or anything on it (checkbox, Reveal, Ignore).
    let row_hovered = ui.rect_contains_pointer(r);
    let hov = kit::anim(
        ui.ctx(),
        egui::Id::new(("row-hov", f.as_str(), id)),
        row_hovered,
        crate::theme::motion::HOVER,
    );
    let p = ui.painter().clone();
    let card = r.shrink2(vec2(2.0, 2.0));
    if selected_now {
        p.rect_filled(
            card,
            12.0,
            t.sec_a.gamma_multiply(0.07 + 0.04 * hov + 0.04 * feel.down),
        );
    } else if hov > 0.0 || feel.down > 0.0 {
        p.rect_filled(
            card,
            12.0,
            Color32::from_white_alpha((9.0 * hov + 8.0 * feel.down) as u8),
        );
    }
    // Checkbox, named after the file.
    let cb_rect = Rect::from_center_size(pos2(r.left() + 20.0, r.center().y), vec2(22.0, 22.0));
    let mut on = selected_now;
    let cb = kit::check_or_lock(ui, &t, cb_rect, &mut on, &name, locked);
    // Its size and why it is listed are heard too, not only the name (REVIEW-C C33).
    let described = format!("{}, {}", human_size(it.size), reason_text(it));
    ui.ctx()
        .accesskit_node_builder(cb.id, |b| b.set_description(described));
    let row_click = row.clicked() && !locked;
    if row_click {
        on = !selected_now;
    }
    if cb.changed() || row_click {
        app.act();
        let every_copy = on
            && keeps_a_copy(f)
            && app
                .plan_state(f)
                .plan
                .as_ref()
                .is_some_and(|p| duplicates::would_select_every_copy(p, id));
        if every_copy {
            app.warn(
                "Keep at least one file in each set. Deselect another copy first, then select this one.",
            );
        } else if let Some(p) = app.plan_state(f).plan.as_mut() {
            p.set_selected(id, on);
        }
    }
    // Kind tile.
    let tile = Rect::from_center_size(
        pos2(r.left() + 60.0, r.center().y),
        egui::Vec2::splat(isz::ROW_TILE),
    );
    p.rect_filled(tile, 10.0, Color32::from_white_alpha(14));
    // The file's own kind (folder, archive, log, picture preview...), from images.rs.
    crate::images::paint_file_icon(
        ui,
        Rect::from_center_size(tile.center(), egui::Vec2::splat(isz::ROW + 4.0)),
        &it.path,
        crate::file_icons::FileKind::of(&it.path),
    );
    // Size on the first line, on the right; Reveal and Ignore show over the end of the row on
    // hover (they are always there for the keyboard, VoiceOver and tests), so names, reasons
    // and paths get the width.
    let size_text = human_size(it.size);
    let size_font = font(size::BODY, Weight::SemiBold);
    let size_w = kit::galley(ui, &size_text, size_font.clone(), t.text)
        .size()
        .x;
    let size_r = Rect::from_min_max(
        pos2(r.right() - 14.0 - size_w.max(64.0), r.top() + 8.0),
        pos2(r.right() - 12.0, r.top() + 32.0),
    );
    kit::text_at(
        ui,
        size_r,
        &size_text,
        &size_text,
        size_font,
        if locked { t.text_faint } else { t.text },
        Align::Max,
    );
    let x0 = r.left() + 90.0;
    // Line 1: the name, then where it is (dimmed), up to the size.
    let text_w = (size_r.left() - 16.0 - x0).max(60.0);
    // Line 2 (why it is listed) runs under the size too, to the row's end.
    let tag_room = (r.right() - 12.0 - x0).max(60.0);
    let name_font = font(size::BODY, Weight::SemiBold);
    // The name comes first: whole while the path keeps at least a short stub, shortened
    // only after the path has shrunk to it (REVIEW-C D6). A display name ("Trash of the
    // startup disk") ends in an ellipsis; a file name keeps its end (".ipsw").
    const MIN_PATH: f32 = 64.0;
    let name_room = (text_w - 12.0 - MIN_PATH).max(text_w * 0.5);
    let file_name = it
        .path
        .file_name()
        .is_some_and(|n| n.to_string_lossy() == name);
    let name_s = if file_name {
        kit::truncate_middle(ui.ctx(), &name, &name_font, name_room)
    } else {
        kit::truncate_end(ui.ctx(), &name, &name_font, name_room)
    };
    let name_w = kit::galley(ui, &name_s, name_font.clone(), t.text).size().x;
    // Painted only: the checkbox already carries the name for VoiceOver.
    p.text(
        pos2(x0, r.top() + 20.0),
        egui::Align2::LEFT_CENTER,
        crate::tr(name_s),
        name_font,
        if locked { t.text_dim } else { t.text },
    );
    // Trash rows are named after their disk or mailbox; the folder macOS keeps them in
    // ("/Volumes/USB/.Trashes/501", "/.../Deleted Messages.mbox") says nothing more, so it
    // is only in the tooltip (user report: a useless "/...x").
    let trash_row = f == FeatureId::TrashBins;
    let shown = if trash_row {
        String::new()
    } else {
        app.env.display(&it.path)
    };
    let path_x = x0 + name_w + 12.0;
    // While the row is hovered its actions cover the end of line 1: the path is shortened
    // again to end before them (a middle ellipsis, never a word cut by the buttons).
    // Trash rows have no row actions (nothing to ignore or reveal): no space is taken.
    let actions_w = if keeps_a_copy(f) {
        262.0
    } else if trash_row {
        0.0
    } else {
        150.0
    };
    let actions_left = size_r.left() - 12.0 - actions_w;
    let path_end = if row_hovered || hov > 0.0 || cb.has_focus() {
        (x0 + text_w).min(actions_left - 20.0)
    } else {
        x0 + text_w
    };
    let path_w = (path_end - path_x).max(24.0);
    let pf = font(size::FOOTNOTE, Weight::Regular);
    let path_s = kit::truncate_middle(ui.ctx(), &shown, &pf, path_w);
    let path_resp = kit::text_at(
        ui,
        Rect::from_min_size(pos2(path_x, r.top() + 11.0), vec2(path_w, 19.0)),
        &path_s,
        &shown,
        pf,
        t.text_faint,
        Align::Min,
    );
    kit::row_tip(&path_resp, r, &app.env.display(&it.path));
    // The reason as a tag. A reason that repeats the row's name ("Trash of USB: 1 item" on
    // the row "Trash of USB") shows only the rest.
    let why_full = match &it.locked {
        Some(l) => format!("{} (cannot be selected: {l})", it.reason),
        None => reason_text(it),
    };
    let reason = if locked {
        it.reason.clone()
    } else {
        reason_text(it)
    };
    let reason = reason
        .strip_prefix(&format!("{name}: "))
        .map(sentence_start)
        .unwrap_or(reason.clone());
    // ... and one that ends in it ("Safari browsing history: History.db" on the row
    // "History.db") drops that end.
    let reason = reason
        .strip_suffix(&format!(": {name}"))
        .map(str::to_string)
        .unwrap_or(reason);
    let tag_font = font(size::FOOTNOTE, Weight::Medium);
    // The row's actions show over its end while it is hovered or has the keyboard: the tag
    // then ends before them instead of running under them (REVIEW-C D2).
    let actions_focus_id = egui::Id::new(("row-actions-focus", f.as_str(), id));
    let actions_focused: bool = ui.data(|d| d.get_temp(actions_focus_id)).unwrap_or(false);
    let keyboard_here = cb.has_focus() || actions_focused;
    let covered = !trash_row && (row_hovered || hov > 0.0 || keyboard_here);
    let tag_room = if covered {
        (actions_left - 10.0 - 8.0 - x0).max(40.0)
    } else {
        tag_room
    };
    // A long reason drops its asides in brackets ("(Default profile)") before it is cut
    // (REVIEW-C C17); the tooltip keeps every word.
    let tag_s = fit_tag(ui.ctx(), &reason, &tag_font, tag_room - 16.0);
    let color = if locked {
        t.text_dim
    } else {
        kit::risk_color(&t, it.risk)
    };
    let tg = kit::galley(ui, &tag_s, tag_font.clone(), color);
    let tag_r = Rect::from_min_size(pos2(x0, r.top() + 31.0), tg.size() + vec2(16.0, 5.0));
    p.rect_filled(tag_r, tag_r.height() / 2.0, color.gamma_multiply(0.16));
    let tag_resp = kit::text_at(ui, tag_r, &tag_s, &why_full, tag_font, color, Align::Center);
    let tip = if locked {
        why_full.clone()
    } else {
        format!("{why_full}\n{}", kit::risk_note(it.risk))
    };
    kit::row_tip(&tag_resp, r, &tip);
    // In a set of copies, the copy that stays is marked "Kept" (REVIEW-U U11).
    let kept = keeps_a_copy(f)
        && !selected_now
        && !locked
        && it.group.as_ref().is_some_and(|g| {
            plan.items
                .iter()
                .any(|o| o.id != id && o.group.as_ref() == Some(g) && o.selected)
        });
    let kept = kept
        && app
            .plan_state(f)
            .plan
            .as_ref()
            .is_some_and(|p| !p.items[id].selected);
    if kept {
        let kf = font(size::FOOTNOTE, Weight::SemiBold);
        let kg = kit::galley(ui, "Kept", kf.clone(), t.success);
        let kr = Rect::from_min_size(
            pos2(tag_r.right() + 6.0, tag_r.top()),
            kg.size() + vec2(16.0, 5.0),
        );
        p.rect_filled(kr, kr.height() / 2.0, t.success.gamma_multiply(0.16));
        kit::text_at(
            ui,
            kr,
            "Kept",
            "Kept: this copy stays",
            kf,
            t.success,
            Align::Center,
        );
    }
    // Why a locked row cannot be ticked, after the tag when it fits whole (never cut: the
    // padlock and the tag's tooltip carry it otherwise).
    if let Some(l) = &it.locked {
        let lf = font(size::FOOTNOTE, Weight::Regular);
        let lg = kit::galley(ui, l, lf.clone(), t.text_faint);
        let lx = tag_r.right() + 8.0;
        let end = if covered {
            actions_left - 18.0
        } else {
            r.right() - 12.0
        };
        if lx + lg.size().x <= end {
            p.galley(
                pos2(lx, tag_r.center().y - lg.size().y / 2.0),
                lg,
                t.text_faint,
            );
        }
    }
    // Row actions, shown on hover.
    let actions = Rect::from_min_max(
        pos2(actions_left, r.top()),
        pos2(size_r.left() - 12.0, r.bottom()),
    );
    // Shown while the row is hovered, and while the keyboard is on the row (its checkbox
    // or one of the actions), so a Tab never lands on an invisible button.
    let show = if trash_row {
        0.0
    } else {
        hov.max(if row_hovered || keyboard_here {
            1.0
        } else {
            0.0
        })
    };
    if show > 0.0 {
        // A soft solid under the buttons so the text beneath does not show through.
        let under = actions
            .expand2(vec2(0.0, -6.0))
            .with_min_x(actions.left() - 10.0);
        let solid = icons::lerp(
            t.glass_solid(),
            t.sec_a,
            if selected_now { 0.10 } else { 0.03 },
        );
        p.rect_filled(under, 10.0, solid.gamma_multiply(show));
    }
    kit::place_with(ui, actions, Layout::right_to_left(Align::Center), |ui| {
        ui.multiply_opacity(show);
        ui.spacing_mut().item_spacing.x = 4.0;
        // Row actions fit the module (REVIEW-U U27): a whole Trash is not ignored or
        // revealed here, and labels inside a file (Wi-Fi networks, permissions) cannot be
        // shown in Finder.
        let on_disk = std::fs::symlink_metadata(&it.path).is_ok();
        let trash = f == FeatureId::TrashBins;
        let ignored = app.settings.ignore.iter().any(|p| it.path.starts_with(p));
        let mut focused = false;
        if keeps_a_copy(f) && !locked && it.group.is_some() && !kept {
            let r = kit::ghost(ui, &t, "Keep This One", None)
                .on_hover_text(crate::tr("Keep this copy and select the others in its set"));
            focused |= r.has_focus();
            if r.clicked() {
                app.act();
                let g = it.group.clone();
                if let Some(p) = app.plan_state(f).plan.as_mut() {
                    for i in 0..p.items.len() {
                        if p.items[i].group == g {
                            p.set_selected(i, i != id);
                        }
                    }
                }
            }
        }
        if !trash && ignored {
            let r = kit::ghost(ui, &t, "Stop Ignoring", None)
                .on_hover_text(crate::tr("Propose this again in scans"));
            focused |= r.has_focus();
            if r.clicked() {
                app.act();
                app.settings.ignore.retain(|p| !it.path.starts_with(p));
                app.save_settings();
                if let Some(p) = app.plan_state(f).plan.as_mut() {
                    p.items[id].locked = None;
                }
                app.tell(format!("{name} is no longer ignored."));
            }
        } else if !trash {
            let ig = kit::ghost(ui, &t, "Ignore", None).on_hover_text(crate::tr(
                "Never propose this again (Settings > Ignore list)",
            ));
            focused |= ig.has_focus();
            if ig.clicked() {
                app.act();
                app.settings.ignore.push(it.path.clone());
                app.save_settings();
                if let Some(p) = app.plan_state(f).plan.as_mut() {
                    p.set_selected(id, false);
                    p.items[id].locked = Some("on your ignore list".into());
                }
                // Undo right in the notice (REVIEW-U N3): off the list, ticked as before.
                let path = it.path.clone();
                let was = selected_now;
                let shown = name.clone();
                app.tell_undo(
                    format!("{name} is on the ignore list now."),
                    "Ignore",
                    move |app| {
                        app.settings.ignore.retain(|p| *p != path);
                        app.save_settings();
                        if let Some(p) = app.plan_state(f).plan.as_mut()
                            && p.items.get(id).is_some_and(|i| i.path == path)
                        {
                            p.items[id].locked = None;
                            p.set_selected(id, was);
                        }
                        app.tell(format!("{shown} is no longer ignored."));
                    },
                );
            }
        }
        // Only files and folders on disk can be shown in Finder (Wi-Fi networks and
        // permissions are listed under a label, not a file).
        let reveal = (on_disk && !trash).then(|| {
            kit::ghost(ui, &t, "Reveal", Some(Icon::Reveal))
                .on_hover_text(crate::tr("Show in Finder"))
        });
        focused |= reveal.as_ref().is_some_and(|r| r.has_focus());
        ui.data_mut(|d| d.insert_temp(actions_focus_id, focused));
        if reveal.is_some_and(|r| r.clicked()) {
            app.act();
            let path = it.path.to_string_lossy().into_owned();
            match app.env.sys.run("/usr/bin/open", &["-R", &path]) {
                Ok(out) if out.ok() => {}
                Ok(out) => app.warn(format!("Could not open Finder: {}", out.stderr.trim())),
                Err(e) => app.warn(format!("Could not open Finder: {e}")),
            }
        }
    });
}

/// A reason tag's text for `max_w`: whole when it fits, else without its asides in brackets
/// ("Google Chrome (Default profile) browsing history" reads "Google Chrome browsing
/// history"), and only then cut at the end.
pub fn fit_tag(ctx: &egui::Context, text: &str, fid: &egui::FontId, max_w: f32) -> String {
    let width = |s: &str| ctx.fonts_mut(|f| s.chars().map(|c| f.glyph_width(fid, c)).sum::<f32>());
    if width(text) <= max_w {
        return text.to_string();
    }
    let mut plain = String::new();
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let plain = plain.replace(" ,", ",").replace(" :", ":");
    kit::truncate_end(ctx, &plain, fid, max_w)
}

/// The reason a confirmation line gives: the item's own, except for a duplicate, which names
/// the copy that stays ("Copy of ~/Downloads/photo.jpg, which stays") (U7-11).
pub fn confirm_reason(
    env: &cleancraft_core::Env,
    plan: &Plan,
    it: &cleancraft_core::Item,
) -> String {
    if plan.feature == Some(FeatureId::Duplicates)
        && let Some(g) = &it.group
        && let Some(keep) = plan
            .items
            .iter()
            .find(|o| o.group.as_ref() == Some(g) && !o.selected)
    {
        return format!("Copy of {}, which stays", env.display(&keep.path));
    }
    it.reason.clone()
}

/// The name a row shows. A Trash is named after its disk ("Trash of USB"), not after the
/// numeric user-id folder macOS keeps it in (`/Volumes/USB/.Trashes/501`).
pub fn item_name(f: FeatureId, it: &cleancraft_core::Item) -> String {
    // Developer Clutter: the project first ("alpha / target"), so rows of many projects do
    // not all read "target" (U7-10).
    if f == FeatureId::DevClutter
        && !it.path.to_string_lossy().contains("CoreSimulator")
        && let (Some(dir), Some(name)) = (
            it.path.parent().and_then(|d| d.file_name()),
            it.path.file_name(),
        )
    {
        return format!("{} / {}", dir.to_string_lossy(), name.to_string_lossy());
    }
    if f == FeatureId::TrashBins && it.category == "mail_trash" {
        // The mailbox ("Deleted Messages", "Trash"); the account is the group header.
        return it
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    if f == FeatureId::TrashBins
        && let Some(g) = &it.group
    {
        return g.clone();
    }
    it.path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// "You will be signed out of websites in Safari and Google Chrome." only when the user ticked
/// items whose removal signs them out (browser cookies in Privacy Traces, never preselected).
pub fn signed_out_note(plan: &Plan) -> Option<String> {
    if plan.feature != Some(FeatureId::Privacy) {
        return None;
    }
    let mut browsers: Vec<String> = Vec::new();
    for it in plan.selected() {
        if !features::privacy::signs_out(it) {
            continue;
        }
        let name = plan
            .categories
            .iter()
            .find(|c| c.id == it.category)
            .map(|c| c.title.clone())
            .unwrap_or_default();
        if !name.is_empty() && !browsers.contains(&name) {
            browsers.push(name);
        }
    }
    let list = match browsers.len() {
        0 => return None,
        1 => browsers[0].clone(),
        n => format!("{} and {}", browsers[..n - 1].join(", "), browsers[n - 1]),
    };
    Some(format!("You will be signed out of websites in {list}."))
}

/// The confirmation line for items that need the administrator password, when there are any.
pub fn admin_note(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some(
            "1 item is in a folder only an administrator can change. macOS will ask for the administrator password, and it goes to the CleanCraft quarantine."
                .into(),
        ),
        n => Some(format!(
            "{n} items are in folders only an administrator can change. macOS will ask for the administrator password, and they go to the CleanCraft quarantine."
        )),
    }
}

/// The reason shown for an item, with the administrator note.
pub fn reason_text(it: &cleancraft_core::Item) -> String {
    let mut s = it.reason.clone();
    if it.needs_admin && it.locked.is_none() {
        s.push_str(" (needs the administrator password)");
    }
    s
}

fn confirm(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    let t = app.t;
    let Some(plan) = app.plan_state(f).plan.clone() else {
        return;
    };
    // A feature can insist on the quarantine (Threat Check); say where items really go.
    let method = features::get(f)
        .and_then(|x| x.method_override())
        .unwrap_or(app.settings.method);
    let dest = match method {
        executor::Method::Trash => "the Trash",
        executor::Method::Quarantine | executor::Method::AdminQuarantine => {
            "the CleanCraft quarantine"
        }
    };
    let mut close = false;
    let mut go = false;
    let width = kit::dialog_width(ui.ctx());
    let max_list = (ui.ctx().content_rect().height() - 350.0).clamp(120.0, 360.0);
    let dlg = kit::modal(egui::Id::new(("confirm", f.as_str())), &t).show(ui.ctx(), |ui| {
        ui.set_width(width);
        kit::dialog_head(
            ui,
            &t,
            Icon::Trash,
            t.sec_a,
            &format!(
                "Move {} ({}) to {dest}?",
                cleancraft_core::fsutil::items(plan.selected_count()),
                human_size(plan.selected_size())
            ),
            "These exact files will be moved. You can put them back from History.",
        );
        ui.add_space(space::M);
        if let Some(note) = admin_note(features::admin_count(&plan)) {
            kit::strip(ui, Icon::Warning, t.warning, &note);
            ui.add_space(space::S);
        }
        // Said once, where the decision is made (REVIEW-U U20), not only in row tags.
        if let Some(note) = signed_out_note(&plan) {
            kit::strip(ui, Icon::Warning, t.warning, &note);
            ui.add_space(space::S);
        }
        // Only the visible rows are laid out: plans can hold thousands of items.
        let rows: Vec<&cleancraft_core::Item> = plan.selected().collect();
        kit::well(&t).show(ui, |ui| {
            ui.set_width(ui.available_width());
            // A height known from the first frame (the list shrinks to its rows up to
            // `max_list`), so the dialog opens at its final size and place.
            let list_h = (rows.len() as f32 * hit::CONFIRM_LINE).min(max_list);
            let out = egui::ScrollArea::vertical()
                .id_salt("confirm-list")
                .min_scrolled_height(list_h)
                .max_height(list_h)
                .auto_shrink([false, false])
                .show_rows(ui, hit::CONFIRM_LINE, rows.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for it in &rows[range] {
                        kit::confirm_line(
                            ui,
                            &t,
                            &it.path,
                            &app.env.display(&it.path),
                            &confirm_reason(&app.env, &plan, it),
                            &human_size(it.size),
                        );
                    }
                });
            kit::list_more(ui, &t, &out, rows.len());
        });
        ui.add_space(space::L);
        let label = match method {
            executor::Method::Trash => "Move to Trash",
            executor::Method::Quarantine | executor::Method::AdminQuarantine => {
                "Move to Quarantine"
            }
        };
        let (ok, cancel) = kit::dialog_actions(ui, &t, label, kit::Kind::Danger);
        go |= ok;
        close |= cancel;
    });
    if dlg.should_close() {
        close = true;
    }
    if go {
        app.act();
        start_clean(app, f);
    } else if close {
        app.act();
        app.plan_state(f).phase = Phase::Results;
    }
}

/// What the person can do about an item that did not move (REVIEW-U U9): the reason in
/// plain words and the next step.
pub fn skip_hint(why: &executor::SkipReason) -> Option<String> {
    use executor::SkipReason as S;
    Some(match why {
        S::InUse => "Quit the app that uses it, then Try Again.".into(),
        S::InUseBy(a) => format!("Quit {a}, then Try Again."),
        S::AppRunning(a) => format!("Quit {a}, then Try Again."),
        S::PermissionDenied => "Turn on Full Disk Access for CleanCraft, then Try Again.".into(),
        S::DiskFull => "Empty the Trash, then Try Again.".into(),
        _ => return None,
    })
}

/// What a Done screen's "Not moved" card asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SkipActions {
    pub try_again: bool,
}

/// The list of what did not move, each with its next step; "Try Again" (when `retry`) and
/// "Open Privacy Settings" when macOS refused access.
pub fn skipped_card(
    app: &mut CleanApp,
    ui: &mut Ui,
    skipped: &[(std::path::PathBuf, executor::SkipReason)],
    max_h: f32,
    retry: bool,
) -> SkipActions {
    let t = app.t;
    let mut out = SkipActions::default();
    if skipped.is_empty() {
        return out;
    }
    let denied = skipped
        .iter()
        .any(|(_, w)| matches!(w, executor::SkipReason::PermissionDenied));
    let can_retry = retry
        && skipped
            .iter()
            .any(|(_, w)| !matches!(w, executor::SkipReason::Missing));
    kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width().min(720.0));
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(egui::Vec2::splat(isz::INLINE), Sense::hover());
            icons::paint(ui.painter(), r, Icon::Warning, t.warning);
            ui.label(
                crate::rich(format!(
                    "Not moved: {}",
                    cleancraft_core::fsutil::items(skipped.len())
                ))
                .font(font(size::TITLE3, Weight::SemiBold))
                .color(t.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if can_retry && kit::secondary(ui, &t, "Try Again") {
                    out.try_again = true;
                }
                if denied && kit::secondary(ui, &t, "Open Privacy Settings") {
                    app.act();
                    if let Err(e) = cleancraft_core::permissions::open_fda_settings(&app.env) {
                        app.warn(format!("Could not open System Settings: {e}"));
                    }
                }
            });
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .max_height(max_h.max(60.0))
            .show(ui, |ui| {
                for (p, why) in skipped {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(
                            crate::rich(app.env.display(p))
                                .font(font(size::CALLOUT, Weight::Medium))
                                .color(t.text),
                        );
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                crate::rich(why.to_string())
                                    .font(font(size::FOOTNOTE, Weight::Medium))
                                    .color(t.warning),
                            );
                            if let Some(h) = skip_hint(why) {
                                ui.label(
                                    crate::rich(h)
                                        .font(font(size::FOOTNOTE, Weight::Regular))
                                        .color(t.text_dim),
                                );
                            }
                        });
                    });
                    ui.add_space(space::XS);
                }
            });
    });
    out
}

/// The note under a Done summary: where the space comes back, by where the items went.
fn space_note(rep: &Report) -> Option<&'static str> {
    let method = rep.moved.iter().find_map(|r| match &r.kind {
        executor::ActionKind::Moved { method, .. } => Some(*method),
        _ => None,
    })?;
    Some(match method {
        executor::Method::Trash => "The space comes back when the Trash is emptied.",
        executor::Method::Quarantine | executor::Method::AdminQuarantine => {
            "The space comes back when the quarantine is moved to the Trash (Settings) and the Trash is emptied."
        }
    })
}

/// The Done screen every cleanup shares: the check, how much moved and where, the summary
/// line, the buttons (from the caller), "Empty Trash" when items went to the Trash, and what
/// did not move.
pub fn done_view(
    app: &mut CleanApp,
    ui: &mut Ui,
    feature: FeatureId,
    rep: &Report,
    summary: &str,
    buttons: impl FnOnce(&mut CleanApp, &mut Ui),
) {
    done_view_with(app, ui, feature, rep, summary, buttons, false);
}

/// [`done_view`] with "Try Again" on the "Not moved" card; returns what was asked.
pub fn done_view_with(
    app: &mut CleanApp,
    ui: &mut Ui,
    feature: FeatureId,
    rep: &Report,
    summary: &str,
    buttons: impl FnOnce(&mut CleanApp, &mut Ui),
    retry: bool,
) -> SkipActions {
    let t = app.t;
    let started = kit::first_seen(ui, ("done", &rep.plan_id));
    let id = egui::Id::new(("done-block", &rep.plan_id));
    let to_trash = space_note(rep) == Some("The space comes back when the Trash is emptied.");
    let mut acts = SkipActions::default();
    kit::centered_block(ui, id, |ui| {
        let d = if ui.available_height() < 460.0 {
            150.0
        } else {
            200.0
        };
        kit::done_hero(ui, &t, feature, d, started);
        ui.add_space(space::L);
        ui.label(
            crate::rich(features::moved_text([rep]))
                .font(font(size::LARGE_TITLE, Weight::Bold))
                .color(t.text),
        );
        ui.add_space(space::XS);
        ui.label(
            crate::rich(summary)
                .font(font(size::BODY_L, Weight::Regular))
                .color(t.text_dim),
        );
        if let Some(note) = space_note(rep) {
            kit::dim(ui, &t, note);
        }
        ui.add_space(space::XL);
        // The page's own buttons end with its primary one, so Empty Trash goes first.
        kit::centered_row(ui, id.with("buttons"), |ui| {
            if to_trash {
                super::extras_cleanup::empty_trash(app, ui, "Empty Trash...");
            }
            buttons(app, ui);
        });
        ui.add_space(space::L);
        let h = ui.available_height() - 80.0;
        acts = skipped_card(app, ui, &rep.skipped, h, retry);
    });
    acts
}

fn done(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    let Phase::Done(rep) = &app.plan_state(f).phase else {
        return;
    };
    let rep = rep.clone();
    let summary = kit::moved_summary(rep.moved.len(), rep.skipped.len());
    let acts = done_view_with(
        app,
        ui,
        f,
        &rep,
        &summary,
        |app, ui| {
            let t = app.t;
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
                start_scan(app, f);
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
                app.navigate("history");
            }
            if kit::primary(ui, &t, "Done") {
                app.act();
                // Back to the list when something is left in it (what did not move, what was
                // not selected); the start screen when the list is empty.
                let st = app.plan_state(f);
                if st.plan.as_ref().is_some_and(|p| !p.items.is_empty()) {
                    st.phase = Phase::Results;
                } else {
                    st.phase = Phase::Idle;
                    st.plan = None;
                }
            }
        },
        true,
    );
    if acts.try_again {
        // Only what did not move, with the same confirmation listing it.
        app.act();
        let again: Vec<std::path::PathBuf> = rep
            .skipped
            .iter()
            .filter(|(_, w)| !matches!(w, executor::SkipReason::Missing))
            .map(|(p, _)| p.clone())
            .collect();
        let st = app.plan_state(f);
        if let Some(plan) = st.plan.as_mut() {
            for i in 0..plan.items.len() {
                let on = again.contains(&plan.items[i].path);
                plan.set_selected(i, on);
            }
            if plan.selected_count() > 0 {
                st.phase = Phase::Confirm;
            }
        }
    }
}

/// A note as a sentence: capital first letter, full stop at the end.
pub fn sentence(s: &str) -> String {
    let mut c = s.trim().chars();
    let mut out: String = match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => return String::new(),
    };
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// A scan error in plain words: "Could not read ~/Library/Caches: permission denied (os error
/// 13)" becomes "CleanCraft is not allowed to read ~/Library/Caches." plus whether Full Disk
/// Access would help (the page then offers to open that pane).
pub fn friendly_error(e: &str) -> (String, bool) {
    let low = e.to_lowercase();
    let denied = (low.contains("permission denied") || low.contains("operation not permitted"))
        && low.contains("read");
    if denied {
        let place = e
            .split_once("read ")
            .and_then(|(_, rest)| rest.split_once(':').map(|(p, _)| p.trim().to_string()))
            .filter(|p| !p.is_empty());
        let text = match place {
            Some(p) => format!(
                "CleanCraft is not allowed to read {p}. Turn on Full Disk Access for CleanCraft, then scan again."
            ),
            None => "CleanCraft is not allowed to read a folder it needs. Turn on Full Disk Access for CleanCraft, then scan again.".to_string(),
        };
        return (text, true);
    }
    // Drop the OS error number.
    let mut text = e.to_string();
    if let Some(i) = text.find(" (os error") {
        text.truncate(i);
    }
    // Otherwise the words stay as written (they are already sentences from the modules).
    (text, false)
}

/// "1 item" stays, "items found" becomes "Items found": the rest of a reason after its
/// prefix, starting like a sentence.
fn sentence_start(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) if f.is_ascii_digit() => s.to_string(),
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}
