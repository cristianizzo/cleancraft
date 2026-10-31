//! The features. Each cleanup feature is one file implementing [`Feature`]: its allow-list and
//! a scan that returns a [`Plan`]. Removing is never done here; the executor does it after the
//! user confirmed the plan. Features that run commands or toggle settings (maintenance,
//! startup items) have their own action functions, which also log to the journal.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::env::Env;
use crate::executor::{self, ExecError, Method, Report, SkipReason};
use crate::plan::{FeatureId, Item, Plan};
use crate::policy;
use crate::progress::Progress;
use crate::settings::Settings;

pub mod dev_clutter;
pub mod downloads;
pub mod duplicates;
pub mod large_old;
pub mod leftovers;
pub mod mail;
pub mod malware;
pub mod privacy;
pub mod similar;
pub mod space_lens;
pub mod system_junk;
pub mod trash;
pub mod uninstaller;

/// What a scan gets.
pub struct ScanCtx<'a> {
    pub env: &'a Env,
    pub settings: &'a Settings,
    pub cancel: &'a AtomicBool,
    /// Called with the place being looked at, for the progress line.
    pub progress: &'a (dyn Fn(&str) + Sync),
    /// How far the scan is (the progress bar): report real units with
    /// [`crate::progress::Progress::set`]; [`ScanCtx::part`] gives a slice of it to a sub-step.
    pub meter: &'a Progress,
}

impl<'a> ScanCtx<'a> {
    /// The same context reporting into `meter` (a slice of this one's bar).
    pub fn with_meter<'b>(&'b self, meter: &'b Progress) -> ScanCtx<'b> {
        ScanCtx {
            env: self.env,
            settings: self.settings,
            cancel: self.cancel,
            progress: self.progress,
            meter,
        }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Settings' ignore list.
    pub fn ignored(&self, p: &Path) -> bool {
        self.settings.is_ignored(p)
    }

    /// May this path be proposed for this allow-list? (Same fence the executor uses.)
    pub fn allowed(&self, allow: &[PathBuf], p: &Path) -> bool {
        !self.ignored(p) && policy::check(self.env, allow, p).is_ok()
    }
}

pub trait Feature: Send + Sync {
    fn id(&self) -> FeatureId;
    /// Folders this feature may remove things from (strictly inside them).
    fn allow_roots(&self, env: &Env) -> Vec<PathBuf>;
    /// Look, never change anything.
    fn scan(&self, ctx: &ScanCtx<'_>) -> anyhow::Result<Plan>;
    /// Where this feature's items go, when it must not follow the user's setting
    /// (threats always go to the quarantine).
    fn method_override(&self) -> Option<Method> {
        None
    }
    /// May items the plan marks [`crate::plan::Item::needs_admin`] be moved with the
    /// administrator password (one `mv` per item into the quarantine, after the same checks)?
    /// Off unless a feature turns it on.
    fn allows_admin(&self) -> bool {
        false
    }
    /// launchd targets ("system/com.example.x") to stop with `launchctl bootout` inside the
    /// SAME administrator command as the moves of `items`, right before they move (a daemon
    /// whose property list is about to go). Asked only about the items that passed every
    /// check, so a skipped item's daemon is left running, and only when
    /// [`Feature::allows_admin`] is on. One run asks for the password once.
    fn admin_stops(&self, _env: &Env, _items: &[&Item]) -> Vec<String> {
        Vec::new()
    }
    /// Check one selected item again, right before anything moves, with the same rules the
    /// scan used to propose it. A saved plan is a plain file that a person or an agent can
    /// edit, so its categories, its locks and its groups are not trusted: only what this
    /// function finds on disk now. `Err` says why the item is refused (it is skipped, the
    /// rest goes on). The allow-list and [`policy::check_move`] run after this anyway.
    fn validate(
        &self,
        _env: &Env,
        _settings: &Settings,
        _plan: &Plan,
        _item: &Item,
    ) -> Result<(), String> {
        Ok(())
    }
    /// Rules about the selection as a whole (Duplicates: one copy of every content stays on
    /// disk), checked after [`Feature::validate`] on the items that passed it. `Err` stops
    /// the whole run: nothing moves.
    fn validate_set(&self, _env: &Env, _settings: &Settings, _plan: &Plan) -> anyhow::Result<()> {
        Ok(())
    }
    /// Work needed right before the confirmed items move, e.g. unloading a launch agent.
    /// Runs only for the confirmed selection; must log what it does to the journal.
    fn prepare(&self, _env: &Env, _plan: &Plan) -> anyhow::Result<()> {
        Ok(())
    }
    /// Work for what really moved, right after the executor (stopping a launch agent whose
    /// property list moved). Items that were skipped are left untouched. Must log what it
    /// does; a failure here is in the journal, the move itself stands.
    fn finish(&self, _env: &Env, _plan: &Plan, _report: &Report) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Carry out a confirmed plan the one way every front end uses: every selected item checked
/// again by its feature ([`Feature::validate`], [`Feature::validate_set`]) and against the
/// ignore list, the feature's allow-list, its method override, its preparation, then the
/// executor. Refused items are reported as skipped and never reach the executor.
pub fn run_plan(
    env: &Env,
    settings: &Settings,
    plan: &Plan,
    confirmed_digest: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> anyhow::Result<Report> {
    let fid = plan
        .feature
        .ok_or_else(|| anyhow::anyhow!("the plan has no feature"))?;
    let f = get(fid).ok_or_else(|| anyhow::anyhow!("{} has no cleanup", fid.title()))?;
    run_plan_with(f.as_ref(), env, settings, plan, confirmed_digest, progress)
}

/// [`run_plan`] with the feature already looked up (Quick Sweep runs several in a row).
/// Same checks: the plan must belong to `f` and match the confirmed digest.
pub fn run_plan_with(
    f: &dyn Feature,
    env: &Env,
    settings: &Settings,
    plan: &Plan,
    confirmed_digest: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> anyhow::Result<Report> {
    if plan.feature != Some(f.id()) {
        anyhow::bail!("this plan does not belong to {}", f.id().title());
    }
    if plan.digest() != confirmed_digest {
        return Err(ExecError::DigestMismatch.into());
    }
    let (checked, refused) = validated(f, env, settings, plan);
    f.validate_set(env, settings, &checked)?;
    let allow = f.allow_roots(env);
    let method = f.method_override().unwrap_or(settings.method);
    f.prepare(env, &checked)?;
    let mut report = executor::execute_stopping(
        env,
        &checked,
        &allow,
        &checked.digest(),
        method,
        f.allows_admin(),
        &|items| f.admin_stops(env, items),
        progress,
    )?;
    // The files moved; what finish could not do is logged in the journal by finish itself.
    let _ = f.finish(env, &checked, &report);
    report.skipped.splice(0..0, refused);
    Ok(report)
}

/// `plan` with every selected item that fails [`Feature::validate`], is on the ignore list or
/// holds an item on it unselected, and those items with the reason.
fn validated(
    f: &dyn Feature,
    env: &Env,
    settings: &Settings,
    plan: &Plan,
) -> (Plan, Vec<(PathBuf, SkipReason)>) {
    let mut checked = plan.clone();
    let mut refused = Vec::new();
    // By position, not by `Item::id`: an edited plan may carry any ids.
    for (n, it) in plan.items.iter().enumerate() {
        if !(it.selected && it.locked.is_none()) {
            continue;
        }
        let inside = policy::ignored_inside(&settings.ignore, &it.path);
        let why = if settings.is_ignored(&it.path) {
            Err("it is on your ignore list".to_string())
        } else if let Some(first) = inside.first() {
            // A folder holding an ignored item never moves as a whole (CHK7-2).
            Err(format!(
                "it holds {}, which is on your ignore list",
                env.display(first)
            ))
        } else {
            f.validate(env, settings, plan, it)
        };
        if let Err(why) = why {
            checked.items[n].selected = false;
            refused.push((it.path.clone(), SkipReason::Refused(why)));
        }
    }
    (checked, refused)
}

/// Keep only the items `only` (item ids) of the plan's confirmed selection: what an agent may
/// do with MCP `execute_plan` `only_items`. It can only narrow the list the user confirmed
/// (the digest was checked on it), never add an item to it: an id that is not selected now is
/// refused and nothing changes.
pub fn narrow_selection(plan: &mut Plan, only: &[usize]) -> Result<(), String> {
    for id in only {
        let ok = plan
            .items
            .iter()
            .any(|i| i.id == *id && i.selected && i.locked.is_none());
        if !ok {
            return Err(format!(
                "Refused: item {id} is not in the confirmed list. only_items can only leave items out."
            ));
        }
    }
    for it in plan.items.iter_mut() {
        if !only.contains(&it.id) {
            it.selected = false;
        }
    }
    Ok(())
}

/// How many selected items of `plan` will move with the administrator password (its feature
/// allows it and the scan marked them). Confirmations say so when this is not zero.
pub fn admin_count(plan: &Plan) -> usize {
    let allowed = plan.feature.and_then(get).is_some_and(|f| f.allows_admin());
    if allowed {
        plan.selected_admin_count()
    } else {
        0
    }
}

/// What a cleanup did, in words: "1.20 MB moved to the Trash". Moving to the Trash or the
/// quarantine frees no space yet (it comes back when the Trash is emptied), so the word
/// "freed" is never used here. The bytes stay in `Report::freed` for the CLI and MCP.
pub fn moved_text<'a>(reports: impl IntoIterator<Item = &'a Report>) -> String {
    let mut bytes = 0;
    let (mut trash, mut quarantine) = (false, false);
    for r in reports {
        bytes += r.freed;
        for m in &r.moved {
            if let crate::executor::ActionKind::Moved { method, .. } = &m.kind {
                match method {
                    Method::Trash => trash = true,
                    Method::Quarantine | Method::AdminQuarantine => quarantine = true,
                }
            }
        }
    }
    let size = crate::fsutil::human_size(bytes);
    match (trash, quarantine) {
        (false, false) => "Nothing moved".into(),
        (true, false) => format!("{size} moved to the Trash"),
        (false, true) => format!("{size} moved to the CleanCraft quarantine"),
        (true, true) => format!("{size} moved to the Trash and the CleanCraft quarantine"),
    }
}

/// The line under [`moved_text`]: when the space really comes back.
pub const SPACE_NOTE: &str = "The space comes back when the Trash is emptied (the quarantine goes to the Trash from Settings).";

/// Every plan-based feature, in sidebar order.
pub fn registry() -> Vec<Box<dyn Feature>> {
    vec![
        Box::new(system_junk::SystemJunk),
        Box::new(large_old::LargeOldFiles),
        Box::new(duplicates::Duplicates),
        Box::new(similar::SimilarImages),
        Box::new(space_lens::SpaceLens),
        Box::new(uninstaller::Uninstaller),
        Box::new(malware::Malware(malware::ThreatDepth::Quick)),
        Box::new(crate::startup::StartupFeature),
        Box::new(mail::MailDownloads),
        Box::new(trash::TrashBins),
        Box::new(privacy::Privacy),
        Box::new(dev_clutter::DevClutter),
    ]
}

pub fn get(id: FeatureId) -> Option<Box<dyn Feature>> {
    registry().into_iter().find(|f| f.id() == id)
}

/// [`get`], with Threat Check looking as deep as `depth` (the others look the same at any depth).
pub fn get_at(id: FeatureId, depth: malware::ThreatDepth) -> Option<Box<dyn Feature>> {
    match id {
        FeatureId::Malware => Some(Box::new(malware::Malware(depth))),
        _ => get(id),
    }
}

/// The allow-list of a feature; empty (nothing may be removed) when the feature has no plans.
pub fn allow_roots(env: &Env, id: FeatureId) -> Vec<PathBuf> {
    get(id).map(|f| f.allow_roots(env)).unwrap_or_default()
}

/// Run a feature's scan the one way every front end uses: then a hard-linked file listed
/// twice counts once ([`Plan::count_links_once`]).
pub fn scan_with(f: &dyn Feature, ctx: &ScanCtx<'_>) -> anyhow::Result<Plan> {
    let mut plan = f.scan(ctx)?;
    if !ctx.cancelled() {
        ctx.meter.complete();
    }
    narrow_ignored(f, ctx.env, ctx.settings, &mut plan);
    lock_linked_roots(f, ctx.env, &mut plan);
    plan.count_links_once();
    Ok(plan)
}

/// Category of the rows that say an allow-list folder is reached through a link.
pub const CAT_LINKED: &str = "linked";

/// CHK7-1 at scan time: an allow-list folder that is a link or lies under one is never
/// cleaned (the fence refuses everything in it), so the scan lists the link as one locked
/// row with the reason and the next step, instead of leaving the person to wonder. Not
/// added when the scan already lists something under that link (locked with its own reason).
fn lock_linked_roots(f: &dyn Feature, env: &Env, plan: &mut Plan) {
    for (link, roots) in policy::linked_roots(env, &f.allow_roots(env)) {
        if plan.items.iter().any(|i| i.path.starts_with(&link.link)) {
            continue;
        }
        plan.add_category(
            CAT_LINKED,
            "Reached through a link",
            "These folders lead to another place through a link. CleanCraft never cleans through a link.",
            crate::plan::Risk::High,
        );
        let size = roots.iter().map(|r| crate::fsutil::tree_size(r).0).sum();
        let id = plan.push(
            link.link.clone(),
            size,
            CAT_LINKED,
            "Reached through a link",
            crate::plan::Risk::High,
            false,
        );
        plan.lock(id, policy::link_lock_reason(env, &link));
    }
}

/// The lock reason of a folder that holds an item on the ignore list.
pub fn ignored_inside_reason(env: &Env, ignored: &Path) -> String {
    format!(
        "It holds {}, which is on your ignore list, so it never moves as a whole. Take it off the ignore list in Settings, or remove the other items yourself in Finder",
        env.display(ignored)
    )
}

/// How deep [`narrow_ignored`] goes to leave an ignored item out.
const NARROW_DEPTH: usize = 8;

/// CHK7-2 at scan time: a folder the scan offers that holds an item on the ignore list is
/// replaced by what is in it, minus the ignored item (folders on the way to it are opened the
/// same way), when the feature accepts those items ([`Feature::validate`] for each,
/// [`Feature::validate_set`] for the whole selection). Otherwise the folder stays listed,
/// locked with the reason. The ignored item itself is never offered.
fn narrow_ignored(f: &dyn Feature, env: &Env, settings: &Settings, plan: &mut Plan) {
    if settings.ignore.is_empty()
        || !plan
            .items
            .iter()
            .any(|i| !policy::ignored_inside(&settings.ignore, &i.path).is_empty())
    {
        return;
    }
    let before = plan.clone();
    let mut items: Vec<Item> = Vec::new();
    let mut narrowed = false;
    for it in &before.items {
        let inside = policy::ignored_inside(&settings.ignore, &it.path);
        if inside.is_empty() {
            items.push(it.clone());
            continue;
        }
        let mut parts = Vec::new();
        let opened = it.locked.is_none()
            && std::fs::symlink_metadata(&it.path).is_ok_and(|m| m.is_dir())
            && split_around(&it.path, &settings.ignore, 0, &mut parts);
        let children: Vec<Item> = parts
            .into_iter()
            .map(|p| {
                let md = std::fs::symlink_metadata(&p).ok();
                Item {
                    size: crate::fsutil::tree_size(&p).0,
                    modified: md
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .or(it.modified),
                    path: p,
                    ..it.clone()
                }
            })
            .collect();
        let accepted = opened
            && !children.is_empty()
            && children
                .iter()
                .all(|c| f.validate(env, settings, &before, c).is_ok());
        if accepted {
            items.extend(children);
            narrowed = true;
        } else {
            let mut locked = it.clone();
            locked.selected = false;
            if locked.locked.is_none() {
                locked.locked = Some(ignored_inside_reason(env, inside[0]));
            }
            items.push(locked);
        }
    }
    let renumber = |items: &mut Vec<Item>| {
        for (n, i) in items.iter_mut().enumerate() {
            i.id = n;
        }
    };
    renumber(&mut items);
    plan.items = items;
    if narrowed && f.validate_set(env, settings, plan).is_err() {
        // The feature does not take the parts as a set: lock the folders instead.
        let mut items: Vec<Item> = Vec::new();
        for it in &before.items {
            let inside = policy::ignored_inside(&settings.ignore, &it.path);
            let mut it = it.clone();
            if let Some(first) = inside.first() {
                it.selected = false;
                if it.locked.is_none() {
                    it.locked = Some(ignored_inside_reason(env, first));
                }
            }
            items.push(it);
        }
        renumber(&mut items);
        plan.items = items;
    }
}

/// What is in `dir` minus the ignored items, opening the folders that hold one. `false` when
/// it cannot be read or the ignored item lies too deep.
fn split_around(dir: &Path, ignore: &[PathBuf], depth: usize, out: &mut Vec<PathBuf>) -> bool {
    if depth >= NARROW_DEPTH {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut kids: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    kids.sort();
    for c in kids {
        if ignore
            .iter()
            .any(|i| policy::same_path_ci(&c, i) || policy::starts_with_ci(&c, i))
        {
            continue;
        }
        if policy::ignored_inside(ignore, &c).is_empty() {
            out.push(c);
        } else if std::fs::symlink_metadata(&c).is_ok_and(|m| m.is_dir()) {
            if !split_around(&c, ignore, depth + 1, out) {
                return false;
            }
        } else {
            return false;
        }
    }
    true
}

/// Scan with a fresh context.
pub fn scan(env: &Env, settings: &Settings, id: FeatureId) -> anyhow::Result<Plan> {
    scan_metered(env, settings, id, Default::default(), &Progress::new())
}

/// [`scan`] at a Threat Check depth, reporting into `meter` (the CLI's text bar).
pub fn scan_metered(
    env: &Env,
    settings: &Settings,
    id: FeatureId,
    depth: malware::ThreatDepth,
    meter: &Progress,
) -> anyhow::Result<Plan> {
    let f = get_at(id, depth).ok_or_else(|| anyhow::anyhow!("{} has no scan", id.title()))?;
    let cancel = AtomicBool::new(false);
    let ctx = ScanCtx {
        env,
        settings,
        cancel: &cancel,
        progress: &|s| meter.set_step(s),
        meter,
    };
    scan_with(f.as_ref(), &ctx)
}
