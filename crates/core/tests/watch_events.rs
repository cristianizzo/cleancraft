//! The watchers on a fake Mac with real file system events (inotify here, FSEvents on a Mac):
//! drag-to-Trash offers, new startup items and apps, debounce, coalescing, pause, quiet hours,
//! Full Disk Access. The watcher never moves anything: the journal stays empty.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cleancraft_core::env::Fault;
use cleancraft_core::executor::{self, Method};
use cleancraft_core::features::uninstaller::{self, CAT_HIGH, CAT_MEDIUM, Uninstaller};
use cleancraft_core::features::{self, Feature, ScanCtx, malware::Malware};
use cleancraft_core::fsutil::{human_size, tree_size};
use cleancraft_core::sys::CmdOut;
use cleancraft_core::testkit::FakeMac;
use cleancraft_core::testkit_perf::{
    add_launch_item, codesign_adhoc, codesign_developer, codesign_unsigned, spctl_rejected,
};
use cleancraft_core::watch::{
    Access, AppRoute, WatchEvent, WatchHandle, WatchKind, WatchSettings, Watcher, preselect_threat,
    trashed_app_plan,
};
use cleancraft_core::{FeatureId, Settings};

type Events = Arc<Mutex<Vec<WatchEvent>>>;

fn quick() -> WatchSettings {
    WatchSettings {
        debounce: Duration::from_millis(300),
        coalesce: Duration::from_millis(300),
        tick: Duration::from_millis(40),
        ..Default::default()
    }
}

/// Start with the launchd and app folders present (a fresh fake Mac has none of them).
fn start(mac: &FakeMac, s: WatchSettings) -> (WatchHandle, Events) {
    for d in [
        "~/Library/LaunchAgents",
        "/Library/LaunchAgents",
        "/Library/LaunchDaemons",
        "~/Applications",
    ] {
        let p = match d.strip_prefix("~/") {
            Some(r) => mac.env.home_path(r),
            None => mac.env.sys_path(d),
        };
        fs::create_dir_all(p).unwrap();
    }
    start_bare(mac, s)
}

fn start_bare(mac: &FakeMac, s: WatchSettings) -> (WatchHandle, Events) {
    let evs: Events = Arc::default();
    let e2 = evs.clone();
    let h = Watcher::start(mac.env.clone(), s, move |ev| e2.lock().unwrap().push(ev))
        .expect("watcher starts");
    (h, evs)
}

/// Wait until `n` events arrived (or 6 s), then a little longer to catch extra ones.
fn wait_events(evs: &Events, n: usize) -> Vec<WatchEvent> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(6) && evs.lock().unwrap().len() < n {
        std::thread::sleep(Duration::from_millis(30));
    }
    std::thread::sleep(Duration::from_millis(900));
    evs.lock().unwrap().clone()
}

/// Nothing arrives within `secs`.
fn quiet_for(evs: &Events, secs: f32) -> Vec<WatchEvent> {
    std::thread::sleep(Duration::from_secs_f32(secs));
    evs.lock().unwrap().clone()
}

fn fake(mac: &FakeMac) -> &cleancraft_core::sys::FakeSys {
    mac.env.fake_sys().unwrap()
}

fn ok_stderr(s: String) -> CmdOut {
    CmdOut {
        status: 0,
        stdout: String::new(),
        stderr: s,
    }
}

/// Info.plist of a bundle.
fn info_plist(name: &str, id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleName</key><string>{name}</string>
<key>CFBundleShortVersionString</key><string>2.1</string>
<key>CFBundleVersion</key><string>210</string>
</dict></plist>"#
    )
}

/// An app bundle built in a staging folder outside every watched folder.
fn staged_app(mac: &FakeMac, name: &str, id: &str) -> PathBuf {
    let app = mac.env.home_path(&format!("staging/{name}.app"));
    mac.write(
        &app.join("Contents/Info.plist"),
        info_plist(name, id).as_bytes(),
    );
    mac.home_file(
        &format!("staging/{name}.app/Contents/MacOS/{name}"),
        400_000,
    );
    app
}

/// Leftovers of Editor: three named after its id, one named like it. Returns their paths.
fn editor_leftovers(mac: &FakeMac) -> Vec<PathBuf> {
    vec![
        mac.home_file(
            "Library/Application Support/com.example.Editor/db.sqlite",
            2_400_000,
        ),
        mac.home_file("Library/Caches/com.example.Editor/blob", 900_000),
        mac.home_file("Library/Preferences/com.example.Editor.plist", 4_000),
        mac.home_file("Library/Application Support/Editor/state.json", 60_000),
    ]
}

/// The top-level leftover entries, sized with the shared formula.
fn expected_leftovers(mac: &FakeMac) -> (usize, u64) {
    let tops = [
        "Library/Application Support/com.example.Editor",
        "Library/Caches/com.example.Editor",
        "Library/Preferences/com.example.Editor.plist",
        "Library/Application Support/Editor",
    ];
    let bytes = tops
        .iter()
        .map(|t| tree_size(&mac.env.home_path(t)).0)
        .sum();
    (tops.len(), bytes)
}

/// What Finder does when you drag an app to the Trash: one rename.
fn drag_to_trash(mac: &FakeMac, app: &Path) -> PathBuf {
    let dest = mac.env.user_trash().join(app.file_name().unwrap());
    fs::rename(app, &dest).unwrap();
    dest
}

fn assert_nothing_moved(mac: &FakeMac) {
    assert!(
        executor::history(&mac.env).is_empty(),
        "the watcher wrote to the journal"
    );
    let f = fake(mac);
    assert!(
        f.admin_calls().is_empty(),
        "admin commands: {:?}",
        f.admin_calls()
    );
    for c in f.calls() {
        assert!(
            !c.contains("launchctl bootout") && !c.contains("osascript") && !c.contains(" mv "),
            "the watcher ran a command that changes something: {c}"
        );
    }
}

#[test]
fn app_dragged_to_the_trash_gives_one_offer_with_its_leftovers() {
    let mac = FakeMac::new();
    editor_leftovers(&mac);
    let app = staged_app(&mac, "Editor", "com.example.Editor");
    let (h, evs) = start(&mac, quick());
    assert_eq!(h.status().trash, Access::Watching);
    let trashed = drag_to_trash(&mac, &app);

    let got = wait_events(&evs, 1);
    assert_eq!(got.len(), 1, "{got:#?}");
    let ev = &got[0];
    let (n, bytes) = expected_leftovers(&mac);
    assert_eq!(n, 4);
    assert_eq!(ev.kind, WatchKind::TrashedApp);
    assert_eq!(ev.title, "Editor was moved to the Trash");
    assert_eq!(
        ev.body,
        format!(
            "4 items it left behind use {}. Review them in App Remover.",
            human_size(bytes)
        )
    );
    assert_eq!(ev.open, AppRoute::UninstallApp(trashed.clone()));
    assert_eq!(ev.paths, vec![trashed.clone()]);
    // The route goes through the agent as text and back.
    assert_eq!(AppRoute::parse(&ev.open.to_arg()).as_ref(), Some(&ev.open));

    // The plan the route opens lists the same files, nothing selected beyond App Remover's
    // usual choice (named after the id), and not the app in the Trash.
    let plan = trashed_app_plan(&mac.env, &Settings::default(), &trashed).unwrap();
    let left: Vec<_> = plan
        .items
        .iter()
        .filter(|i| i.category == CAT_HIGH || i.category == CAT_MEDIUM)
        .collect();
    assert_eq!(left.len(), 4);
    assert_eq!(left.iter().map(|i| i.size).sum::<u64>(), bytes);
    assert!(plan.items.iter().all(|i| !i.path.starts_with(&trashed)));
    assert_eq!(plan.selected_count(), 3);
    assert!(
        plan.items
            .iter()
            .filter(|i| i.selected)
            .all(|i| i.category == CAT_HIGH)
    );

    // Everything is still where it was: the watcher only told.
    assert!(trashed.join("Contents/Info.plist").is_file());
    for p in editor_leftovers_paths(&mac) {
        assert!(p.exists(), "{} moved", p.display());
    }
    assert_nothing_moved(&mac);
    drop(h);
}

fn editor_leftovers_paths(mac: &FakeMac) -> Vec<PathBuf> {
    [
        "Library/Application Support/com.example.Editor/db.sqlite",
        "Library/Caches/com.example.Editor/blob",
        "Library/Preferences/com.example.Editor.plist",
        "Library/Application Support/Editor/state.json",
    ]
    .iter()
    .map(|r| mac.env.home_path(r))
    .collect()
}

#[test]
fn the_plan_from_the_offer_runs_through_the_normal_confirmation() {
    // The offer only opens a plan; moving needs the digest of the plan the user confirmed.
    let mac = FakeMac::new();
    editor_leftovers(&mac);
    let app = staged_app(&mac, "Editor", "com.example.Editor");
    let trashed = drag_to_trash(&mac, &app);
    let plan = trashed_app_plan(&mac.env, &Settings::default(), &trashed).unwrap();
    let allow = Uninstaller.allow_roots(&mac.env);
    let refused = executor::execute(
        &mac.env,
        &plan,
        &allow,
        "not-the-digest",
        Method::Trash,
        &mut |_, _| {},
    );
    assert!(refused.is_err());
    assert!(executor::history(&mac.env).is_empty());
    let rep = features::run_plan(
        &mac.env,
        &Settings::default(),
        &plan,
        &plan.digest(),
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(rep.moved.len(), 3);
    assert!(
        mac.env
            .home_path("Library/Application Support/Editor")
            .exists()
    );
}

#[test]
fn an_app_cleancraft_moved_to_the_trash_gives_no_offer() {
    let mac = FakeMac::new();
    let app = mac.app("Editor", "com.example.Editor", "2.1");
    editor_leftovers(&mac);
    // Keep one leftover out of the uninstall so it would still be offered.
    let (h, evs) = start(&mac, quick());
    let mut plan =
        uninstaller::uninstall_plan(&mac.env, &Settings::default(), &app, false).unwrap();
    let keep = plan
        .items
        .iter()
        .position(|i| i.path.ends_with("com.example.Editor.plist"))
        .unwrap();
    plan.set_selected(keep, false);
    let rep = features::run_plan(
        &mac.env,
        &Settings::default(),
        &plan,
        &plan.digest(),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(
        rep.moved.iter().any(|r| matches!(&r.kind,
            executor::ActionKind::Moved { original, .. } if original == &app)),
        "the app went to the Trash through CleanCraft"
    );
    assert!(fs::read_dir(mac.env.user_trash()).unwrap().count() > 0);
    let got = quiet_for(&evs, 2.0);
    assert!(got.is_empty(), "{got:#?}");
    drop(h);
}

#[test]
fn a_bundle_still_being_copied_waits_until_its_size_settles() {
    let mac = FakeMac::new();
    editor_leftovers(&mac);
    let (h, evs) = start(&mac, quick());
    let app = mac.env.user_trash().join("Editor.app");
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    // The copy goes on for 1.5 s, five times the debounce, without the Info.plist at first.
    let t = Instant::now();
    let mut i = 0;
    while t.elapsed() < Duration::from_millis(1500) {
        fs::write(
            app.join(format!("Contents/MacOS/part{i}")),
            vec![7u8; 50_000],
        )
        .unwrap();
        if i == 6 {
            fs::write(
                app.join("Contents/Info.plist"),
                info_plist("Editor", "com.example.Editor"),
            )
            .unwrap();
        }
        i += 1;
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            evs.lock().unwrap().is_empty(),
            "an event came while the bundle was still growing"
        );
    }
    let got = wait_events(&evs, 1);
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!(got[0].kind, WatchKind::TrashedApp);
    assert!(got[0].body.starts_with("4 items it left behind"));
    drop(h);
}

#[test]
fn a_second_copy_of_the_same_app_and_a_restart_give_no_second_offer() {
    let mac = FakeMac::new();
    editor_leftovers(&mac);
    let a = staged_app(&mac, "Editor", "com.example.Editor");
    let (h, evs) = start(&mac, quick());
    drag_to_trash(&mac, &a);
    assert_eq!(wait_events(&evs, 1).len(), 1);
    // The same app again, under the name Finder gives a second copy.
    let b = staged_app(&mac, "Editor 2", "com.example.Editor");
    drag_to_trash(&mac, &b);
    assert_eq!(quiet_for(&evs, 1.5).len(), 1);
    drop(h);
    // The agent restarts: what is already in the Trash is old news, and the id is remembered.
    let (h2, evs2) = start(&mac, quick());
    let c = staged_app(&mac, "Editor 3", "com.example.Editor");
    drag_to_trash(&mac, &c);
    assert!(quiet_for(&evs2, 1.5).is_empty());
    drop(h2);
}

#[test]
fn an_app_without_leftovers_or_still_installed_gives_no_offer() {
    let mac = FakeMac::new();
    // Still installed in /Applications: the copy in the Trash is a duplicate.
    mac.app("Editor", "com.example.Editor", "2.1");
    editor_leftovers(&mac);
    let (h, evs) = start(&mac, quick());
    let dup = staged_app(&mac, "Editor", "com.example.Editor");
    drag_to_trash(&mac, &dup);
    // An app that left nothing behind.
    let clean = staged_app(&mac, "Tidy", "com.example.Tidy");
    drag_to_trash(&mac, &clean);
    // Not an app at all.
    mac.home_file("staging/notes.txt", 10);
    drag_to_trash(&mac, &mac.env.home_path("staging/notes.txt"));
    assert!(quiet_for(&evs, 1.5).is_empty());
    drop(h);
}

#[test]
fn an_unsigned_launch_agent_gives_an_event_that_opens_threat_check_on_it() {
    let mac = FakeMac::new();
    let f = fake(&mac);
    f.answer("id -u", "501\n");
    f.answer("launchctl", "");
    let (h, evs) = start(&mac, quick());
    assert_eq!(h.status().threats, Access::Watching);
    let prog = "/Library/Application Support/Example/helper";
    let plist = add_launch_item(
        &mac,
        "~/Library/LaunchAgents",
        "com.example.helper",
        prog,
        Some(codesign_unsigned(prog)),
    );
    let got = wait_events(&evs, 1);
    assert_eq!(got.len(), 1, "{got:#?}");
    let ev = &got[0];
    assert_eq!(ev.kind, WatchKind::StartupItem);
    assert_eq!(ev.title, "New login item from an unsigned program");
    assert!(
        ev.body
            .starts_with("com.example.helper: its program is not signed"),
        "{}",
        ev.body
    );
    assert_eq!(ev.open, AppRoute::ThreatReview(plist.clone()));
    assert_eq!(ev.paths, vec![plist.clone()]);
    // Threat Check finds the same item, and the route selects it for review.
    let settings = Settings::default();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let ctx = ScanCtx {
        env: &mac.env,
        settings: &settings,
        cancel: &cancel,
        progress: &|_| {},
        meter: &cleancraft_core::progress::Progress::new(),
    };
    let mut plan = Malware(Default::default()).scan(&ctx).unwrap();
    let i = preselect_threat(&mut plan, &ev.paths[0]).expect("Threat Check lists it");
    assert!(plan.items[i].selected);
    // A path the scan did not list is never added.
    let n = plan.items.len();
    assert_eq!(
        preselect_threat(&mut plan, &mac.env.home_path("Documents/x.plist")),
        None
    );
    assert_eq!(plan.items.len(), n);
    assert!(plist.exists(), "never quarantined by the watcher");
    assert_nothing_moved(&mac);
    drop(h);
}

#[test]
fn signed_launch_agents_and_daemons_give_no_event_and_ad_hoc_ones_do() {
    let mac = FakeMac::new();
    let (h, evs) = start(&mac, quick());
    let good = "/Library/Application Support/Example/agent";
    add_launch_item(
        &mac,
        "/Library/LaunchAgents",
        "com.example.agent",
        good,
        Some(ok_stderr(codesign_developer(
            good,
            "Example Corp",
            "ABCDE12345",
        ))),
    );
    let svc = "/Library/PrivilegedHelperTools/com.example.daemon";
    add_launch_item(
        &mac,
        "/Library/LaunchDaemons",
        "com.example.daemon",
        svc,
        Some(ok_stderr(codesign_developer(
            svc,
            "Example Corp",
            "ABCDE12345",
        ))),
    );
    assert!(quiet_for(&evs, 1.5).is_empty());
    let odd = "/Library/Application Support/Odd/updater";
    add_launch_item(
        &mac,
        "/Library/LaunchDaemons",
        "com.odd.updater",
        odd,
        Some(ok_stderr(codesign_adhoc(odd))),
    );
    let got = wait_events(&evs, 1);
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!(
        got[0].title,
        "New background service from a program no known developer signed"
    );
    drop(h);
}

#[test]
fn a_changed_agent_is_checked_again_but_reported_once() {
    let mac = FakeMac::new();
    let prog = "/Library/Application Support/Example/helper";
    let plist = add_launch_item(
        &mac,
        "~/Library/LaunchAgents",
        "com.example.helper",
        prog,
        Some(ok_stderr(codesign_developer(
            prog,
            "Example Corp",
            "ABCDE12345",
        ))),
    );
    let (h, evs) = start(&mac, quick());
    // Its program is swapped for an unsigned one and the property list is touched.
    fake(&mac).answer_full(
        &format!("codesign -dv --verbose=2 {prog}"),
        codesign_unsigned(prog),
    );
    let text = fs::read_to_string(&plist).unwrap();
    fs::write(&plist, text.replace("--background", "--bg")).unwrap();
    assert_eq!(wait_events(&evs, 1).len(), 1);
    // Touched again with the same finding: no second event.
    fs::write(&plist, text).unwrap();
    assert_eq!(quiet_for(&evs, 1.5).len(), 1);
    drop(h);
}

#[test]
fn an_installer_writing_many_files_and_several_agents_is_coalesced() {
    let mac = FakeMac::new();
    let f = fake(&mac);
    f.answer_full(
        "spctl",
        ok_stderr("accepted\nsource=Notarized Developer ID\n".into()),
    );
    f.answer_full(
        "spctl --assess --type execute /Applications/Shady.app",
        spctl_rejected("/Applications/Shady.app"),
    );
    let (h, evs) = start(&mac, quick());
    // An installer: one app with 200 files, written over a short time.
    let app = mac.env.sys_path("/Applications/Shady.app");
    mac.write(
        &app.join("Contents/Info.plist"),
        info_plist("Shady", "com.shady.app").as_bytes(),
    );
    for i in 0..200 {
        mac.write(
            &app.join(format!("Contents/Resources/r{i}.dat")),
            &[1u8; 300],
        );
    }
    let got = wait_events(&evs, 1);
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!(got[0].kind, WatchKind::NewApp);
    assert_eq!(got[0].title, "New app Gatekeeper rejects: Shady");
    assert_eq!(got[0].open, AppRoute::ThreatReview(app.clone()));
    assert_eq!(got[0].paths, vec![app.clone()]);

    // Three unsigned agents at once: one event naming them.
    evs.lock().unwrap().clear();
    for n in ["one", "two", "three"] {
        let prog = format!("/Library/Application Support/Bundle/{n}");
        add_launch_item(
            &mac,
            "~/Library/LaunchAgents",
            &format!("com.bundle.{n}"),
            &prog,
            Some(codesign_unsigned(&prog)),
        );
    }
    let got = wait_events(&evs, 1);
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!(got[0].title, "3 new startup items need a look");
    assert!(got[0].body.contains("com.bundle.one"));
    assert_eq!(got[0].paths.len(), 3);
    assert_eq!(got[0].open, AppRoute::Scan(FeatureId::Malware));
    assert_nothing_moved(&mac);
    drop(h);
}

#[test]
fn paused_watcher_drops_events_and_resumes_without_replaying() {
    let mac = FakeMac::new();
    editor_leftovers(&mac);
    let (h, evs) = start(&mac, quick());
    h.set_paused(true);
    assert!(h.is_paused() && h.status().paused);
    let app = staged_app(&mac, "Editor", "com.example.Editor");
    drag_to_trash(&mac, &app);
    let prog = "/Library/Application Support/Example/helper";
    add_launch_item(
        &mac,
        "~/Library/LaunchAgents",
        "com.example.helper",
        prog,
        Some(codesign_unsigned(prog)),
    );
    assert!(quiet_for(&evs, 1.2).is_empty());
    h.set_paused(false);
    assert!(
        quiet_for(&evs, 1.2).is_empty(),
        "nothing replays after a pause"
    );
    // New things after the pause are seen again.
    let prog2 = "/Library/Application Support/Example/other";
    add_launch_item(
        &mac,
        "~/Library/LaunchAgents",
        "com.example.other",
        prog2,
        Some(codesign_unsigned(prog2)),
    );
    assert_eq!(wait_events(&evs, 1).len(), 1);
    drop(h);
}

#[test]
fn quiet_hours_hold_events_until_they_end() {
    let mac = FakeMac::new();
    let hour = Arc::new(AtomicU8::new(23));
    let h2 = hour.clone();
    let s = WatchSettings {
        quiet_hours: Some((22, 7)),
        clock: Some(Arc::new(move || h2.load(Ordering::SeqCst))),
        ..quick()
    };
    let (h, evs) = start(&mac, s);
    let prog = "/Library/Application Support/Example/helper";
    add_launch_item(
        &mac,
        "~/Library/LaunchAgents",
        "com.example.helper",
        prog,
        Some(codesign_unsigned(prog)),
    );
    assert!(quiet_for(&evs, 1.5).is_empty());
    hour.store(7, Ordering::SeqCst);
    let got = wait_events(&evs, 1);
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!(got[0].kind, WatchKind::StartupItem);
    drop(h);
}

#[test]
fn missing_full_disk_access_is_reported_once_and_nothing_is_offered() {
    let mac = FakeMac::new();
    editor_leftovers(&mac);
    mac.env
        .inject_fault(&mac.env.user_trash(), Fault::PermissionDenied);
    let (h, evs) = start(&mac, quick());
    assert_eq!(h.status().trash, Access::NoFullDiskAccess);
    assert!(
        h.status()
            .notes
            .iter()
            .any(|n| n.contains("needs Full Disk Access")),
        "{:?}",
        h.status().notes
    );
    let app = staged_app(&mac, "Editor", "com.example.Editor");
    drag_to_trash(&mac, &app);
    let got = wait_events(&evs, 1);
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!(got[0].kind, WatchKind::NeedsFullDiskAccess);
    assert_eq!(got[0].open, AppRoute::FullDiskAccess);
    assert!(got[0].body.contains("Full Disk Access"));
    // The threat watch works without it.
    assert_eq!(h.status().threats, Access::Watching);
    drop(h);
    // Told once: a restart does not repeat it.
    let (h2, evs2) = start(&mac, quick());
    assert_eq!(h2.status().trash, Access::NoFullDiskAccess);
    assert!(quiet_for(&evs2, 1.0).is_empty());
    drop(h2);
}

#[test]
fn folders_that_appear_later_are_watched_and_turned_off_watches_stay_off() {
    let mac = FakeMac::new();
    let s = WatchSettings {
        trash_offer: false,
        ..quick()
    };
    let (h, evs) = start_bare(&mac, s);
    assert_eq!(h.status().trash, Access::Off);
    assert!(
        h.status()
            .notes
            .iter()
            .any(|n| n.contains("/Library/LaunchAgents does not exist yet")),
        "{:?}",
        h.status().notes
    );
    // ~/Library/LaunchAgents is created with an unsigned agent in it.
    let prog = "/Library/Application Support/Example/helper";
    add_launch_item(
        &mac,
        "~/Library/LaunchAgents",
        "com.example.helper",
        prog,
        Some(codesign_unsigned(prog)),
    );
    let got = wait_events(&evs, 1);
    assert_eq!(got.len(), 1, "{got:#?}");
    // The Trash is not watched.
    editor_leftovers(&mac);
    let app = staged_app(&mac, "Editor", "com.example.Editor");
    drag_to_trash(&mac, &app);
    assert_eq!(quiet_for(&evs, 1.2).len(), 1);
    drop(h);
}

#[test]
fn the_trash_plan_refuses_paths_that_are_not_an_app_folder_in_the_trash() {
    let mac = FakeMac::new();
    editor_leftovers(&mac);
    let s = Settings::default();
    // Installed, not in the Trash.
    let installed = mac.app("Editor", "com.example.Editor", "2.1");
    assert!(trashed_app_plan(&mac.env, &s, &installed).is_err());
    // A link in the Trash pointing at the installed app.
    let link = mac.env.user_trash().join("Editor.app");
    std::os::unix::fs::symlink(&installed, &link).unwrap();
    assert!(trashed_app_plan(&mac.env, &s, &link).is_err());
    // Untrusted ids: Apple's, no dot.
    for (name, id) in [("Mail", "com.apple.mail"), ("Short", "short")] {
        let a = staged_app(&mac, name, id);
        let t = drag_to_trash(&mac, &a);
        assert!(trashed_app_plan(&mac.env, &s, &t).is_err(), "{id}");
    }
    // One folder deeper in the Trash.
    let a = staged_app(&mac, "Deep", "com.example.Deep");
    let deep = mac.env.user_trash().join("folder/Deep.app");
    fs::create_dir_all(deep.parent().unwrap()).unwrap();
    fs::rename(&a, &deep).unwrap();
    assert!(trashed_app_plan(&mac.env, &s, &deep).is_err());
    assert!(executor::history(&mac.env).is_empty());
}
