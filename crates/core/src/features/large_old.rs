//! Large & Old Files: files in the home folder that are big, or that nobody opened for a long
//! time. Grouped by kind (archives, movies, music, pictures, documents, disk images, other).
//! These are the user's own files, so nothing is ever selected for them.
//!
//! Where it looks: the location the user picked ([`ScanOptions`], remembered in
//! `<data>/files.json`): the whole home folder, Downloads, Movies, Music, Pictures, a folder the
//! user typed (inside the home folder, never a protected place), or "Downloads by source app"
//! (see [`super::downloads`]).
//!
//! What it never looks at: `~/Library`, the contents of Documents and Desktop (protected
//! anyway), hidden files and folders (names starting with a dot, which includes the Trash),
//! iCloud and cloud folders, Photos, Music, TV and Photo Booth libraries, app bundles and the
//! contents of packages. A document package (a `.pages` file, a `.sparsebundle`) is proposed as
//! one item.
//!
//! "Large" means at least `settings.large_min_bytes` on disk. "Old" means not opened for
//! `settings.old_days` days, for files of at least [`OLD_MIN_BYTES`] (tiny old files are not
//! worth a look). Last opened = the later of the modification time and the last use: on macOS
//! the Spotlight attribute `kMDItemLastUsedDate` (asked in batches of [`MDLS_BATCH`] files with
//! one `mdls` call, cheap enough), otherwise, or when Spotlight has no answer, the access time.
//! Each item's `modified` holds that last-opened time (seconds since the epoch), so "Sort by
//! date" and the age groups both use it.
//!
//! Grouping ([`GroupBy`], remembered in `<data>/files.json`): by kind (the default), by size
//! (under 50 MB, 50 MB to 1 GB, 1 to 5 GB, over 5 GB; decimal units like Finder) or by age
//! (opened in the last 3 months, not opened in 3 to 6 months, 6 to 12 months, over a year,
//! unknown; a month is 30 days and a year 365). [`regroup`] only rewrites the plan's
//! categories: the items, their sizes and the selection stay as they are (nothing is ever
//! preselected).

use std::collections::HashMap;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::env::{Env, has_parent_component};
use crate::features::{Feature, ScanCtx};
use crate::fsutil::{entry_size, human_size, tree_size};
use crate::plan::{FeatureId, Plan, Risk};
use crate::policy;

pub struct LargeOldFiles;

/// Old files smaller than this are not proposed (1 MB).
pub const OLD_MIN_BYTES: u64 = 1_000_000;
/// Files per `mdls` call.
pub const MDLS_BATCH: usize = 100;

/// Where the file features look.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", content = "path")]
pub enum Location {
    #[default]
    Home,
    Downloads,
    Movies,
    Music,
    Pictures,
    /// Large & Old Files only: every item in Downloads, grouped by the app that saved it.
    DownloadsBySource,
    /// A folder inside the home folder, checked with [`parse_folder`].
    Folder(PathBuf),
}

impl Location {
    /// The preset choices, in the order the picker shows them.
    pub const PRESETS: [Location; 5] = [
        Location::Home,
        Location::Downloads,
        Location::Movies,
        Location::Music,
        Location::Pictures,
    ];

    pub fn label(&self, env: &Env) -> String {
        match self {
            Location::Home => "Home folder".into(),
            Location::Downloads => "Downloads".into(),
            Location::Movies => "Movies".into(),
            Location::Music => "Music".into(),
            Location::Pictures => "Pictures".into(),
            Location::DownloadsBySource => "Downloads by source app".into(),
            Location::Folder(p) => env.display(p),
        }
    }

    /// The folder walked.
    pub fn root(&self, env: &Env) -> PathBuf {
        match self {
            Location::Home => env.home().to_path_buf(),
            Location::Downloads | Location::DownloadsBySource => env.home_path("Downloads"),
            Location::Movies => env.home_path("Movies"),
            Location::Music => env.home_path("Music"),
            Location::Pictures => env.home_path("Pictures"),
            Location::Folder(p) => p.clone(),
        }
    }
}

/// The file features' remembered choices: `<data>/files.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ScanOptions {
    pub large_old: Location,
    pub duplicates: Location,
    /// How Large & Old Files groups its results.
    pub large_old_group: GroupBy,
    /// How deep Threat Check looks.
    pub threat_depth: super::malware::ThreatDepth,
}

/// How Large & Old Files groups its list.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    #[default]
    Kind,
    Size,
    Age,
}

impl GroupBy {
    pub const ALL: [GroupBy; 3] = [GroupBy::Kind, GroupBy::Size, GroupBy::Age];

    pub fn label(self) -> &'static str {
        match self {
            GroupBy::Kind => "Kind",
            GroupBy::Size => "Size",
            GroupBy::Age => "Age",
        }
    }
}

/// 50 MB, 1 GB, 5 GB (decimal, like Finder and [`human_size`]).
pub const SIZE_EDGES: [u64; 3] = [50_000_000, 1_000_000_000, 5_000_000_000];

/// Size groups: (id, title, smallest size included, first size excluded).
pub const SIZE_GROUPS: &[(&str, &str, u64, u64)] = &[
    (
        "size_50mb_1gb",
        "50 MB to 1 GB",
        SIZE_EDGES[0],
        SIZE_EDGES[1],
    ),
    ("size_1gb_5gb", "1 to 5 GB", SIZE_EDGES[1], SIZE_EDGES[2]),
    ("size_over_5gb", "Over 5 GB", SIZE_EDGES[2], u64::MAX),
    ("size_under_50mb", "Under 50 MB", 0, SIZE_EDGES[0]),
];

/// Days in the age groups' months and year.
pub const DAYS_3_MONTHS: i64 = 90;
pub const DAYS_6_MONTHS: i64 = 180;
pub const DAYS_YEAR: i64 = 365;

/// Age groups, oldest first (U7-17): (id, title, fewest days since last opened included,
/// first excluded).
pub const AGE_GROUPS: &[(&str, &str, i64, i64)] = &[
    (
        "age_over_year",
        "Not opened for over a year",
        DAYS_YEAR,
        i64::MAX,
    ),
    (
        "age_6_12_months",
        "Not opened in 6 to 12 months",
        DAYS_6_MONTHS,
        DAYS_YEAR,
    ),
    (
        "age_3_6_months",
        "Not opened in 3 to 6 months",
        DAYS_3_MONTHS,
        DAYS_6_MONTHS,
    ),
    (
        "age_recent",
        "Opened in the last 3 months",
        i64::MIN,
        DAYS_3_MONTHS,
    ),
];

/// Items whose last-opened time is not known.
pub const AGE_UNKNOWN: (&str, &str) = ("age_unknown", "Last opened at an unknown time");

/// The size group of `bytes`.
pub fn size_group(bytes: u64) -> &'static str {
    SIZE_GROUPS
        .iter()
        .find(|(_, _, lo, hi)| bytes >= *lo && bytes < *hi)
        .map(|g| g.0)
        .unwrap_or("size_over_5gb")
}

/// The age group of an item last opened at `opened` (seconds), at time `now` (seconds).
pub fn age_group(opened: Option<i64>, now: i64) -> &'static str {
    let Some(t) = opened else {
        return AGE_UNKNOWN.0;
    };
    let days = (now - t).div_euclid(86_400);
    AGE_GROUPS
        .iter()
        .find(|(_, _, lo, hi)| days >= *lo && days < *hi)
        .map(|g| g.0)
        .unwrap_or(AGE_UNKNOWN.0)
}

/// The kind group of a Large & Old item: [`kind_of`], and a document package counts as a
/// document.
pub fn item_kind(p: &Path) -> &'static str {
    let k = kind_of(p);
    let package = k == "other"
        && ext_in(p, PACKAGE_EXTS)
        && std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    if package { "documents" } else { k }
}

/// Group a Large & Old Files plan by `by` (seconds `now` for the ages). Only the categories
/// change: items, sizes and the selection are kept. A plan of Downloads by source app keeps
/// its own groups.
pub fn regroup(plan: &mut Plan, by: GroupBy, now: i64) {
    if plan.feature != Some(FeatureId::LargeOldFiles) || is_by_source(plan) {
        return;
    }
    const NOTE: &str = "Your own files. Look at each one before you select it.";
    plan.categories.clear();
    match by {
        GroupBy::Kind => {
            for (id, title, _) in KINDS {
                plan.add_category(id, title, NOTE, Risk::High);
            }
        }
        GroupBy::Size => {
            for (id, title, _, _) in SIZE_GROUPS {
                plan.add_category(id, title, NOTE, Risk::High);
            }
        }
        GroupBy::Age => {
            for (id, title, _, _) in AGE_GROUPS {
                plan.add_category(id, title, NOTE, Risk::High);
            }
            plan.add_category(AGE_UNKNOWN.0, AGE_UNKNOWN.1, NOTE, Risk::High);
        }
    }
    for it in &mut plan.items {
        it.category = match by {
            GroupBy::Kind => item_kind(&it.path),
            GroupBy::Size => size_group(it.size),
            GroupBy::Age => age_group(it.modified, now),
        }
        .to_string();
    }
}

/// Which grouping a plan shows now (from its categories).
pub fn grouped_by(plan: &Plan) -> GroupBy {
    let has = |pre: &str| plan.categories.iter().any(|c| c.id.starts_with(pre));
    if has("size_") {
        GroupBy::Size
    } else if has("age_") {
        GroupBy::Age
    } else {
        GroupBy::Kind
    }
}

/// A plan of "Downloads by source app" (grouped by the app that saved each file).
pub fn is_by_source(plan: &Plan) -> bool {
    plan.categories
        .iter()
        .any(|c| c.id.starts_with("src_") || c.id == super::downloads::NO_SOURCE)
}

/// Seconds since the epoch, now.
pub fn now_secs() -> i64 {
    secs(SystemTime::now())
}

impl ScanOptions {
    pub fn path(env: &Env) -> PathBuf {
        env.data_dir().join("files.json")
    }

    pub fn load(env: &Env) -> ScanOptions {
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

    /// The location of a feature (Large & Old Files or Duplicates).
    pub fn location(&self, f: FeatureId) -> &Location {
        match f {
            FeatureId::Duplicates => &self.duplicates,
            _ => &self.large_old,
        }
    }

    pub fn set_location(&mut self, f: FeatureId, loc: Location) {
        match f {
            FeatureId::Duplicates => self.duplicates = loc,
            _ => self.large_old = loc,
        }
    }
}

/// Check a folder the user typed: `~/Projects`, `Projects` (inside the home folder) or an
/// absolute path. Returns the folder or a message for the user.
pub fn parse_folder(env: &Env, input: &str) -> Result<PathBuf, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("Type the path of a folder in your home folder.".into());
    }
    let s = s.trim_end_matches('/');
    let p = if s.is_empty() {
        env.sys_path("/")
    } else if s == "~" {
        env.home().to_path_buf()
    } else if let Some(rest) = s.strip_prefix("~/") {
        env.home_path(rest)
    } else if s.starts_with('/') {
        let p = PathBuf::from(s);
        if env.contains(&p) { p } else { env.sys_path(s) }
    } else {
        env.home_path(s)
    };
    let shown = env.display(&p);
    if has_parent_component(&p) {
        return Err("Write the path without \"..\".".into());
    }
    if !p.starts_with(env.home()) {
        return Err(format!(
            "{shown} is outside your home folder. Choose a folder inside it."
        ));
    }
    if p != env.home()
        && let Some(why) = policy::protected_reason(env, &p)
    {
        return Err(format!(
            "{shown} is protected ({why}). CleanCraft never proposes files there."
        ));
    }
    if policy::starts_with_ci(&p, &env.home_path("Library")) {
        return Err(format!(
            "{shown} is in your Library, which holds app data. Use System Junk or App Remover for it."
        ));
    }
    match std::fs::symlink_metadata(&p) {
        Err(_) => Err(format!("There is no folder at {shown}.")),
        Ok(md) if md.file_type().is_symlink() => Err(format!(
            "{shown} is a link to another place. Choose the folder itself."
        )),
        Ok(md) if !md.is_dir() => Err(format!("{shown} is a file. Choose a folder.")),
        Ok(_) => Ok(p),
    }
}

/// Bundles and libraries that are skipped entirely: apps and the libraries apps manage.
pub const SKIP_EXTS: &[&str] = &[
    "app",
    "photoslibrary",
    "photolibrary",
    "aplibrary",
    "musiclibrary",
    "tvlibrary",
    "photoboothlibrary",
    "imovielibrary",
    "theater",
    "migratedphotolibrary",
];

/// Folders (relative to the home folder) never walked.
pub const SKIP_HOME: &[&str] = &[
    "Library",
    "Documents",
    "Desktop",
    "Applications",
    "Music/Music",
    "Music/iTunes",
    "Pictures/Photo Booth Library",
];

/// Packages: folders macOS shows as one file. Proposed as one item, never opened.
pub const PACKAGE_EXTS: &[&str] = &[
    "pages",
    "numbers",
    "key",
    "rtfd",
    "sparsebundle",
    "fcpbundle",
    "logicx",
    "band",
    "bundle",
    "framework",
    "plugin",
    "xcarchive",
    "playground",
    "dSYM",
];

fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn ext_in(p: &Path, list: &[&str]) -> bool {
    let e = ext_of(p);
    !e.is_empty() && list.iter().any(|x| x.eq_ignore_ascii_case(&e))
}

/// Kinds, in the order the categories are shown: (id, title, extensions).
pub const KINDS: &[(&str, &str, &[&str])] = &[
    (
        "archives",
        "Archives",
        &[
            "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "lz4", "sit", "sitx",
            "cpio", "pkg",
        ],
    ),
    (
        "movies",
        "Movies",
        &[
            "mov", "mp4", "m4v", "mkv", "avi", "wmv", "flv", "webm", "mpg", "mpeg", "3gp", "mts",
        ],
    ),
    (
        "music",
        "Music",
        &[
            "mp3", "m4a", "aac", "wav", "aiff", "aif", "flac", "ogg", "alac", "m4p", "caf", "band",
            "logicx",
        ],
    ),
    (
        "pictures",
        "Pictures",
        &[
            "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "raw",
            "cr2", "cr3", "nef", "arw", "dng", "psd", "svg",
        ],
    ),
    (
        "documents",
        "Documents",
        &[
            "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "pages", "numbers", "key", "rtf",
            "rtfd", "txt", "md", "csv", "epub", "odt", "ods",
        ],
    ),
    (
        "disk_images",
        "Disk images",
        &["dmg", "iso", "img", "sparseimage", "sparsebundle", "toast"],
    ),
    ("other", "Other", &[]),
];

/// The kind category of a file, by extension.
pub fn kind_of(p: &Path) -> &'static str {
    let e = ext_of(p);
    KINDS
        .iter()
        .find(|(_, _, exts)| exts.iter().any(|x| *x == e))
        .map(|(id, _, _)| *id)
        .unwrap_or("other")
}

pub fn kind_title(id: &str) -> &'static str {
    KINDS
        .iter()
        .find(|(k, _, _)| *k == id)
        .map(|(_, t, _)| *t)
        .unwrap_or("Other")
}

/// A file (or package) found by [`walk_area`].
pub struct Found {
    pub path: PathBuf,
    pub md: Metadata,
    /// Bytes on disk (the whole tree for a package).
    pub size: u64,
    pub package: bool,
}

#[derive(Default)]
pub struct Walk {
    pub found: Vec<Found>,
    /// Folders that could not be read.
    pub unreadable: Vec<PathBuf>,
    pub stopped: bool,
}

/// Should this folder or file be left out of the file features' walk?
pub fn skipped(env: &Env, p: &Path) -> bool {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.starts_with('.') {
        return true;
    }
    if SKIP_HOME
        .iter()
        .any(|rel| policy::same_path_ci(p, &env.home_path(rel)))
    {
        return true;
    }
    if ext_in(p, SKIP_EXTS) {
        return true;
    }
    p != env.home() && policy::protected_reason(env, p).is_some()
}

/// Walk `root` the way every file feature does (see the module notes). Files shorter than
/// `min_len` bytes are left out. With `packages`, document packages are returned as one item;
/// without, they are skipped.
pub fn walk_area(
    ctx: &ScanCtx<'_>,
    root: &Path,
    allow: &[PathBuf],
    min_len: u64,
    packages: bool,
) -> Walk {
    let env = ctx.env;
    let mut w = Walk::default();
    // Depth 0 is `root`. Progress: folders right under `root` finished of the ones found
    // there, each refined by its own sub-folders finished. The stack is depth first, so
    // everything inside one top folder is done before the next top folder starts.
    let mut stack = vec![(root.to_path_buf(), 0u8)];
    let mut n = 0u64;
    let mut top = Meter2::default();
    while let Some((dir, depth)) = stack.pop() {
        if ctx.cancelled() {
            w.stopped = true;
            break;
        }
        n += 1;
        if n % 64 == 1 {
            (ctx.progress)(&env.display(&dir));
        }
        top.enter(depth);
        top.report(ctx);
        let before = stack.len();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => {
                w.unreadable.push(dir);
                continue;
            }
        };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries.into_iter().rev() {
            if skipped(env, &p) || ctx.ignored(&p) {
                continue;
            }
            let Ok(md) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if md.is_dir() {
                if ext_in(&p, PACKAGE_EXTS) {
                    if packages && ctx.allowed(allow, &p) {
                        let (size, _) = tree_size(&p);
                        if size >= min_len {
                            w.found.push(Found {
                                path: p,
                                md,
                                size,
                                package: true,
                            });
                        }
                    }
                } else {
                    stack.push((p, depth.saturating_add(1)));
                }
            } else if md.is_file() && md.len() >= min_len && ctx.allowed(allow, &p) {
                w.found.push(Found {
                    size: entry_size(&md),
                    path: p,
                    md,
                    package: false,
                });
            }
        }
        top.found(depth, stack.len() - before);
    }
    w.found.sort_by(|a, b| a.path.cmp(&b.path));
    w
}

/// Two-level folder counting for [`walk_area`]'s progress: `a` of `a_total` top folders
/// entered, `b` of `b_total` sub-folders entered in the current top folder.
#[derive(Default)]
struct Meter2 {
    a: u64,
    a_total: u64,
    b: u64,
    b_total: u64,
}

impl Meter2 {
    /// A folder at `depth` is about to be read.
    fn enter(&mut self, depth: u8) {
        match depth {
            1 => {
                self.a += 1;
                self.b = 0;
                self.b_total = 0;
            }
            2 => self.b += 1,
            _ => {}
        }
    }

    /// The folder at `depth` held `subdirs` folders to walk.
    fn found(&mut self, depth: u8, subdirs: usize) {
        match depth {
            0 => self.a_total = subdirs as u64,
            1 => self.b_total = subdirs as u64,
            _ => {}
        }
    }

    fn report(&self, ctx: &ScanCtx<'_>) {
        if self.a_total == 0 {
            return;
        }
        // The current top folder counts as started, its finished share from its sub-folders.
        let inner = if self.b_total == 0 {
            0.0
        } else {
            self.b.saturating_sub(1) as f64 / self.b_total as f64
        };
        let done = self.a.saturating_sub(1) as f64 + inner;
        ctx.meter.set_fraction(done / self.a_total as f64);
    }
}

fn secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `mdls -raw` output for several files: one value per file, separated by NUL bytes,
/// `(null)` when Spotlight has none. Returns None when the count does not match.
pub fn parse_mdls_raw(out: &str, count: usize) -> Option<Vec<Option<SystemTime>>> {
    let vals: Vec<&str> = out.split('\0').collect();
    let vals: Vec<&str> =
        if vals.len() == count + 1 && vals.last().is_some_and(|v| v.trim().is_empty()) {
            vals[..count].to_vec()
        } else {
            vals
        };
    if vals.len() != count {
        return None;
    }
    Some(vals.into_iter().map(parse_mdls_date).collect())
}

/// "2024-03-01 10:22:33 +0000" -> time.
pub fn parse_mdls_date(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    if s.is_empty() || s == "(null)" {
        return None;
    }
    let dt = chrono::DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S %z").ok()?;
    let t = dt.timestamp();
    if t < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(t as u64))
}

/// Spotlight's last-used date for each file, when macOS knows it.
pub fn last_used_dates(env: &Env, paths: &[PathBuf]) -> HashMap<PathBuf, SystemTime> {
    let mut out = HashMap::new();
    if !env.sys.is_macos() {
        return out;
    }
    for chunk in paths.chunks(MDLS_BATCH) {
        let names: Vec<String> = chunk
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let mut args = vec!["-raw", "-name", "kMDItemLastUsedDate"];
        args.extend(names.iter().map(|s| s.as_str()));
        let Ok(res) = env.sys.run("/usr/bin/mdls", &args) else {
            // No Spotlight answer at all: the access times are used.
            return out;
        };
        if !res.ok() {
            continue;
        }
        if let Some(vals) = parse_mdls_raw(&res.stdout, chunk.len()) {
            for (p, v) in chunk.iter().zip(vals) {
                if let Some(t) = v {
                    out.insert(p.clone(), t);
                }
            }
        }
    }
    out
}

/// When the file was last opened or changed, as best known.
pub fn last_opened(md: &Metadata, last_used: Option<SystemTime>) -> Option<SystemTime> {
    let m = md.modified().ok();
    let other = last_used.or_else(|| md.accessed().ok());
    match (m, other) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

fn days(now: SystemTime, t: Option<SystemTime>) -> Option<u64> {
    t.and_then(|t| now.duration_since(t).ok())
        .map(|d| d.as_secs() / 86_400)
}

/// Calendar days since `t` (today, yesterday...), for the "last opened" words.
fn calendar_days(now: SystemTime, t: Option<SystemTime>) -> Option<u64> {
    let secs = |x: SystemTime| {
        x.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as i64)
    };
    let (t, now) = (secs(t?)?, secs(now)?);
    (t <= now).then(|| crate::fsutil::calendar_days_ago(t, now) as u64)
}

fn ago(d: u64) -> String {
    match d {
        0 => "today".into(),
        1 => "yesterday".into(),
        n => format!("{n} days ago"),
    }
}

/// The scan's rules for one item of Large & Old Files, Downloads by source app, Duplicates and
/// Similar Images, checked again before it moves (an edited plan may name anything in the
/// home folder): inside the home folder; no folder on the way is skipped by the walk
/// ([`skipped`]: Library, Documents, Desktop, Applications, the Music and Photo Booth
/// libraries, hidden names, apps and media libraries, protected places); and it is a file, a
/// link, a document package, or (Downloads by source app) an entry at the top of Downloads.
pub fn validate_path(env: &Env, p: &Path) -> Result<(), String> {
    let home = env.home();
    if !p.starts_with(home) || p == home || has_parent_component(p) {
        return Err("not in your home folder".into());
    }
    let mut cur = Some(p);
    while let Some(c) = cur {
        if c == home {
            break;
        }
        if skipped(env, c) {
            return Err(format!(
                "{} is never cleaned by this feature (Library, Documents, Desktop, apps, media libraries and hidden items are left alone)",
                env.display(c)
            ));
        }
        cur = c.parent();
    }
    let md = std::fs::symlink_metadata(p).map_err(|_| "it is no longer there".to_string())?;
    let top_of_downloads = p
        .parent()
        .is_some_and(|d| policy::same_path_ci(d, &env.home_path("Downloads")));
    if md.is_dir() && !ext_in(p, PACKAGE_EXTS) && !top_of_downloads {
        return Err("a folder: this feature only removes files and document packages".into());
    }
    Ok(())
}

impl Feature for LargeOldFiles {
    fn id(&self) -> FeatureId {
        FeatureId::LargeOldFiles
    }

    fn allow_roots(&self, env: &Env) -> Vec<PathBuf> {
        vec![env.home().to_path_buf()]
    }

    fn scan(&self, ctx: &ScanCtx<'_>) -> anyhow::Result<Plan> {
        let loc = ScanOptions::load(ctx.env).large_old;
        scan_location(ctx, &loc)
    }

    fn validate(
        &self,
        env: &Env,
        _settings: &crate::settings::Settings,
        _plan: &Plan,
        item: &crate::plan::Item,
    ) -> Result<(), String> {
        validate_path(env, &item.path)
    }
}

/// Large & Old Files in one location.
pub fn scan_location(ctx: &ScanCtx<'_>, loc: &Location) -> anyhow::Result<Plan> {
    if *loc == Location::DownloadsBySource {
        return super::downloads::scan(ctx);
    }
    let env = ctx.env;
    let s = ctx.settings;
    let allow = LargeOldFiles.allow_roots(env);
    let mut plan = Plan::new(FeatureId::LargeOldFiles);
    for (id, title, _) in KINDS {
        plan.add_category(
            id,
            title,
            "Your own files. Look at each one before you select it.",
            Risk::High,
        );
    }
    let root = loc.root(env);
    if let Location::Folder(p) = loc
        && let Err(e) = parse_folder(env, &p.to_string_lossy())
    {
        plan.notes.push(e);
        return Ok(plan);
    }
    if !root.is_dir() {
        plan.notes
            .push(format!("{} does not exist.", env.display(&root)));
        return Ok(plan);
    }
    let min = s.large_min_bytes.min(OLD_MIN_BYTES);
    // The walk is nearly all the work; reading last-used dates is the rest.
    let w = walk_area(
        &ctx.with_meter(&ctx.meter.range(0.0, 0.9)),
        &root,
        &allow,
        min,
        true,
    );
    let candidates: Vec<PathBuf> = w.found.iter().map(|f| f.path.clone()).collect();
    let used = last_used_dates(env, &candidates);
    let now = SystemTime::now();
    for f in &w.found {
        let last = last_opened(&f.md, used.get(&f.path).copied());
        let opened = days(now, last);
        let large = f.size >= s.large_min_bytes;
        let old = f.size >= OLD_MIN_BYTES && opened.is_some_and(|d| d >= s.old_days);
        if !large && !old {
            continue;
        }
        let when = calendar_days(now, last)
            .map(ago)
            .unwrap_or_else(|| "at an unknown time".into());
        let reason = match (large, old) {
            (true, true) => format!(
                "Large ({}) and not opened for {} days",
                human_size(f.size),
                opened.unwrap_or(0)
            ),
            (true, false) => format!("Large ({}), last opened {when}", human_size(f.size)),
            _ => format!("Not opened for {} days", opened.unwrap_or(0)),
        };
        let cat = if f.package && kind_of(&f.path) == "other" {
            "documents"
        } else {
            kind_of(&f.path)
        };
        let id = plan.push(f.path.clone(), f.size, cat, reason, Risk::High, false);
        // The last-opened time: sorting by date and the age groups use it.
        plan.items[id].modified = last.map(secs);
    }
    if !w.unreadable.is_empty() {
        plan.notes.push(format!(
            "{} folders could not be read and were skipped (first: {}).",
            w.unreadable.len(),
            env.display(&w.unreadable[0])
        ));
    }
    if w.stopped {
        plan.notes.push("Scan stopped before the end.".into());
    }
    regroup(&mut plan, ScanOptions::load(env).large_old_group, secs(now));
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdls_values() {
        let out = "2024-03-01 10:22:33 +0000\0(null)\x002020-01-01 00:00:00 +0100";
        let v = parse_mdls_raw(out, 3).unwrap();
        assert!(v[0].is_some() && v[1].is_none() && v[2].is_some());
        assert!(parse_mdls_raw(out, 2).is_none());
        assert_eq!(
            parse_mdls_date("1970-01-02 00:00:00 +0000"),
            Some(UNIX_EPOCH + Duration::from_secs(86_400))
        );
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_of(Path::new("/a/B.ZIP")), "archives");
        assert_eq!(kind_of(Path::new("/a/movie.mkv")), "movies");
        assert_eq!(kind_of(Path::new("/a/x.dmg")), "disk_images");
        assert_eq!(kind_of(Path::new("/a/noext")), "other");
    }
}
