//! App Remover, several apps at once (apps6), by clicks and keys on a FakeMac: tick apps
//! (checkbox, Cmd-click, Shift-click range, Space on the focused row, group "select all"),
//! the bar's size against a recalculation from disk, one review grouped by app, one
//! confirmation listing every file, all gone, per-app results, Undo per app and Undo All
//! (also from History), running apps locked per app with Quit All, apps of macOS never
//! selectable, one password dialog, errors per file.

mod common;

use std::path::{Path, PathBuf};

use cleancraft_core::executor;
use cleancraft_core::fsutil::{human_size, tree_size};
use cleancraft_core::sys::parse_admin_moves;
use cleancraft_core::testkit::FakeMac;
use cleancraft_core::testkit_apps::FakeMacApps;
use cleancraft_core::testkit_quit::FakeMacQuit;
use cleancraft_core::{Fault, features::uninstaller};
use common::*;
use egui_kittest::kittest::{NodeT, Queryable};

struct Mac {
    mac: FakeMac,
    editor: PathBuf,
    viewer: PathBuf,
    jotter: PathBuf,
    safari: PathBuf,
    /// (app, files named after its id).
    sure: Vec<(PathBuf, Vec<PathBuf>)>,
}

fn mac() -> Mac {
    let mac = FakeMac::new();
    let editor = mac.app("Editor", "com.example.Editor", "2.0");
    let viewer = mac.app("Viewer", "com.maker.Viewer", "1.0");
    let jotter = mac.app("Jotter", "org.jot.Jotter", "3.1");
    let safari = mac.app("Safari", "com.apple.Safari", "18.0");
    // Sizes that sort: Viewer, Editor, Jotter.
    mac.write(
        &viewer.join("Contents/Resources/r.bin"),
        &vec![1u8; 3_000_000],
    );
    mac.write(
        &editor.join("Contents/Resources/r.bin"),
        &vec![1u8; 2_000_000],
    );
    mac.spotlight_finds_nothing();
    mac.home_file("Library/Caches/com.example.Editor/blob", 900_000);
    mac.home_file("Library/Preferences/com.example.Editor.plist", 3_000);
    mac.home_file("Library/Logs/Editor/run.log", 5_000);
    mac.home_file("Library/Caches/com.maker.Viewer/c", 120_000);
    mac.home_file(
        "Library/Application Support/com.maker.Viewer/db.sqlite",
        40_000,
    );
    mac.home_file("Library/Preferences/org.jot.Jotter.plist", 2_000);
    let h = |r: &str| mac.env.home_path(r);
    let sure = vec![
        (
            editor.clone(),
            vec![
                h("Library/Caches/com.example.Editor"),
                h("Library/Preferences/com.example.Editor.plist"),
            ],
        ),
        (
            viewer.clone(),
            vec![
                h("Library/Caches/com.maker.Viewer"),
                h("Library/Application Support/com.maker.Viewer"),
            ],
        ),
        (
            jotter.clone(),
            vec![h("Library/Preferences/org.jot.Jotter.plist")],
        ),
    ];
    Mac {
        mac,
        editor,
        viewer,
        jotter,
        safari,
        sure,
    }
}

fn go(h: &mut H) {
    click(h, "Apps");
    click(h, "App Remover");
}

fn shown(m: &Mac, p: &Path) -> String {
    m.mac.env.display(p)
}

/// What the three apps move with their usual preselection, from disk.
fn disk_size(m: &Mac, which: &[usize]) -> u64 {
    which
        .iter()
        .flat_map(|k| std::iter::once(&m.sure[*k].0).chain(&m.sure[*k].1))
        .map(|p| tree_size(p).0)
        .sum()
}

fn tick(h: &mut H, name: &str) {
    click(h, &format!("Select {name}"));
}

fn picked(h: &H) -> Vec<PathBuf> {
    h.state().apps.picked.iter().cloned().collect()
}

fn settle(h: &mut H) {
    for _ in 0..3 {
        steps(h);
    }
}

#[test]
fn scan_select_three_review_confirm_all_gone_then_undo() {
    let m = mac();
    let mut h = harness(&m.mac);
    go(&mut h);
    // No bar before anything is ticked; the row still opens the app.
    assert!(!has(&h, "Review and Uninstall"));
    for n in ["Editor", "Viewer", "Jotter"] {
        tick(&mut h, n);
    }
    assert!(!has(&h, "Back to Apps"), "ticking does not open the app");
    let size = disk_size(&m, &[0, 1, 2]);
    assert!(
        has(
            &h,
            &format!(
                "3 apps selected: {} to move, leftovers included",
                human_size(size)
            )
        ),
        "bar: 3 apps selected, {}",
        human_size(size)
    );
    click(&mut h, "Review and Uninstall");
    assert!(has(&h, "Uninstall 3 apps"));
    // Every file of every app, grouped by app, the name match listed but not selected.
    for (a, files) in &m.sure {
        assert!(has(&h, &shown(&m, a)));
        for f in files {
            assert!(has(&h, &shown(&m, f)), "{} not listed", f.display());
        }
    }
    let logs = m.mac.env.home_path("Library/Logs/Editor");
    assert!(has(&h, &shown(&m, &logs)));
    assert!(has(
        &h,
        &format!("8 selected from 3 apps, {}", human_size(size))
    ));
    // Fold Editor's files away and back.
    click(&mut h, "Hide files of Editor");
    assert!(!has(&h, &shown(&m, &m.sure[0].1[0])));
    click(&mut h, "Show files of Editor");
    assert!(has(&h, &shown(&m, &m.sure[0].1[0])));
    click(&mut h, "Uninstall 3 Apps");
    assert!(has_text(
        &h,
        &format!(
            "Uninstall 3 apps? 8 items ({}) move to the Trash.",
            human_size(size)
        )
    ));
    // The confirmation lists every file (dialog plus the review behind it).
    for (a, files) in &m.sure {
        for p in std::iter::once(a).chain(files) {
            assert_eq!(
                h.query_all_by_label(&shown(&m, p)).count(),
                2,
                "{} not in the confirmation",
                p.display()
            );
        }
    }
    assert_eq!(h.query_all_by_label(&shown(&m, &logs)).count(), 1);
    click(&mut h, "Move to Trash");
    for (a, files) in &m.sure {
        assert!(!a.exists());
        assert!(files.iter().all(|f| !f.exists()));
    }
    assert!(logs.exists(), "unticked file stays");
    assert!(has_text(&h, "3 apps uninstalled. 8 items moved."));
    assert!(has(&h, &format!("{} moved to the Trash", human_size(size))));
    for n in ["Editor", "Viewer", "Jotter"] {
        assert!(has(&h, &format!("Undo {n}")), "no Undo for {n}");
    }
    // The list follows at once: the apps left every group, the numbers match the disk.
    let st = &h.state().apps;
    let names: Vec<&str> = st.list.iter().flatten().map(|a| a.name.as_str()).collect();
    assert_eq!(names, vec!["Safari"]);
    assert!(st.picked.is_empty());
    // Undo one app from the result screen.
    click(&mut h, "Undo Viewer");
    assert!(m.viewer.exists() && m.sure[1].1.iter().all(|p| p.exists()));
    assert!(!m.editor.exists());
    assert!(has_text(&h, "3 items put back."));
    // Undo All puts back the rest, as one action.
    click(&mut h, "Undo All");
    for (a, files) in &m.sure {
        assert!(a.exists() && files.iter().all(|f| f.exists()));
    }
    assert!(has_text(&h, "5 items put back."));
    settle(&mut h);
    // lists7: apps that are part of macOS (Safari) are hidden from the groups by default.
    let all = [&m.editor, &m.viewer, &m.jotter]
        .iter()
        .map(|p| tree_size(p).0)
        .sum::<u64>();
    assert!(
        has_text(&h, &format!("All Apps  3  ·  {}", human_size(all))),
        "counts recalculated after Undo"
    );
}

#[test]
fn history_undoes_one_app_or_all_of_them() {
    let m = mac();
    let mut h = harness(&m.mac);
    go(&mut h);
    tick(&mut h, "Editor");
    tick(&mut h, "Jotter");
    click(&mut h, "Review and Uninstall");
    click(&mut h, "Uninstall 2 Apps");
    click(&mut h, "Move to Trash");
    assert!(!m.editor.exists() && !m.jotter.exists());
    click(&mut h, "Show History");
    assert!(has(&h, "Undo Editor") && has(&h, "Undo Jotter"));
    click(&mut h, "Undo Jotter");
    assert!(m.jotter.exists() && !m.editor.exists());
    assert!(!has(&h, "Undo Jotter"), "put back app has no Undo");
    click(&mut h, "Undo All");
    assert!(m.editor.exists() && m.sure[0].1.iter().all(|p| p.exists()));
    // The App Remover list follows (the journal watch).
    go(&mut h);
    settle(&mut h);
    assert!(has(&h, "Editor") && has(&h, "Jotter"));
}

#[test]
fn apps_of_macos_cannot_be_selected() {
    let m = mac();
    let mut h = harness(&m.mac);
    go(&mut h);
    // Apps of macOS are hidden until asked for (lists7); shown, they stay locked.
    assert!(!has(&h, "Select Safari"));
    click(&mut h, "Show apps that are part of macOS");
    let cb = h.get_by_label("Select Safari");
    assert!(cb.accesskit_node().is_disabled(), "padlock, not a checkbox");
    cb.click();
    settle(&mut h);
    // Cmd-click on its row does not tick it either (it opens nothing ticked).
    let r = h.get_by_label("Safari").rect();
    click_at(&mut h, r.center(), egui::Modifiers::COMMAND);
    settle(&mut h);
    assert!(picked(&h).is_empty(), "{:?}", picked(&h));
    // Select all of All Apps asks first, with the count of removable apps (not Safari).
    if has(&h, "Back to Apps") {
        click(&mut h, "Back to Apps");
    }
    click(&mut h, "Select all in All Apps");
    assert!(has_text(&h, "Select all 3 apps?"));
    click(&mut h, "Cancel");
    assert!(picked(&h).is_empty());
    click(&mut h, "Select all in All Apps");
    click(&mut h, "Select 3 Apps");
    assert_eq!(picked(&h).len(), 3);
    assert!(!picked(&h).contains(&m.safari));
    // Untick all: no question.
    click(&mut h, "Select all in All Apps");
    assert!(picked(&h).is_empty());
    // Another group: one click, no question.
    click(&mut h, "Select all in Other Sources");
    assert_eq!(picked(&h).len(), 3);
    // Safari pushed into the ticked set by hand still never reaches a plan.
    h.state_mut().apps.picked.insert(m.safari.clone());
    click(&mut h, "Review and Uninstall");
    assert!(has_text(&h, "Safari is part of macOS"));
    assert!(!has(&h, "Uninstall 4 Apps"));
    assert!(m.safari.exists());
}

/// A pointer press and release at `p` with these modifier keys held.
fn click_at(h: &mut H, p: egui::Pos2, modifiers: egui::Modifiers) {
    h.hover_at(p);
    h.step();
    h.event(egui::Event::ModifiersChanged(modifiers));
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers,
        });
        h.step();
    }
    h.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
    steps(h);
}

#[test]
fn keyboard_and_pointer_selection() {
    let m = mac();
    let mut h = harness(&m.mac);
    go(&mut h);
    // Space on the focused row ticks it and does not open it; again unticks.
    h.get_by_label("Editor").focus();
    steps(&mut h);
    h.key_press(egui::Key::Space);
    settle(&mut h);
    assert_eq!(picked(&h), vec![m.editor.clone()]);
    assert!(!has(&h, "Back to Apps"));
    h.key_press(egui::Key::Space);
    settle(&mut h);
    assert!(picked(&h).is_empty());
    // Cmd-click toggles; Shift-click ticks the range from it (sorted by name: Editor,
    // Jotter, Viewer).
    let viewer = h.get_by_label("Viewer").rect();
    let jotter = h.get_by_label("Jotter").rect();
    click_at(&mut h, viewer.center(), egui::Modifiers::COMMAND);
    click_at(&mut h, jotter.center(), egui::Modifiers::SHIFT);
    settle(&mut h);
    let mut want = vec![m.viewer.clone(), m.jotter.clone()];
    want.sort();
    assert_eq!(picked(&h), want);
    assert!(!has(&h, "Back to Apps"));
    // A plain click still opens the app.
    click_at(&mut h, jotter.center(), egui::Modifiers::NONE);
    assert!(has(&h, "Back to Apps"));
    click(&mut h, "Back to Apps");
    // The pointing hand over a checkbox.
    h.get_by_label("Select Editor").hover();
    settle(&mut h);
    assert_eq!(
        h.output().platform_output.cursor_icon,
        egui::CursorIcon::PointingHand
    );
    // Default sort: by name. (Safari, part of macOS, is hidden from the list by default.)
    let rows: Vec<String> = ["Editor", "Jotter", "Viewer"]
        .iter()
        .map(|n| n.to_string())
        .collect();
    let ys: Vec<f32> = rows
        .iter()
        .map(|n| h.get_by_label(n).rect().center().y)
        .collect();
    assert!(ys.windows(2).all(|w| w[0] < w[1]), "{ys:?}");
    for (n, p) in [("Viewer", &m.viewer), ("Editor", &m.editor)] {
        let a = h
            .state()
            .apps
            .list
            .iter()
            .flatten()
            .find(|a| a.name == n)
            .unwrap()
            .clone();
        assert_eq!(a.size, tree_size(p).0);
        assert!(has(&h, &human_size(a.size)));
    }
}

#[test]
fn a_running_app_locks_only_its_group_and_quit_all_unlocks() {
    let m = mac();
    let me = 501;
    m.mac.user_id(me);
    m.mac
        .run_program(&m.editor.join("Contents/MacOS/Editor"), 4242, me);
    m.mac
        .run_program(&m.viewer.join("Contents/MacOS/Viewer"), 4243, me);
    let f = m.mac.env.fake_sys().unwrap();
    f.answer("/usr/bin/osascript", "");
    f.answer("/bin/kill", "");
    let mut h = harness(&m.mac);
    go(&mut h);
    for n in ["Editor", "Viewer", "Jotter"] {
        tick(&mut h, n);
    }
    click(&mut h, "Review and Uninstall");
    // The review lists the apps biggest first.
    assert!(has_text(&h, "Viewer and Editor are running"));
    assert!(has(&h, "Quit Editor") && has(&h, "Quit Viewer") && has(&h, "Quit All"));
    // Their files are locked, Jotter's are not.
    for k in [0, 1] {
        for p in std::iter::once(&m.sure[k].0).chain(&m.sure[k].1) {
            assert!(
                h.get_by_label(&shown(&m, p)).accesskit_node().is_disabled(),
                "{} should be locked",
                p.display()
            );
        }
    }
    let jot = shown(&m, &m.sure[2].1[0]);
    assert!(!h.get_by_label(&jot).accesskit_node().is_disabled());
    // Ticking a running app no longer blocks Uninstall: all three apps are offered together,
    // and clicking it quits the running ones first (REVIEW apps4: Uninstall asks to quit).
    assert!(has(&h, "Uninstall 3 Apps"));
    // Quit All: Viewer quits, Editor shows a save dialog and stays.
    let quit_key = |id: &str| {
        std::iter::once("/usr/bin/osascript".to_string())
            .chain(cleancraft_core::app_quit::quit_args(id))
            .collect::<Vec<_>>()
            .join(" ")
    };
    f.answer(&quit_key("com.maker.Viewer"), "");
    m.mac.stops(&quit_key("com.maker.Viewer"), &[4243]);
    click(&mut h, "Quit All");
    settle(&mut h);
    let calls = f.calls();
    assert!(calls.iter().any(|c| c.contains("com.maker.Viewer")));
    assert!(calls.iter().any(|c| c.contains("com.example.Editor")));
    assert!(has_text(&h, "Viewer quit."), "notice");
    assert!(has(&h, "Editor did not quit"));
    assert!(has(&h, "Force Quit Editor"));
    // Viewer's files unlocked and preselected again.
    // (The bundle's box is never a choice: the app is always part of an uninstall.)
    for p in &m.sure[1].1 {
        assert!(!h.get_by_label(&shown(&m, p)).accesskit_node().is_disabled());
    }
    assert!(has(&h, "Uninstall 3 Apps"));
    // Force Quit asks first; Cancel changes nothing.
    click(&mut h, "Force Quit Editor");
    assert!(has(&h, "Force quit Editor?"));
    click(&mut h, "Cancel");
    assert!(!f.calls().iter().any(|c| c.starts_with("/bin/kill")));
    // Uninstall tries Editor again (it still refuses), then carries on with the two that
    // can go; Editor stays with every file.
    click(&mut h, "Uninstall 3 Apps");
    settle(&mut h);
    assert!(has_text(
        &h,
        "Editor is running and stays until you quit it."
    ));
    click(&mut h, "Move to Trash");
    assert!(!m.viewer.exists() && !m.jotter.exists());
    assert!(m.editor.exists() && m.sure[0].1.iter().all(|p| p.exists()));
    assert!(has_text(&h, "2 apps uninstalled."));
    assert!(has(
        &h,
        "Not removed: it was running. Quit it, then uninstall it."
    ));
}

#[test]
fn force_quit_one_app_after_confirmation() {
    let m = mac();
    m.mac.user_id(501);
    m.mac
        .run_program(&m.editor.join("Contents/MacOS/Editor"), 4242, 501);
    let f = m.mac.env.fake_sys().unwrap();
    f.answer("/usr/bin/osascript", "");
    f.answer("/bin/kill", "");
    let mut h = harness(&m.mac);
    go(&mut h);
    tick(&mut h, "Editor");
    tick(&mut h, "Jotter");
    click(&mut h, "Review and Uninstall");
    // One running app: its own banner, no Quit All.
    assert!(has(&h, "Quit Editor") && !has(&h, "Quit All"));
    click(&mut h, "Quit Editor");
    settle(&mut h);
    click(&mut h, "Force Quit Editor");
    let n = h.query_all_by_label("Force Quit").count();
    h.get_all_by_label("Force Quit").nth(n - 1).unwrap().click();
    settle(&mut h);
    assert!(f.calls().iter().any(|c| c.starts_with("/bin/kill")));
}

#[test]
fn apps_needing_the_password_share_one_prompt() {
    let m = mac();
    m.mac.env.inject_fault(&m.editor, Fault::PermissionDenied);
    m.mac.env.inject_fault(&m.jotter, Fault::PermissionDenied);
    let mut h = harness(&m.mac);
    go(&mut h);
    for n in ["Editor", "Viewer", "Jotter"] {
        tick(&mut h, n);
    }
    click(&mut h, "Review and Uninstall");
    click(&mut h, "Uninstall 3 Apps");
    assert!(has(
        &h,
        "2 items are in folders only an administrator can change. macOS will ask for the administrator password, and they go to the CleanCraft quarantine."
    ));
    click(&mut h, "Move to Trash");
    let calls = m.mac.env.fake_sys().unwrap().admin_calls();
    assert_eq!(calls.len(), 1, "one password dialog");
    assert_eq!(parse_admin_moves(&calls[0]).unwrap().len(), 2);
    assert!(!m.editor.exists() && !m.viewer.exists() && !m.jotter.exists());
    assert!(has_text(&h, "3 apps uninstalled."));
}

#[test]
fn permission_denied_and_file_in_use_are_shown_per_file() {
    let m = mac();
    let mut h = harness(&m.mac);
    go(&mut h);
    for n in ["Editor", "Viewer", "Jotter"] {
        tick(&mut h, n);
    }
    click(&mut h, "Review and Uninstall");
    let cache = m.sure[0].1[0].clone();
    let db = m.sure[1].1[1].clone();
    m.mac.env.inject_fault(&cache, Fault::PermissionDenied);
    m.mac
        .env
        .fake_sys()
        .unwrap()
        .set_open(&db.join("db.sqlite"));
    click(&mut h, "Uninstall 3 Apps");
    click(&mut h, "Move to Trash");
    assert!(cache.exists() && db.exists());
    assert!(!m.editor.exists() && !m.viewer.exists() && !m.jotter.exists());
    assert!(has_text(&h, "com.example.Editor: permission denied"));
    assert!(has_text(&h, "com.maker.Viewer: open in"));
    assert!(has_text(&h, "6 items moved. 2 skipped."));
    let _ = uninstaller::BATCH_PREFIX;
    assert_eq!(executor::history(&m.mac.env).len(), 6);
}

#[test]
fn leave_out_and_group_select_for_files() {
    let m = mac();
    let gone = m.mac.env.home_path("Library/Caches/com.gone.Writer");
    m.mac.home_file("Library/Caches/com.gone.Writer/c", 60_000);
    let mut h = harness(&m.mac);
    go(&mut h);
    tick(&mut h, "Editor");
    tick(&mut h, "Viewer");
    click(&mut h, "Review and Uninstall");
    click(&mut h, "Leave Out Viewer");
    assert!(has(&h, "Uninstall 1 app"));
    assert!(!has(&h, &shown(&m, &m.sure[1].1[0])));
    assert_eq!(picked(&h), vec![m.editor.clone()]);
    click(&mut h, "Back to Apps");
    // Leftovers: select all in the group, then remove.
    click(&mut h, "Select all in Leftovers");
    assert!(
        h.state()
            .apps
            .extras
            .as_ref()
            .unwrap()
            .items
            .iter()
            .any(|i| i.path == gone && i.selected)
    );
}

/// Renders of the new screens at both window sizes, saved with
/// `CLEANCRAFT_SNAPSHOTS=1` (into CLEANCRAFT_SNAPSHOT_DIR, default docs/screens/v2/apps6).
#[test]
fn renders() {
    if std::env::var("CLEANCRAFT_SNAPSHOTS").is_err() {
        return;
    }
    if std::panic::catch_unwind(egui_kittest::wgpu::WgpuTestRenderer::new).is_err() {
        return;
    }
    let dir = std::env::var("CLEANCRAFT_SNAPSHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/screens/v2/apps6")
        });
    std::fs::create_dir_all(&dir).unwrap();
    for (w, hh) in [(1060.0, 680.0), (1280.0, 820.0)] {
        for scene in ["list", "review", "confirm", "done"] {
            let m = mac();
            m.mac.user_id(501);
            m.mac
                .run_program(&m.editor.join("Contents/MacOS/Editor"), 4242, 501);
            m.mac
                .env
                .fake_sys()
                .unwrap()
                .answer("/usr/bin/osascript", "");
            if scene == "done" {
                m.mac
                    .env
                    .inject_fault(&m.sure[1].1[0], Fault::PermissionDenied);
            }
            let mut app = cleancraft_ui_egui::CleanApp::new(m.mac.env.clone()).with_sync();
            app.fda = cleancraft_core::permissions::Access::Granted;
            let mut h = egui_kittest::Harness::builder()
                .with_size(egui::vec2(w, hh))
                .build_ui_state(
                    |ui, app: &mut cleancraft_ui_egui::CleanApp| {
                        let ctx = ui.ctx().clone();
                        app.logic(&ctx);
                        let mut root = egui::Ui::new(
                            ctx.clone(),
                            egui::Id::new("window"),
                            egui::UiBuilder::new()
                                .layer_id(egui::LayerId::background())
                                .max_rect(ctx.viewport_rect()),
                        );
                        app.ui(&mut root);
                    },
                    app,
                );
            settle(&mut h);
            go(&mut h);
            for n in ["Editor", "Viewer", "Jotter"] {
                tick(&mut h, n);
            }
            if scene != "list" {
                click(&mut h, "Review and Uninstall");
            }
            if scene == "confirm" || scene == "done" {
                click(&mut h, "Uninstall 2 Apps");
            }
            if scene == "done" {
                click(&mut h, "Move to Trash");
            }
            for _ in 0..20 {
                h.step();
            }
            let img = h.render().expect("render");
            img.save(dir.join(format!("batch-{scene}-{w}x{hh}.png")))
                .unwrap();
        }
    }
}

#[test]
fn select_all_while_searching_ticks_only_the_apps_shown_and_never_cleancraft() {
    let m = mac();
    let me = m
        .mac
        .app("CleanCraft", "com.cristianizzo.cleancraft", "1.0");
    let mut h = harness(&m.mac);
    go(&mut h);
    assert!(
        h.get_by_label("Select CleanCraft")
            .accesskit_node()
            .is_disabled()
    );
    type_into(&mut h, "Search apps", "o");
    // Shown: Editor, Viewer, Jotter (and CleanCraft, locked); Safari hidden.
    click(&mut h, "Select all in All Apps");
    assert!(has_text(&h, "Select all 3 apps?"));
    click(&mut h, "Select 3 Apps");
    let p = picked(&h);
    assert_eq!(p.len(), 3);
    assert!(!p.contains(&me) && !p.contains(&m.safari));
    // Another group while searching: only what is shown.
    click(&mut h, "Clear Selection");
    h.state_mut().apps.group = cleancraft_ui_egui::pages::apps::Group::Other;
    h.state_mut().apps.search = "jot".into();
    settle(&mut h);
    click(&mut h, "Select all in Other Sources");
    assert_eq!(picked(&h), vec![m.jotter.clone()]);
}

#[test]
fn an_app_already_in_the_trash_offers_its_leftovers_only() {
    let m = mac();
    // Dragged to the Trash by hand: the bundle sits in ~/.Trash, its files stay.
    let trashed = m.mac.env.user_trash().join("Viewer.app");
    std::fs::create_dir_all(m.mac.env.user_trash()).unwrap();
    std::fs::rename(&m.viewer, &trashed).unwrap();
    let mut h = harness(&m.mac);
    go(&mut h);
    {
        let app = h.state_mut();
        cleancraft_ui_egui::pages::apps::open_app(app, &trashed);
    }
    settle(&mut h);
    assert!(has_text(&h, "Viewer is already in the Trash"));
    for f in &m.sure[1].1 {
        assert!(has(&h, &shown(&m, f)), "{} not listed", f.display());
    }
    assert!(!has(&h, "Uninstall") && !has(&h, "Reset App..."));
    click(&mut h, "Remove Leftovers");
    assert!(has_text(&h, "Move 2 items"));
    click(&mut h, "Move to Trash");
    assert!(m.sure[1].1.iter().all(|f| !f.exists()));
    assert!(trashed.exists(), "the app in the Trash is left alone");
}
