//! The click-every-control test (FEATURES S12).
//!
//! Every sidebar page is opened on a fake Mac with data for it (the builders' `testkit_*`
//! helpers, FakeSys answering the tools each page asks). From the page's first state the test
//! explores every state a user can reach by clicking (scanned results, popups, confirm
//! dialogs, done pages, tabs, groups, app details), breadth first. In each state it lists every
//! control (button, checkbox, radio button, combo box, link, text box, slider) and, for every
//! control that state shows for the first time, it rebuilds the state from scratch, clicks it
//! (text boxes: types into them) and fails when:
//! - the control lies outside the window even after scrolling it into view;
//! - nothing happened: `CleanApp::action_count()` did not grow and the accessibility tree
//!   (labels, values, check states) did not change;
//! - the app panicked;
//! - any text on screen says "not available", "coming soon", "not implemented", "TODO",
//!   contains an em dash, or uses the conjunction the house rules ban (docs/agents/STANDARD.md, rule 7).
//!
//! Controls shared by every page (sidebar rows, the Full Disk Access banner, the notice's
//! Dismiss button) are tested once in `shell_controls_respond`. A control a state inherits from
//! the state before it (the same role, label and position among equal labels) was already
//! clicked there; the table lists it as inherited.
//!
//! Run with `--nocapture` to see the table of every control per page.

mod common;

use std::collections::{BTreeSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use cleancraft_core::executor::{self, ActionKind, ActionRecord};
use cleancraft_core::permissions::Access;
use cleancraft_core::sys::CmdOut;
use cleancraft_core::testkit::{FakeMac, age};
use cleancraft_core::testkit_apps::{AppSpec, FakeMacApps, appcast};
use cleancraft_core::testkit_files::{ImageFormat, save_scene};
use cleancraft_core::{FeatureId, Settings, features, testkit_cleanup, testkit_monitor};
use cleancraft_core::{testkit_perf as tp, updater};
use common::*;
use egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};

// ---------------------------------------------------------------------------------------
// Fake Macs, one per page, each with something for every control to act on.
// ---------------------------------------------------------------------------------------

fn base_answers(m: &FakeMac) {
    let f = m.env.fake_sys().expect("fake sys");
    // Reveal in Finder, hand-offs to other apps.
    f.answer("/usr/bin/open", "");
    // Undo of a toggled setting runs its undo command through the shell.
    f.answer("sh", "");
}

fn mac_sweep() -> FakeMac {
    let m = testkit_monitor::sweep_mac();
    testkit_monitor::answer_all(m.env.fake_sys().unwrap());
    base_answers(&m);
    m
}

fn mac_junk() -> FakeMac {
    let m = testkit_cleanup::junk_mac();
    base_answers(&m);
    m
}

fn mac_mail() -> FakeMac {
    let m = testkit_cleanup::mail_mac();
    base_answers(&m);
    m
}

const EMPTY_CALL: &str = "/usr/bin/osascript -e tell application \"Finder\" to empty trash";

fn mac_trash() -> FakeMac {
    let m = testkit_cleanup::trash_mac();
    base_answers(&m);
    m.env.fake_sys().unwrap().answer(EMPTY_CALL, "");
    m
}

fn mac_privacy() -> FakeMac {
    let m = testkit_cleanup::privacy_mac();
    base_answers(&m);
    m.env
        .fake_sys()
        .unwrap()
        .answer("tccutil", "Successfully reset\n");
    m
}

fn mac_startup() -> FakeMac {
    let m = tp::startup_mac();
    base_answers(&m);
    m
}

fn mac_tasks() -> FakeMac {
    let m = FakeMac::new();
    let f = m.env.fake_sys().unwrap();
    f.answer("id -u", "501\n");
    f.answer("sw_vers -productVersion", "14.6.1\n");
    tp::mail_index(&m, 300_000);
    base_answers(&m);
    m
}

fn mac_remover() -> FakeMac {
    let mac = FakeMac::new();
    let editor = mac.app_spec(&AppSpec {
        size: 3_000_000,
        ..AppSpec::new("Editor", "com.example.Editor", "2.0")
    });
    let viewer = mac.app_spec(&AppSpec {
        mas: true,
        ..AppSpec::new("Viewer", "com.maker.Viewer", "1.0")
    });
    mac.app_spec(&AppSpec {
        dir: "/Applications/Setapp",
        ..AppSpec::new("Tidy", "com.setapp.Tidy", "1.2")
    });
    mac.app("Safari", "com.apple.Safari", "18.0");
    mac.last_opened(&editor, 2);
    mac.last_opened(&viewer, 300);
    mac.spotlight_finds_nothing();
    mac.home_file(
        "Library/Application Support/com.example.Editor/db.sqlite",
        400_000,
    );
    mac.home_file("Library/Caches/com.example.Editor/blob", 900_000);
    mac.home_file("Library/Preferences/com.example.Editor.plist", 3_000);
    mac.home_file("Library/Logs/Editor/run.log", 5_000);
    mac.home_file("Library/Caches/com.gone.Writer/c", 60_000);
    mac.home_file("Downloads/Tool-1.0.pkg", 80_000);
    mac.home_file("Library/PreferencePanes/Thing.prefPane/Contents/x", 20_000);
    base_answers(&mac);
    mac
}

const CAST: &str = "https://example.com/editor/cast.xml";
const DOWN: &str = "https://down.example.org/cast.xml";

fn mac_updates() -> FakeMac {
    let mac = FakeMac::new();
    mac.app_spec(&AppSpec {
        build: "200",
        feed: Some(CAST),
        ..AppSpec::new("Editor", "com.example.Editor", "2.0")
    });
    mac.app_spec(&AppSpec {
        mas: true,
        ..AppSpec::new("Notes Pro", "com.maker.NotesPro", "3.1")
    });
    mac.app_spec(&AppSpec {
        build: "9",
        feed: Some(DOWN),
        ..AppSpec::new("Viewer", "org.example.Viewer", "0.9")
    });
    mac.web(
        CAST,
        &appcast(&[("210", "2.1", "<ul><li>Faster search</li></ul>")]),
    );
    mac.web(
        &updater::lookup_url("com.maker.NotesPro"),
        r#"{"resultCount":1,"results":[{"version":"3.2","releaseNotes":"Sync fixes"}]}"#,
    );
    mac.web_fails(DOWN, 6);
    mac.env.fake_sys().unwrap().answer(
        "/usr/sbin/softwareupdate --list",
        "* Label: macOS 15.1-24B83\n\tTitle: macOS Sequoia 15.1, Version: 15.1, Size: 2000000KiB, Recommended: YES, Action: restart,\n",
    );
    base_answers(&mac);
    mac
}

/// Large and old files, duplicate sets, look-alike pictures and a disk to map.
fn mac_files() -> FakeMac {
    let m = FakeMac::sample();
    Settings {
        large_min_bytes: 2_000_000,
        old_days: 30,
        ..Default::default()
    }
    .save(&m.env)
    .expect("save settings");
    m.home_file("Downloads/big.zip", 2_500_000);
    let old = m.home_file("Downloads/old.pdf", 1_200_000);
    age(&old, 400);
    m.home_file("Movies/clip.mov", 3_000_000);
    let dup: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(7919) >> 3) as u8)
        .collect();
    for p in ["Downloads/a.bin", "Music/old/a.bin", "Projects/a.bin"] {
        m.write(&m.env.home_path(p), &dup);
    }
    let pics = m.env.home_path("Pictures");
    save_scene(&pics.join("a.png"), 320, 240, 0, 0, 1, ImageFormat::Png);
    save_scene(
        &pics.join("a-bright.png"),
        400,
        300,
        0,
        5,
        3,
        ImageFormat::Png,
    );
    save_scene(&pics.join("other.png"), 320, 240, 1, 0, 5, ImageFormat::Png);
    m.env.fake_sys().unwrap().answer(
        "df",
        "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s1 488245288 300000000 188245288 62% /\n",
    );
    base_answers(&m);
    m
}

fn mac_monitor() -> FakeMac {
    let m = FakeMac::new();
    testkit_monitor::answer_all(m.env.fake_sys().unwrap());
    m.home_file(".Trash/old.zip", 2_500_000);
    base_answers(&m);
    m
}

/// A Mac with history: a System Junk cleanup that really ran, a maintenance command and a
/// startup item turned off.
fn mac_history() -> FakeMac {
    let m = FakeMac::sample();
    base_answers(&m);
    let settings = Settings::default();
    let plan = features::scan(&m.env, &settings, FeatureId::SystemJunk).expect("junk scan");
    let rep = features::run_plan(&m.env, &settings, &plan, &plan.digest(), &mut |_, _| {})
        .expect("junk clean");
    assert!(!rep.moved.is_empty(), "history needs moved items");
    let now = chrono::Utc::now().timestamp();
    executor::append_journal(
        &m.env,
        &ActionRecord {
            id: "cmd-1".into(),
            plan_id: "task-1".into(),
            feature: Some(FeatureId::Maintenance),
            time: now,
            size: 0,
            kind: ActionKind::Command {
                command: "dscacheutil -flushcache".into(),
                ok: true,
                output: String::new(),
            },
            undone: None,
        },
    )
    .expect("journal");
    executor::append_journal(
        &m.env,
        &ActionRecord {
            id: "tog-1".into(),
            plan_id: "startup-1".into(),
            feature: Some(FeatureId::LoginItems),
            time: now,
            size: 0,
            kind: ActionKind::Toggled {
                what: "Turned off com.example.helper".into(),
                undo_command: Some("launchctl enable gui/501/com.example.helper".into()),
            },
            undone: None,
        },
    )
    .expect("journal");
    m
}

fn mac_settings() -> FakeMac {
    let m = FakeMac::new();
    let s = Settings {
        ignore: vec![
            m.env.home_path("Library/Caches/com.example.Keep"),
            m.env.home_path("Downloads/keep"),
        ],
        ..Settings::default()
    };
    s.save(&m.env).expect("save settings");
    base_answers(&m);
    m
}

// ---------------------------------------------------------------------------------------
// The accessibility tree as plain data.
// ---------------------------------------------------------------------------------------

const CLICKABLE: &[Role] = &[
    Role::Button,
    Role::CheckBox,
    Role::Switch,
    Role::RadioButton,
    Role::ComboBox,
    Role::Link,
    Role::TextInput,
    Role::MultilineTextInput,
    Role::SearchInput,
    Role::Slider,
    Role::SpinButton,
];

/// The sidebar is the left panel, 232 points wide.
const SIDEBAR_RIGHT: f32 = cleancraft_ui_egui::theme::space::SIDEBAR;
/// Shell controls tested once by `shell_controls_respond`, not on every page.
const SHELL_LABELS: &[&str] = &["Dismiss"];

#[derive(Clone, Debug, PartialEq)]
struct N {
    role: Role,
    label: String,
    value: String,
    toggled: String,
    disabled: bool,
    rect: Option<egui::Rect>,
    /// Inside the top modal dialog, or no dialog is open.
    interactive: bool,
    /// Position in the tree walk, to find the kittest node again.
    idx: usize,
}

fn nodes(h: &H) -> Vec<N> {
    let modal = h.ctx.memory(|m| m.top_modal_layer());
    let modal_id = modal.map(|l| l.id.accesskit_id());
    let ppp = h.ctx.pixels_per_point();
    let mut out = Vec::new();
    for (idx, n) in h.root().children_recursive().enumerate() {
        let a = n.accesskit_node();
        if a.is_hidden() {
            continue;
        }
        let interactive = match modal_id {
            None => true,
            Some(id) => {
                let mut cur = Some(n);
                let mut inside = false;
                while let Some(c) = cur {
                    if c.accesskit_node().locate().0 == id {
                        inside = true;
                        break;
                    }
                    cur = c.parent();
                }
                inside
            }
        };
        out.push(N {
            role: a.role(),
            label: a.label().unwrap_or_default(),
            value: a.value().unwrap_or_default(),
            toggled: a.toggled().map(|t| format!("{t:?}")).unwrap_or_default(),
            disabled: a.is_disabled(),
            rect: a.bounding_box().map(|r| {
                egui::Rect::from_min_max(
                    egui::pos2(r.x0 as f32 / ppp, r.y0 as f32 / ppp),
                    egui::pos2(r.x1 as f32 / ppp, r.y1 as f32 / ppp),
                )
            }),
            interactive,
            idx,
        });
    }
    out
}

/// One control: its role, label and position among controls with the same role and label.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Key {
    role: String,
    label: String,
    nth: usize,
}

impl Key {
    fn show(&self) -> String {
        let label = if self.label.is_empty() {
            "(no label)"
        } else {
            &self.label
        };
        if self.nth > 0 {
            format!("{} '{}' #{}", self.role, label, self.nth + 1)
        } else {
            format!("{} '{}'", self.role, label)
        }
    }
}

fn role_name(r: Role) -> String {
    match r {
        Role::Button => "Button".into(),
        Role::CheckBox => "CheckBox".into(),
        Role::RadioButton => "Radio".into(),
        Role::ComboBox => "ComboBox".into(),
        Role::Link => "Link".into(),
        Role::TextInput | Role::MultilineTextInput | Role::SearchInput => "TextInput".into(),
        Role::Slider => "Slider".into(),
        Role::SpinButton => "Spin".into(),
        other => format!("{other:?}"),
    }
}

/// The page's own controls (not the sidebar, not shell controls), in tree order, with keys.
fn controls(ns: &[N], with_sidebar: bool) -> Vec<(Key, N)> {
    let mut out: Vec<(Key, N)> = Vec::new();
    for n in ns {
        if !CLICKABLE.contains(&n.role) || !n.interactive {
            continue;
        }
        let in_sidebar = n.rect.is_some_and(|r| r.max.x <= SIDEBAR_RIGHT + 0.5);
        if in_sidebar != with_sidebar {
            continue;
        }
        // The section switcher's segments (one per tool) are shell controls too: they are
        // clicked in `shell_controls_respond`, and a page's exploration stays on its tool.
        if !with_sidebar && (SHELL_LABELS.contains(&n.label.as_str()) || is_tool_title(&n.label)) {
            continue;
        }
        let role = role_name(n.role);
        let nth = out
            .iter()
            .filter(|(k, _)| k.role == role && k.label == n.label)
            .count();
        out.push((
            Key {
                role,
                label: n.label.clone(),
                nth,
            },
            n.clone(),
        ));
    }
    out
}

/// What a state looks like for exploration: the set of its controls (role and label). Values
/// and check marks are left out, so ticking an item or typing does not make a new state.
fn signature(ctls: &[(Key, N)]) -> BTreeSet<(String, String)> {
    ctls.iter()
        .map(|(k, _)| (k.role.clone(), k.label.clone()))
        .collect()
}

/// Everything the tree says, for "did anything change".
fn tree_text(ns: &[N]) -> Vec<(Role, String, String, String, bool)> {
    ns.iter()
        .map(|n| {
            (
                n.role,
                n.label.clone(),
                n.value.clone(),
                n.toggled.clone(),
                n.disabled,
            )
        })
        .collect()
}

const BANNED: &[&str] = &[
    "not available",
    "coming soon",
    "not implemented",
    "TODO",
    "\u{2014}",
];

/// Texts on screen that break the rules.
fn bad_texts(ns: &[N]) -> Vec<String> {
    let mut bad = Vec::new();
    for n in ns {
        for s in [&n.label, &n.value] {
            if s.is_empty() {
                continue;
            }
            let lower = s.to_lowercase();
            for b in BANNED {
                let hit = if b.chars().all(|c| c.is_ascii_uppercase()) {
                    s.contains(b)
                } else {
                    lower.contains(&b.to_lowercase())
                };
                if hit {
                    bad.push(format!("{b:?} in {s:?}"));
                }
            }
            if lower
                .split(|c: char| !c.is_alphanumeric())
                .any(|w| w == concat!("n", "or"))
            {
                bad.push(format!("the banned conjunction in {s:?}"));
            }
        }
    }
    bad
}

// ---------------------------------------------------------------------------------------
// Driving the app.
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Step {
    /// Click a control (text boxes: type into them); true: scroll it into view first.
    Use(Key, bool),
}

thread_local! {
    /// The window size this test thread uses: the default 1280 x 820, or the app's smallest
    /// window (1060 x 680, `with_min_inner_size` in apps/cleancraft/src/main.rs).
    static WINDOW: std::cell::Cell<(f32, f32)> = const { std::cell::Cell::new((W, HGT)) };
}

const MIN_WINDOW: (f32, f32) = (1060.0, 680.0);

fn inside_window(r: egui::Rect) -> bool {
    let (w, hgt) = WINDOW.with(|c| c.get());
    r.min.x >= -0.5 && r.min.y >= -0.5 && r.max.x <= w + 0.5 && r.max.y <= hgt + 0.5
}

/// The app on `mac` at this thread's window size.
fn sized_harness(mac: &FakeMac) -> H {
    let (w, hgt) = WINDOW.with(|c| c.get());
    if (w, hgt) == (W, HGT) {
        return harness(mac);
    }
    let app = cleancraft_ui_egui::CleanApp::new(mac.env.clone()).with_sync();
    let mut h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(w, hgt))
        .build_ui_state(
            |ui, app: &mut cleancraft_ui_egui::CleanApp| {
                app.logic(&ui.ctx().clone());
                app.ui(ui);
            },
            app,
        );
    steps(&mut h);
    h
}

/// Find a control by key, scroll it into view, check it is inside the window, use it.
fn use_control(h: &mut H, key: &Key, with_sidebar: bool) -> Result<(), String> {
    use_control_with(h, key, with_sidebar, true)
}

/// `scroll`: bring the control into view first, like a user scrolling a list to it. Without
/// it the control is clicked where it is drawn (when that is inside the window); scrolling a
/// long list changes which rows exist, so the test tries in place first.
fn use_control_with(h: &mut H, key: &Key, with_sidebar: bool, scroll: bool) -> Result<(), String> {
    let find = |h: &H| {
        controls(&nodes(h), with_sidebar)
            .into_iter()
            .find(|(k, _)| k == key)
    };
    let Some((_, n)) = find(h) else {
        return Err(format!("{} is gone", key.show()));
    };
    if scroll || !n.rect.is_some_and(inside_window) {
        scroll_to(h, key, with_sidebar);
    }
    let key = &match find(h) {
        Some(_) => key.clone(),
        None => {
            // A long list draws only the rows in view, so after scrolling the same control can
            // have another position among equal labels: take the last one in view.
            let same: Vec<Key> = controls(&nodes(h), with_sidebar)
                .into_iter()
                .filter(|(k, n)| {
                    k.role == key.role && k.label == key.label && n.rect.is_some_and(inside_window)
                })
                .map(|(k, _)| k)
                .collect();
            match same.last() {
                Some(k) => k.clone(),
                None => {
                    let shown: Vec<String> = controls(&nodes(h), with_sidebar)
                        .iter()
                        .map(|(k, n)| format!("{} {:?}", k.show(), n.rect))
                        .collect();
                    return Err(format!(
                        "{} is gone after scrolling; shown: {}",
                        key.show(),
                        shown.join(", ")
                    ));
                }
            }
        }
    };
    let find = |h: &H| {
        controls(&nodes(h), with_sidebar)
            .into_iter()
            .find(|(k, _)| k == key)
    };
    let Some((_, n)) = find(h) else {
        return Err(format!("{} is gone after scrolling", key.show()));
    };
    match n.rect {
        Some(r) if inside_window(r) => {}
        r => return Err(format!("{} is outside the window: {r:?}", key.show())),
    }
    let is_text = key.role == "TextInput";
    with_node(h, key, with_sidebar, |node| {
        if is_text {
            node.focus();
            node.type_text("zz");
        } else {
            node.click();
        }
    });
    steps(h);
    Ok(())
}

fn scroll_to(h: &mut H, key: &Key, with_sidebar: bool) {
    with_node(h, key, with_sidebar, |n| n.scroll_to_me());
    steps(h);
}

/// Run `f` on the kittest node of `key`.
fn with_node(h: &H, key: &Key, with_sidebar: bool, f: impl FnOnce(&egui_kittest::Node<'_>)) {
    let Some((_, n)) = controls(&nodes(h), with_sidebar)
        .into_iter()
        .find(|(k, _)| k == key)
    else {
        return;
    };
    if let Some(node) = h.root().children_recursive().nth(n.idx) {
        f(&node);
    }
}

/// A tool's name (a segment of a section switcher).
fn is_tool_title(label: &str) -> bool {
    cleancraft_ui_egui::app::GROUPS
        .iter()
        .any(|(_, fs)| fs.len() > 1 && fs.iter().any(|f| f.title() == label))
}

/// Open a page like a user: its sidebar entry, then (a section with several tools) its
/// segment in the switcher at the top of the page.
fn nav(h: &mut H, title: &str) -> Result<(), String> {
    let groups = cleancraft_ui_egui::app::GROUPS;
    let section = groups
        .iter()
        .find(|(_, fs)| fs.len() > 1 && fs.iter().any(|f| f.title() == title))
        .map(|(name, _)| *name);
    let row = Key {
        role: "Button".into(),
        label: section.unwrap_or(title).into(),
        nth: 0,
    };
    use_control(h, &row, true)?;
    if section.is_some() {
        let segment = Key {
            role: "Radio".into(),
            label: title.into(),
            nth: 0,
        };
        click_segment(h, &segment)?;
    }
    Ok(())
}

/// Click a switcher segment (page area, kept out of `controls`).
fn click_segment(h: &mut H, key: &Key) -> Result<(), String> {
    let found = h
        .root()
        .children_recursive()
        .find(|n| {
            let a = n.accesskit_node();
            role_name(a.role()) == key.role && a.label().unwrap_or_default() == key.label
        })
        .is_some();
    if !found {
        return Err(format!("{}: no such segment", key.label));
    }
    h.get_by_label(&key.label).click();
    steps(h);
    Ok(())
}

struct PageSpec {
    title: &'static str,
    mac: fn() -> FakeMac,
    /// Run on every copy of the Mac: faults are not part of the copy.
    post: fn(&FakeMac),
}

fn no_post(_: &FakeMac) {}

fn lock_system(m: &FakeMac) {
    tp::lock_system_folders(m);
}

/// One file system entry of the template Mac, with its times taken once at creation.
struct Entry {
    rel: PathBuf,
    kind: Kind,
    accessed: SystemTime,
    modified: SystemTime,
    perms: std::fs::Permissions,
}

enum Kind {
    Dir,
    File,
    Link(PathBuf),
}

/// A page's fake Mac, built once; each state starts from a fresh copy of it (building the Mac
/// from scratch for every click would take most of the test's time).
struct Template {
    mac: FakeMac,
    entries: Vec<Entry>,
    post: fn(&FakeMac),
}

impl Template {
    fn new(p: &PageSpec) -> Template {
        let mac = (p.mac)();
        let mut entries = Vec::new();
        walk(mac.dir.path(), Path::new(""), &mut entries);
        Template {
            mac,
            entries,
            post: p.post,
        }
    }

    /// A new fake Mac with the same files, times, tool answers and running apps.
    fn copy(&self) -> FakeMac {
        let m = FakeMac::new();
        let from_root = self.mac.dir.path();
        let to_root = m.dir.path();
        let from = from_root.to_string_lossy().into_owned();
        let to = to_root.to_string_lossy().into_owned();
        let remap = |s: &str| s.replace(&from, &to);
        for e in &self.entries {
            let dst = to_root.join(&e.rel);
            match &e.kind {
                Kind::Dir => {
                    let _ = std::fs::create_dir_all(&dst);
                }
                Kind::File => {
                    let _ = std::fs::copy(from_root.join(&e.rel), &dst);
                }
                Kind::Link(target) => {
                    let t = PathBuf::from(remap(&target.to_string_lossy()));
                    let _ = std::os::unix::fs::symlink(t, &dst);
                }
            }
        }
        // Times last, deepest first, so creating children does not change a folder's time.
        for e in self.entries.iter().rev() {
            if matches!(e.kind, Kind::Link(_)) {
                continue;
            }
            let dst = to_root.join(&e.rel);
            let _ = std::fs::set_permissions(&dst, e.perms.clone());
            let f = match e.kind {
                Kind::Dir => std::fs::File::open(&dst),
                _ => std::fs::File::options().write(true).open(&dst),
            };
            if let Ok(f) = f {
                let _ = f.set_times(
                    std::fs::FileTimes::new()
                        .set_accessed(e.accessed)
                        .set_modified(e.modified),
                );
            }
        }
        let src = self.mac.env.fake_sys().expect("fake sys");
        let dst = m.env.fake_sys().expect("fake sys");
        let (answers, open, running, free, cancelled, macos) = src.with(|s| {
            (
                s.answers.clone(),
                s.open_files.clone(),
                s.running.clone(),
                s.free_space,
                s.admin_cancelled,
                s.macos,
            )
        });
        dst.with(|d| {
            d.answers = answers
                .into_iter()
                .map(|(k, v)| {
                    (
                        remap(&k),
                        CmdOut {
                            status: v.status,
                            stdout: remap(&v.stdout),
                            stderr: remap(&v.stderr),
                        },
                    )
                })
                .collect();
            d.open_files = open
                .into_iter()
                .map(|p| PathBuf::from(remap(&p.to_string_lossy())))
                .collect();
            d.running = running
                .into_iter()
                .map(|mut a| {
                    a.path = a.path.map(|p| PathBuf::from(remap(&p.to_string_lossy())));
                    a
                })
                .collect();
            d.free_space = free;
            d.admin_cancelled = cancelled;
            d.macos = macos;
        });
        (self.post)(&m);
        m
    }
}

fn walk(root: &Path, rel: &Path, out: &mut Vec<Entry>) {
    let Ok(rd) = std::fs::read_dir(root.join(rel)) else {
        return;
    };
    let mut names: Vec<_> = rd.flatten().map(|e| e.file_name()).collect();
    names.sort();
    for name in names {
        let r = rel.join(&name);
        let p = root.join(&r);
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        let kind = if md.file_type().is_symlink() {
            Kind::Link(std::fs::read_link(&p).unwrap_or_default())
        } else if md.is_dir() {
            Kind::Dir
        } else {
            Kind::File
        };
        let is_dir = matches!(kind, Kind::Dir);
        out.push(Entry {
            rel: r.clone(),
            kind,
            accessed: md.accessed().unwrap_or(SystemTime::UNIX_EPOCH),
            modified: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            perms: md.permissions(),
        });
        if is_dir {
            walk(root, &r, out);
        }
    }
}

/// A fresh app on a fresh copy of the page's fake Mac, on the page, after `path`.
fn build(p: &PageSpec, t: &Template, path: &[Step]) -> Result<(FakeMac, H), String> {
    let mac = t.copy();
    let mut h = sized_harness(&mac);
    // Page tests never show the Full Disk Access banner; the shell test covers it.
    h.state_mut().fda = Access::Granted;
    steps(&mut h);
    match p.title {
        "History" | "Settings" => {
            let page = p.title.to_lowercase();
            h.state_mut().navigate(&page);
        }
        t => nav(&mut h, t)?,
    }
    steps(&mut h);
    for s in path {
        let Step::Use(k, scroll) = s;
        use_control_with(&mut h, k, false, *scroll)?;
    }
    Ok((mac, h))
}

#[derive(Clone, Debug)]
enum Outcome {
    Works(&'static str),
    Disabled,
    /// A radio button that is already the chosen one: clicking it changes nothing, as on macOS.
    AlreadyChosen,
    Inherited,
    Fail(String),
}

struct Row {
    state: String,
    key: Key,
    outcome: Outcome,
}

struct Explored {
    rows: Vec<Row>,
    states: usize,
    failures: Vec<String>,
}

const MAX_STATES: usize = 40;
const MAX_DEPTH: usize = 6;

fn path_name(path: &[Step]) -> String {
    if path.is_empty() {
        return "start".into();
    }
    path.iter()
        .map(|Step::Use(k, _)| k.show())
        .collect::<Vec<_>>()
        .join(" > ")
}

/// Explore a page breadth first and use every control of every state.
fn explore(p: &PageSpec, max_states: usize) -> Explored {
    let t = Template::new(p);
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    let mut seen: BTreeSet<BTreeSet<(String, String)>> = BTreeSet::new();
    let mut queue: VecDeque<(Vec<Step>, Vec<Key>)> = VecDeque::new();
    queue.push_back((Vec::new(), Vec::new()));
    let mut states = 0;
    while let Some((path, parent_keys)) = queue.pop_front() {
        if states >= max_states {
            break;
        }
        let built = catch_unwind(AssertUnwindSafe(|| build(p, &t, &path)));
        let (_mac, h) = match built {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => {
                failures.push(format!(
                    "{}: {}: cannot rebuild: {e}",
                    p.title,
                    path_name(&path)
                ));
                continue;
            }
            Err(_) => {
                failures.push(format!(
                    "{}: {}: panic while rebuilding",
                    p.title,
                    path_name(&path)
                ));
                continue;
            }
        };
        let ns = nodes(&h);
        let ctls = controls(&ns, false);
        if states == 0 {
            seen.insert(signature(&ctls));
        }
        states += 1;
        for b in bad_texts(&ns) {
            failures.push(format!("{}: {}: {b}", p.title, path_name(&path)));
        }
        drop(h);
        let state_name = path_name(&path);
        let keys: Vec<Key> = ctls.iter().map(|(k, _)| k.clone()).collect();
        for (key, n) in &ctls {
            if parent_keys.contains(key) {
                rows.push(Row {
                    state: state_name.clone(),
                    key: key.clone(),
                    outcome: Outcome::Inherited,
                });
                continue;
            }
            if n.disabled {
                rows.push(Row {
                    state: state_name.clone(),
                    key: key.clone(),
                    outcome: Outcome::Disabled,
                });
                continue;
            }
            if n.role == Role::RadioButton && n.toggled == "True" {
                rows.push(Row {
                    state: state_name.clone(),
                    key: key.clone(),
                    outcome: Outcome::AlreadyChosen,
                });
                continue;
            }
            let attempt = |scroll: bool| {
                catch_unwind(AssertUnwindSafe(|| {
                    let (_mac, mut h) = build(p, &t, &path)?;
                    let before_n = nodes(&h);
                    let before = (h.state().action_count(), tree_text(&before_n));
                    use_control_with(&mut h, key, false, scroll)?;
                    let after_n = nodes(&h);
                    let acted = h.state().action_count() > before.0;
                    let changed = tree_text(&after_n) != before.1;
                    let sig = signature(&controls(&after_n, false));
                    Ok::<_, String>((acted, changed, sig, bad_texts(&after_n)))
                }))
            };
            let mut scrolled = false;
            let mut res = attempt(false);
            if matches!(res, Ok(Ok((false, false, _, _)))) {
                // Drawn but maybe clipped by its list: scroll to it and try again.
                scrolled = true;
                res = attempt(true);
            }
            let outcome = match res {
                Err(_) => Outcome::Fail("panic".into()),
                Ok(Err(e)) => Outcome::Fail(e),
                Ok(Ok((acted, changed, sig, bad))) => {
                    for b in bad {
                        failures.push(format!("{}: {state_name} > {}: {b}", p.title, key.show()));
                    }
                    if path.len() < MAX_DEPTH && !seen.contains(&sig) {
                        seen.insert(sig);
                        let mut next = path.clone();
                        next.push(Step::Use(key.clone(), scrolled));
                        queue.push_back((next, keys.clone()));
                    }
                    match (acted, changed) {
                        (true, true) if scrolled => {
                            Outcome::Works("acted, screen changed, after scrolling to it")
                        }
                        (true, false) if scrolled => Outcome::Works("acted, after scrolling to it"),
                        (false, true) if scrolled => {
                            Outcome::Works("screen changed, after scrolling to it")
                        }
                        (true, true) => Outcome::Works("acted, screen changed"),
                        (true, false) => Outcome::Works("acted"),
                        (false, true) => Outcome::Works("screen changed"),
                        (false, false) => {
                            Outcome::Fail(format!("nothing happened (at {:?})", n.rect))
                        }
                    }
                }
            };
            if let Outcome::Fail(why) = &outcome {
                failures.push(format!("{}: {state_name} > {}: {why}", p.title, key.show()));
            }
            rows.push(Row {
                state: state_name.clone(),
                key: key.clone(),
                outcome,
            });
        }
    }
    if !queue.is_empty() {
        println!(
            "{}: stopped after {max_states} states, {} more not explored",
            p.title,
            queue.len()
        );
    }
    Explored {
        rows,
        states,
        failures,
    }
}

fn print_table(title: &str, e: &Explored, secs: f32) {
    let tested = e
        .rows
        .iter()
        .filter(|r| matches!(r.outcome, Outcome::Works(_) | Outcome::Fail(_)))
        .count();
    let failed = e
        .rows
        .iter()
        .filter(|r| matches!(r.outcome, Outcome::Fail(_)))
        .count();
    println!(
        "\n== {title}: {} states, {} controls listed, {tested} clicked, {failed} failed ({secs:.1} s)",
        e.states,
        e.rows.len()
    );
    println!("| State | Control | Result |");
    println!("|---|---|---|");
    for r in &e.rows {
        let res = match &r.outcome {
            Outcome::Works(w) => format!("ok ({w})"),
            Outcome::Disabled => "disabled".into(),
            Outcome::AlreadyChosen => "already chosen".into(),
            Outcome::Inherited => "inherited".into(),
            Outcome::Fail(w) => format!("FAIL: {w}"),
        };
        if matches!(r.outcome, Outcome::Inherited) {
            continue;
        }
        println!("| {} | {} | {res} |", r.state, r.key.show());
    }
}

fn run_page(p: PageSpec) {
    let t0 = Instant::now();
    let e = explore(&p, MAX_STATES);
    print_table(p.title, &e, t0.elapsed().as_secs_f32());
    assert!(e.states > 0, "{}: no state could be built", p.title);
    assert!(
        e.failures.is_empty(),
        "{}: {} problems:\n{}",
        p.title,
        e.failures.len(),
        e.failures.join("\n")
    );
}

macro_rules! page_test {
    ($name:ident, $title:expr, $mac:expr) => {
        page_test!($name, $title, $mac, no_post);
    };
    ($name:ident, $title:expr, $mac:expr, $post:expr) => {
        #[test]
        fn $name() {
            run_page(PageSpec {
                title: $title,
                mac: $mac,
                post: $post,
            });
        }
    };
}

page_test!(quick_sweep, "Quick Sweep", mac_sweep);
page_test!(system_junk, "System Junk", mac_junk);
page_test!(mail_downloads, "Mail Downloads", mac_mail);
page_test!(trash_bins, "Trash Bins", mac_trash);
page_test!(threat_check, "Threat Check", mac_startup, lock_system);
page_test!(privacy_traces, "Privacy Traces", mac_privacy);
page_test!(tune_up_tasks, "Tune-up Tasks", mac_tasks);
page_test!(startup_items, "Startup Items", mac_startup, lock_system);
page_test!(app_remover, "App Remover", mac_remover);
page_test!(app_updates, "App Updates", mac_updates);
page_test!(live_monitor, "Live Monitor", mac_monitor);
page_test!(history, "History", mac_history);
page_test!(settings, "Settings", mac_settings);

page_test!(large_old_files, "Large & Old Files", mac_files);
page_test!(duplicates, "Duplicates", mac_files);
page_test!(similar_images, "Similar Images", mac_files);
page_test!(disk_map, "Disk Map", mac_files);

/// The same walk at the app's smallest window (1060 x 680), fewer states per page: every
/// control the first screens show must still be reachable and respond.
#[test]
fn every_page_at_the_smallest_window() {
    WINDOW.with(|c| c.set(MIN_WINDOW));
    let pages: Vec<PageSpec> = vec![
        PageSpec {
            title: "Quick Sweep",
            mac: mac_sweep,
            post: no_post,
        },
        PageSpec {
            title: "System Junk",
            mac: mac_junk,
            post: no_post,
        },
        PageSpec {
            title: "Mail Downloads",
            mac: mac_mail,
            post: no_post,
        },
        PageSpec {
            title: "Trash Bins",
            mac: mac_trash,
            post: no_post,
        },
        PageSpec {
            title: "Threat Check",
            mac: mac_startup,
            post: lock_system,
        },
        PageSpec {
            title: "Privacy Traces",
            mac: mac_privacy,
            post: no_post,
        },
        PageSpec {
            title: "Tune-up Tasks",
            mac: mac_tasks,
            post: no_post,
        },
        PageSpec {
            title: "Startup Items",
            mac: mac_startup,
            post: lock_system,
        },
        PageSpec {
            title: "App Remover",
            mac: mac_remover,
            post: no_post,
        },
        PageSpec {
            title: "App Updates",
            mac: mac_updates,
            post: no_post,
        },
        PageSpec {
            title: "Large & Old Files",
            mac: mac_files,
            post: no_post,
        },
        PageSpec {
            title: "Duplicates",
            mac: mac_files,
            post: no_post,
        },
        PageSpec {
            title: "Similar Images",
            mac: mac_files,
            post: no_post,
        },
        PageSpec {
            title: "Disk Map",
            mac: mac_files,
            post: no_post,
        },
        PageSpec {
            title: "Live Monitor",
            mac: mac_monitor,
            post: no_post,
        },
        PageSpec {
            title: "History",
            mac: mac_history,
            post: no_post,
        },
        PageSpec {
            title: "Settings",
            mac: mac_settings,
            post: no_post,
        },
    ];
    let mut failures = Vec::new();
    println!("\n== Smallest window 1060 x 680");
    println!("| Page | States | Controls clicked | Failed |");
    println!("|---|---|---|---|");
    let only = std::env::var("QA_PAGE").ok();
    for p in &pages {
        if only.as_deref().is_some_and(|o| o != p.title) {
            continue;
        }
        let e = explore(p, 6);
        let clicked = e
            .rows
            .iter()
            .filter(|r| matches!(r.outcome, Outcome::Works(_) | Outcome::Fail(_)))
            .count();
        println!(
            "| {} | {} | {clicked} | {} |",
            p.title,
            e.states,
            e.failures.len()
        );
        failures.extend(e.failures);
    }
    assert!(
        failures.is_empty(),
        "{} problems:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Every sidebar row is inside the window and opens its page; the Full Disk Access banner's
/// buttons and the notice's Dismiss respond.
#[test]
fn shell_controls_respond() {
    // Sidebar: every row, from a fresh app.
    let mac = FakeMac::sample();
    base_answers(&mac);
    let h = harness(&mac);
    let rows: Vec<Key> = controls(&nodes(&h), true)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    drop(h);
    // One row per section (the sections' tools are in each page's switcher).
    let mut names: Vec<String> = Vec::new();
    for (name, _) in cleancraft_ui_egui::app::GROUPS {
        names.push(name.to_string());
    }
    names.push("History".into());
    names.push("Settings".into());
    let labels: Vec<&str> = rows.iter().map(|k| k.label.as_str()).collect();
    assert_eq!(labels, names, "sidebar rows");
    let mut failures = Vec::new();
    println!("\n== Sidebar");
    println!("| Control | Result |");
    println!("|---|---|");
    for k in &rows {
        let mut h = harness(&mac);
        // Start somewhere else so every row, Quick Sweep included, has a page to switch from.
        h.state_mut().navigate("settings");
        steps(&mut h);
        let before = h.state().action_count();
        let page_before = h.state().page;
        let r = use_control(&mut h, k, true);
        let ok = r.is_ok() && h.state().action_count() > before && h.state().page != page_before
            || (k.label == "Settings" && h.state().action_count() > before);
        let title_shown = has(&h, &k.label);
        println!(
            "| {} | {} |",
            k.show(),
            if ok && title_shown { "ok" } else { "FAIL" }
        );
        if let Err(e) = r {
            failures.push(e);
        } else if !ok {
            failures.push(format!("{}: page did not change", k.show()));
        }
        failures.extend(bad_texts(&nodes(&h)));
    }

    // Section switchers: every segment opens its tool.
    println!("\n== Section switchers");
    for (name, feats) in cleancraft_ui_egui::app::GROUPS {
        if feats.len() < 2 {
            continue;
        }
        for f in *feats {
            let mut h = harness(&mac);
            h.state_mut().navigate("settings");
            steps(&mut h);
            let row = Key {
                role: "Button".into(),
                label: name.to_string(),
                nth: 0,
            };
            if let Err(e) = use_control(&mut h, &row, true) {
                failures.push(e);
                continue;
            }
            // Start from another tool of the section, so the segment has a page to switch to.
            let other = feats.iter().find(|x| *x != f).copied().unwrap_or(*f);
            h.state_mut().page = cleancraft_ui_egui::Page::Feature(other);
            steps(&mut h);
            let before = h.state().action_count();
            let key = Key {
                role: "Radio".into(),
                label: f.title().to_string(),
                nth: 0,
            };
            let r = click_segment(&mut h, &key);
            let ok = r.is_ok()
                && h.state().action_count() > before
                && h.state().page == cleancraft_ui_egui::Page::Feature(*f);
            println!(
                "| {name} > {} | {} |",
                f.title(),
                if ok { "ok" } else { "FAIL" }
            );
            if !ok {
                failures.push(format!(
                    "{name} > {}: segment did not open its tool",
                    f.title()
                ));
            }
            failures.extend(bad_texts(&nodes(&h)));
        }
    }

    // Full Disk Access banner: shown when access is missing.
    let mac = FakeMac::new();
    base_answers(&mac);
    mac.home_file("Library/Safari/Bookmarks.plist", 10);
    mac.env.inject_fault(
        &mac.env.home_path("Library/Safari/Bookmarks.plist"),
        cleancraft_core::Fault::PermissionDenied,
    );
    for label in ["Open Privacy Settings", "Check Again"] {
        let mut h = harness(&mac);
        assert_eq!(h.state().fda, Access::Missing);
        let before = h.state().action_count();
        let k = Key {
            role: "Button".into(),
            label: label.into(),
            nth: 0,
        };
        let r = use_control(&mut h, &k, false);
        let ok = r.is_ok() && h.state().action_count() > before;
        println!(
            "| banner {} | {} |",
            k.show(),
            if ok { "ok" } else { "FAIL" }
        );
        if !ok {
            failures.push(format!("banner {label}: {r:?}"));
        }
        if label == "Open Privacy Settings" {
            let urls = mac.env.fake_sys().unwrap().with(|s| s.opened_urls.clone());
            assert!(
                urls.iter()
                    .any(|u| u == cleancraft_core::permissions::FDA_SETTINGS_URL),
                "{urls:?}"
            );
        } else {
            assert!(has(&h, "Full Disk Access is still off."));
        }
        failures.extend(bad_texts(&nodes(&h)));
    }

    // Notice: Dismiss removes it.
    let mut h = harness(&mac);
    h.state_mut().tell("Something happened.");
    steps(&mut h);
    let before = h.state().action_count();
    click(&mut h, "Dismiss");
    assert!(h.state().action_count() > before);
    assert!(h.state().notice.is_none());
    assert!(!has(&h, "Something happened."));
    println!("| notice Button 'Dismiss' | ok |");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every sidebar page in one app: no forbidden text on any first screen.
#[test]
fn every_page_text_follows_the_rules() {
    let mac = testkit_cleanup::junk_mac();
    base_answers(&mac);
    let mut h = harness(&mac);
    let mut bad = Vec::new();
    let titles: Vec<&str> = cleancraft_ui_egui::app::GROUPS
        .iter()
        .flat_map(|(_, f)| f.iter().map(|f| f.title()))
        .chain(["History", "Settings"])
        .collect();
    for t in titles {
        nav(&mut h, t).unwrap();
        for b in bad_texts(&nodes(&h)) {
            bad.push(format!("{t}: {b}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// PAR-1 (fixed): Privacy Traces adds its own buttons to the results toolbar; the toolbar used
/// to grow past the right edge of the window and push Select None and every row's Ignore and
/// Reveal buttons out of reach. It now wraps.
#[test]
fn results_toolbar_and_rows_fit_the_window() {
    let mac = mac_privacy();
    let mut h = harness(&mac);
    nav(&mut h, "Privacy Traces").unwrap();
    click(&mut h, "Scan");
    for label in [
        "Select None",
        "Select All",
        "Wi-Fi Settings",
        "Reset Permissions",
    ] {
        let r = h.get_by_label(label).rect();
        assert!(inside_window(r), "{label} at {r:?}");
    }
    let mut rows = 0;
    for n in nodes(&h) {
        if n.role == Role::Button && (n.label == "Ignore" || n.label == "Reveal") {
            let r = n.rect.unwrap();
            assert!(r.max.x <= W, "{} at {r:?}", n.label);
            rows += 1;
        }
    }
    assert!(rows >= 4, "rows shown: {rows}");
    // Every row's Ignore still works where it is.
    let before = h.state().settings.ignore.len();
    click(&mut h, "Select None");
    common::click_nth(&mut h, "Ignore", 0);
    assert_eq!(h.state().settings.ignore.len(), before + 1);
}

/// PAR-2 (fixed): the sort menus had no accessible name (VoiceOver read nothing).
#[test]
fn sort_menus_have_a_name() {
    let mac = mac_junk();
    let mut h = harness(&mac);
    nav(&mut h, "System Junk").unwrap();
    click(&mut h, "Scan");
    let n = h.get_by_label("Sort");
    assert_eq!(n.value().as_deref(), Some("Sort by size"));
    click(&mut h, "Sort");
    click(&mut h, "Sort by name");
    assert_eq!(
        h.state().plans[&FeatureId::SystemJunk].sort,
        cleancraft_ui_egui::pages::plan_page::Sort::Name
    );
    let mac = mac_remover();
    let mut h = harness(&mac);
    nav(&mut h, "App Remover").unwrap();
    assert_eq!(
        h.get_by_label("Sort").value().as_deref(),
        Some("Sort by name")
    );
    click(&mut h, "Sort");
    click(&mut h, "Sort by size");
    assert!(h.get_by_label("Sort").value().as_deref() == Some("Sort by size"));
}

/// PAR-4 (fixed): egui reports a visible link's text as a plain label, so the Disk Map's
/// folder names were not announced as links (VoiceOver) and the walk above could not see them.
#[test]
fn disk_map_folder_names_are_links_that_open_the_folder() {
    let mac = mac_files();
    let mut h = harness(&mac);
    nav(&mut h, "Disk Map").unwrap();
    click(&mut h, "Map Home Folder");
    let before = h.state().disk_map.current.clone();
    let n = h.get_by_role_and_label(Role::Link, "Downloads");
    assert!(inside_window(n.rect()), "{:?}", n.rect());
    n.click();
    steps(&mut h);
    assert_ne!(h.state().disk_map.current, before);
    assert_eq!(
        h.state().disk_map.current.as_deref(),
        Some(mac.env.home_path("Downloads").as_path())
    );
}
