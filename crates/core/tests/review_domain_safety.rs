//! Domain QA review (reviewer "domain"): attempts to make CleanCraft move what it must never
//! move, on a fake Mac. Ignored tests prove open defects (DOM-n, see
//! docs/agents/reports/domain.md); the others protect behaviour checked and found right.
//!
//! Run the defects: `cargo test -p cleancraft-core --test review_domain_safety -- --ignored`.
//!
//! "Edited plan" means what a CLI user or an AI agent can do: a saved plan is plain JSON in
//! `<data>/plans/<id>.json`; `cleancraft clean <id> --yes` and the MCP tool `execute_plan`
//! load it again, and its digest is a plain hash of the file's own content (MCP `get_plan`
//! even returns the new digest). So the only fences left are each feature's allow-list,
//! `policy::check` and `Feature::prepare`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use cleancraft_core::features::{
    self, ScanCtx, duplicates, large_old, large_old::Location, malware, space_lens, system_junk,
    uninstaller,
};
use cleancraft_core::fsutil::tree_size;
use cleancraft_core::testkit::FakeMac;
use cleancraft_core::testkit_cleanup::{junk_mac, mail_mac, privacy_mac};
use cleancraft_core::testkit_perf::startup_mac;
use cleancraft_core::{Env, FeatureId, Plan, Report, Risk, Settings, executor, plans, policy};

// ----------------------------------------------------------------------------------------
// helpers

fn ctx_scan(
    env: &Env,
    settings: &Settings,
    f: impl FnOnce(&ScanCtx<'_>) -> anyhow::Result<Plan>,
) -> Plan {
    let cancel = AtomicBool::new(false);
    let ctx = ScanCtx {
        env,
        settings,
        cancel: &cancel,
        progress: &|_| {},
        meter: &cleancraft_core::progress::Progress::new(),
    };
    f(&ctx).expect("scan")
}

fn small_files() -> Settings {
    Settings {
        large_min_bytes: 1_000,
        ..Settings::default()
    }
}

/// What a person or an agent does to a saved plan file: load it, change it, write it back.
/// Returns the digest the MCP `get_plan` tool would now report for it.
fn edit_saved_plan(env: &Env, plan: &Plan, edit: impl FnOnce(&mut Plan)) -> String {
    plans::save(env, plan).expect("save plan");
    let mut p = plans::load(env, &plan.id).expect("load plan");
    edit(&mut p);
    plans::save(env, &p).expect("write edited plan");
    plans::load(env, &plan.id).expect("reload").digest()
}

/// The steps of the MCP tool `execute_plan` (crates/mcp/src/lib.rs) after its settings check,
/// copied line for line: load the saved plan, compare the digest, apply `only_items`, run.
/// (fix2, DOM-5: mcp/lib.rs now narrows with `features::narrow_selection`, replayed here.)
fn mcp_execute_plan(
    env: &Env,
    plan_id: &str,
    digest: &str,
    only_items: Option<Vec<usize>>,
) -> Result<Report, String> {
    let settings = Settings::load(env);
    let mut plan = plans::load(env, plan_id).map_err(|e| e.to_string())?;
    if plan.digest() != digest {
        return Err("Refused: the digest does not match this plan.".into());
    }
    if let Some(only) = &only_items {
        features::narrow_selection(&mut plan, only)?;
    }
    let digest = plan.digest();
    features::run_plan(env, &settings, &plan, &digest, &mut |_, _| {}).map_err(|e| format!("{e:#}"))
}

/// `cleancraft clean <id> --yes` (apps/cleancraft/src/cli.rs): load and run with its own digest.
fn cli_clean(env: &Env, plan_id: &str) -> anyhow::Result<Report> {
    let settings = Settings::load(env);
    let plan = plans::load(env, plan_id)?;
    features::run_plan(env, &settings, &plan, &plan.digest(), &mut |_, _| {})
}

fn push(plan: &mut Plan, p: &Path, cat: &str) -> usize {
    plan.push(
        p.to_path_buf(),
        tree_size(p).0,
        cat,
        "edited in",
        Risk::Low,
        true,
    )
}

fn shown(env: &Env, ps: &[&PathBuf]) -> Vec<String> {
    ps.iter().map(|p| env.display(p)).collect()
}

// ----------------------------------------------------------------------------------------
// DOM-1: the protected list compares paths letter case by letter case.

#[test]
fn dom1_case_variant_of_documents_is_still_documents() {
    // macOS volumes are case-insensitive by default: ~/documents IS ~/Documents. The fake
    // home is on a case-sensitive disk, so a folder spelled "documents" stands for the same
    // folder the way macOS would resolve it.
    let m = FakeMac::new();
    let env = &m.env;
    let secret = m.home_file("documents/tax-return-2025.pdf", 200_000);
    let settings = small_files();

    // Large & Old Files > location > typed folder "~/documents".
    if let Ok(folder) = large_old::parse_folder(env, "~/documents") {
        let mut plan = ctx_scan(env, &settings, |c| {
            large_old::scan_location(c, &Location::Folder(folder.clone()))
        });
        for i in 0..plan.items.len() {
            plan.set_selected(i, true);
        }
        let _ = features::run_plan(env, &settings, &plan, &plan.digest(), &mut |_, _| {});
    }
    assert!(
        secret.exists(),
        "a file in ~/documents was moved: that is the Documents folder on a default Mac"
    );

    // Disk Map: typed folder and the lock on its items.
    let in_docs = env.home_path("documents/tax-return-2025.pdf");
    assert!(
        cleancraft_core::space::lock_reason(env, &in_docs).is_some(),
        "Disk Map lets ~/documents/... be ticked and removed"
    );
    // Every protected place, in another case.
    for rel in [
        "DESKTOP/notes.txt",
        "documents/x.pdf",
        "Library/mobile documents/com~apple~CloudDocs/x.pdf",
        "library/Keychains/login.keychain-db",
        "Library/Application Support/cleancraft/journal.jsonl",
        "Library/messages/chat.db",
    ] {
        assert!(
            policy::protected_reason(env, &env.home_path(rel)).is_some(),
            "~/{rel} is not protected"
        );
    }
}

// ----------------------------------------------------------------------------------------
// DOM-2: a folder that CONTAINS a protected place passes the fence.

#[test]
fn dom2_folder_holding_a_protected_place_is_not_protected() {
    let m = FakeMac::new();
    let env = &m.env;
    let library = m.home_file(
        "Pictures/Photos Library.photoslibrary/database/Photos.sqlite",
        300_000,
    );
    m.home_file("Pictures/holiday.jpg", 100_000);
    let pictures = env.home_path("Pictures");
    let s = Settings::default();

    // Disk Map: tick "Pictures" in the map, Review and Remove.
    let plan = space_lens::plan_for(env, &[(pictures.clone(), tree_size(&pictures).0)]);
    let _ = features::run_plan(env, &s, &plan, &plan.digest(), &mut |_, _| {});
    let photos_ok = library.exists();

    // Large & Old Files, edited plan: the whole ~/Library (keychains, iCloud Drive,
    // CleanCraft's own journal are inside it).
    let keychain = m.home_file("Library/Keychains/login.keychain-db", 10_000);
    let icloud = m.home_file(
        "Library/Mobile Documents/com~apple~CloudDocs/cv.pdf",
        10_000,
    );
    let base = ctx_scan(env, &small_files(), |c| {
        large_old::scan_location(c, &Location::Downloads)
    });
    let lib = env.home_path("Library");
    let digest = edit_saved_plan(env, &base, |p| {
        push(p, &lib, "other");
    });
    let _ = mcp_execute_plan(env, &base.id, &digest, None);
    assert!(
        photos_ok,
        "the Photos library was moved to the Trash together with ~/Pictures"
    );
    assert!(
        keychain.exists() && icloud.exists(),
        "~/Library was moved: keychains and iCloud Drive went with it"
    );
}

// ----------------------------------------------------------------------------------------
// DOM-3: Large & Old Files (and Duplicates) may remove anything in the home folder.

#[test]
fn dom3_large_old_allow_list_is_wider_than_the_row() {
    let m = FakeMac::new();
    let env = &m.env;
    m.home_file("Downloads/big.iso", 200_000);
    let ssh = m.home_file(".ssh/id_ed25519", 400);
    let pop = m.home_file(
        "Library/Mail/V10/POP-me@pop.example/INBOX.mbox/0F/Data/Messages/1.emlx",
        5_000,
    );
    let contacts = m.home_file(
        "Library/Application Support/AddressBook/AddressBook-v22.abcddb",
        5_000,
    );
    let music = m.home_file(
        "Music/Music/Music Library.musiclibrary/Library.musicdb",
        5_000,
    );
    let pop_mbox = env.home_path("Library/Mail/V10/POP-me@pop.example/INBOX.mbox");
    let plan = ctx_scan(env, &small_files(), |c| {
        large_old::scan_location(c, &Location::Downloads)
    });
    let targets = [&ssh, &pop_mbox, &contacts, &music];
    let digest = edit_saved_plan(env, &plan, |p| {
        for t in targets {
            push(p, t, "other");
        }
    });
    let rep = mcp_execute_plan(env, &plan.id, &digest, None);
    let moved: Vec<&PathBuf> = [&ssh, &pop, &contacts, &music]
        .into_iter()
        .filter(|p| !p.exists())
        .collect();
    assert!(
        moved.is_empty(),
        "Large & Old Files moved {:?} ({rep:?}); FEATURES C1: never Library, hidden files, Music library",
        shown(env, &moved)
    );
}

// ----------------------------------------------------------------------------------------
// DOM-4: the last copy of a duplicate set.

#[test]
fn dom4_duplicates_core_check_trusts_the_group_field() {
    let m = FakeMac::new();
    let env = &m.env;
    let a = m.home_file("Downloads/contract.pdf", 300_000);
    let b = env.home_path("Downloads/contract copy.pdf");
    fs::copy(&a, &b).unwrap();
    let plan = ctx_scan(env, &Settings::default(), |c| {
        duplicates::scan_location(c, &Location::Downloads)
    });
    assert_eq!(plan.items.len(), 2, "the two copies form one set");
    let digest = edit_saved_plan(env, &plan, |p| {
        for it in p.items.iter_mut() {
            it.selected = true;
            it.group = None;
        }
    });
    let rep = mcp_execute_plan(env, &plan.id, &digest, None);
    assert!(
        a.exists() || b.exists(),
        "both copies were moved ({rep:?}); FEATURES C2: one copy per set always kept (core check before any move)"
    );
}

// ----------------------------------------------------------------------------------------
// DOM-5: MCP `only_items` is applied after the digest check.

#[test]
fn dom5_mcp_only_items_widens_the_confirmed_selection() {
    let m = privacy_mac();
    let env = &m.env;
    let plan = features::scan(env, &Settings::default(), FeatureId::Privacy).unwrap();
    plans::save(env, &plan).unwrap();
    // The digest `scan` returned and the user confirmed (the default selection).
    let confirmed = plan.digest();
    let pw = plan
        .items
        .iter()
        .find(|i| i.path.ends_with("Default/Login Data"))
        .expect("Chrome saved passwords listed");
    assert!(!pw.selected, "passwords are never preselected");
    let login_data = pw.path.clone();
    let rep = mcp_execute_plan(env, &plan.id, &confirmed, Some(vec![pw.id]));
    assert!(
        login_data.exists(),
        "saved passwords moved though the confirmed list did not have them ({rep:?})"
    );
}

// ----------------------------------------------------------------------------------------
// DOM-6: System Junk locks are scan-time only.

#[test]
fn dom6_system_junk_locks_and_icloud_cache_are_not_fenced() {
    let m = junk_mac();
    let env = &m.env;
    let s = Settings::default();
    let plan = features::scan(env, &s, FeatureId::SystemJunk).unwrap();
    let newest = plan
        .items
        .iter()
        .find(|i| i.locked.as_deref() == Some(system_junk::LOCK_NEWEST))
        .expect("newest backup listed locked")
        .path
        .clone();
    let bird = env.home_path("Library/Caches/com.apple.bird");
    assert!(bird.exists());
    let digest = edit_saved_plan(env, &plan, |p| {
        for it in p.items.iter_mut() {
            it.selected = false;
            if it.path == newest {
                it.locked = None;
                it.selected = true;
            }
        }
        push(p, &bird, "user_caches");
    });
    let rep = mcp_execute_plan(env, &plan.id, &digest, None);
    assert!(
        newest.exists(),
        "the newest backup of the iPhone was moved ({rep:?})"
    );
    assert!(bird.exists(), "the iCloud cache com.apple.bird was moved");
}

// ----------------------------------------------------------------------------------------
// DOM-7: Apple apps.

#[test]
fn dom7_apple_app_moves_when_the_category_is_edited() {
    let m = FakeMac::new();
    let env = &m.env;
    let pages = m.app("Pages", "com.apple.iWork.Pages", "14.0");
    let numbers = m.app("Numbers", "com.apple.iWork.Numbers", "14.0");
    let s = Settings::default();

    let mut p = Plan::new(FeatureId::Uninstaller);
    push(&mut p, &pages, uninstaller::CAT_HIGH);
    plans::save(env, &p).unwrap();
    let _ = cli_clean(env, &p.id);

    let mut q = Plan::new(FeatureId::Malware);
    push(&mut q, &numbers, "apps");
    let _ = features::run_plan(env, &s, &q, &q.digest(), &mut |_, _| {});
    let _ = malware::Malware(Default::default());

    assert!(
        pages.exists(),
        "Pages (com.apple.*) moved by an App Remover plan; FEATURES A3: never removable"
    );
    assert!(
        numbers.exists(),
        "Numbers (com.apple.*) quarantined by a Threat Check plan"
    );
}

// ----------------------------------------------------------------------------------------
// DOM-8: open files whose name has " (".

#[test]
fn dom8_lsof_names_with_parenthesis_are_cut() {
    let out = "p812\nfcwd\nn/Users/tester/Downloads/Invoice (1).pdf\np900\nn/Users/tester/Movies/Trip (edit).mov\n";
    let open = cleancraft_core::sys::parse_lsof(out);
    for p in [
        "/Users/tester/Downloads/Invoice (1).pdf",
        "/Users/tester/Movies/Trip (edit).mov",
    ] {
        assert!(
            open.contains(Path::new(p)),
            "{p} not in the open set: {open:?}"
        );
    }
}

// ----------------------------------------------------------------------------------------
// DOM-9: two copies of the same app.

#[test]
fn dom9_second_copy_of_an_app_takes_the_shared_data() {
    let m = FakeMac::new();
    let env = &m.env;
    m.app("Foo", "com.example.Foo", "2.0");
    let old = m.app("Foo 1.9", "com.example.Foo", "1.9");
    let prefs = m.home_file("Library/Preferences/com.example.Foo.plist", 4_000);
    let data = m.home_file(
        "Library/Application Support/com.example.Foo/db.sqlite",
        90_000,
    );
    let plan = uninstaller::uninstall_plan(env, &Settings::default(), &old, false).unwrap();
    let preselected: Vec<&PathBuf> = plan
        .selected()
        .filter(|i| i.path != old)
        .map(|i| &i.path)
        .collect();
    assert!(
        preselected.is_empty(),
        "selected for removal although Foo.app (same id) stays: {:?}",
        shown(env, &preselected)
    );
    let _ = (prefs, data);
}

// ----------------------------------------------------------------------------------------
// DOM-10, DOM-11: the ignore list.

#[test]
fn dom10_ignored_app_is_still_offered_for_removal() {
    let m = FakeMac::new();
    let env = &m.env;
    let app = m.app("Keep Me", "com.example.KeepMe", "1.0");
    m.home_file("Library/Caches/com.example.KeepMe/c", 10_000);
    let s = Settings {
        ignore: vec![app.clone()],
        ..Settings::default()
    };
    let plan = uninstaller::uninstall_plan(env, &s, &app, false).unwrap();
    assert!(
        plan.selected().all(|i| i.path != app),
        "the ignored app bundle is selected for removal; S11/Settings: scans never propose ignored paths"
    );
}

#[test]
fn dom11_ignore_entry_in_other_case_is_not_honoured() {
    let m = FakeMac::new();
    let env = &m.env;
    let f = m.home_file("Downloads/movie.mkv.crdownload", 50_000);
    // Settings > Ignore list > "~/downloads/movie.mkv.crdownload" (what the page stores).
    let s = Settings {
        ignore: vec![env.home_path("downloads/movie.mkv.crdownload")],
        ..Settings::default()
    };
    let plan = features::scan(env, &s, FeatureId::SystemJunk).unwrap();
    assert!(
        plan.items.iter().all(|i| i.path != f),
        "proposed although it is on the ignore list (same file on a default Mac)"
    );
}

// ----------------------------------------------------------------------------------------
// DOM-12: "broken" startup items whose program is on a disk that is not connected.

#[test]
fn dom12_agent_on_unplugged_disk_is_not_broken() {
    let m = FakeMac::new();
    let env = &m.env;
    let plist = env.home_path("Library/LaunchAgents/com.example.backup-sync.plist");
    m.write(
        &plist,
        cleancraft_core::testkit_perf::launch_plist(
            "com.example.backup-sync",
            "/Volumes/Backup/tools/sync",
            true,
        )
        .as_bytes(),
    );
    let plan = features::scan(env, &Settings::default(), FeatureId::SystemJunk).unwrap();
    let it = plan.items.iter().find(|i| i.path == plist);
    assert!(
        it.is_none_or(|i| !i.selected),
        "preselected as broken although /Volumes/Backup is only unplugged: {:?}",
        it.map(|i| &i.reason)
    );
}

// ----------------------------------------------------------------------------------------
// Checked and found right.

#[test]
fn protected_places_are_refused_even_for_home_wide_allow_lists() {
    let m = FakeMac::new();
    let env = &m.env;
    let home = vec![env.home().to_path_buf()];
    for rel in [
        "Documents",
        "Documents/report.pdf",
        "Desktop/notes.txt",
        "Library/Mobile Documents/com~apple~CloudDocs/cv.pdf",
        "Library/CloudStorage/Dropbox/x",
        "Pictures/Photos Library.photoslibrary",
        "Pictures/Old/iPhoto Library.photolibrary/x",
        "Library/Keychains/login.keychain-db",
        "Library/Application Support/CleanCraft/journal.jsonl",
        "Library/Application Support/CleanCraft/Quarantine/x",
        "Library/Application Support/CleanCraft/settings.json",
        "Library/Messages/chat.db",
        "Library/Group Containers/group.com.apple.notes/NoteStore.sqlite",
    ] {
        let p = env.home_path(rel);
        assert!(
            matches!(
                policy::check(env, &home, &p),
                Err(policy::Refusal::Protected(_))
            ),
            "{rel}"
        );
    }
    assert!(policy::check(env, &home, env.home()).is_err());
    assert!(
        policy::check(
            env,
            &home,
            &env.home_path("Library/Caches/../../Documents/x")
        )
        .is_err()
    );
    assert!(policy::check(env, &home, Path::new("Documents/x")).is_err());
    assert!(policy::check(env, &home, &env.sys_path("/System/Library/x")).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_and_hard_link_into_documents_never_reach_documents() {
    let m = FakeMac::new();
    let env = &m.env;
    let doc = m.home_file("Documents/thesis/thesis.pdf", 300_000);
    let docs_dir = env.home_path("Documents/thesis");
    let link = env.home_path("Downloads/thesis-link");
    std::os::unix::fs::symlink(&docs_dir, &link).unwrap();
    let hard = env.home_path("Downloads/thesis-hard.pdf");
    fs::hard_link(&doc, &hard).unwrap();
    let s = small_files();

    // Large & Old Files of Downloads never lists the Documents file through the link.
    let plan = ctx_scan(env, &s, |c| {
        large_old::scan_location(c, &Location::Downloads)
    });
    assert!(
        plan.items.iter().all(|i| !i.path.starts_with(&link)),
        "{:?}",
        plan.items
    );
    // Duplicates ignore the hard link pair.
    let d = ctx_scan(env, &Settings::default(), |c| {
        duplicates::scan_location(c, &Location::Home)
    });
    assert!(d.items.iter().all(|i| i.path != hard && i.path != doc));

    // An edited plan through the link is refused; the link itself and the hard link may go,
    // and the Documents file stays whole.
    let digest = edit_saved_plan(env, &plan, |p| {
        push(p, &link.join("thesis.pdf"), "other");
        push(p, &link, "other");
        push(p, &hard, "other");
    });
    let rep = mcp_execute_plan(env, &plan.id, &digest, None).unwrap();
    assert!(
        rep.skipped
            .iter()
            .any(|(p, _)| p == &link.join("thesis.pdf"))
    );
    assert!(doc.exists());
    assert_eq!(fs::metadata(&doc).unwrap().len(), 300_000);
    assert!(!link.exists() && fs::symlink_metadata(&link).is_err());
    // Undo puts the link back, still pointing at Documents.
    let (n, errs) = executor::undo_plan(env, &plan.id);
    assert!(errs.is_empty(), "{errs:?}");
    assert!(n >= 2);
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(hard.exists());
}

#[test]
fn pop_and_on_my_mac_attachments_are_never_proposed_or_moved() {
    let m = mail_mac();
    let env = &m.env;
    let s = Settings::default();
    let plan = features::scan(env, &s, FeatureId::MailAttachments).unwrap();
    for it in &plan.items {
        let p = env.display(&it.path);
        assert!(!p.contains("/POP-") && !p.contains("/Mailboxes/"), "{p}");
    }
    let only_copy: Vec<PathBuf> = ["POP-old@pop.example", "Mailboxes"]
        .iter()
        .map(|a| env.home_path(&format!("Library/Mail/V10/{a}")))
        .collect();
    let mut found = Vec::new();
    for root in &only_copy {
        cleancraft_core::fsutil::walk_files(root, &|_| false, &mut found);
    }
    let files: Vec<PathBuf> = found.into_iter().map(|(p, _)| p).collect();
    assert_eq!(files.len(), 2, "{files:?}");
    let digest = edit_saved_plan(env, &plan, |p| {
        for f in &files {
            push(p, f, "attachments");
        }
    });
    let rep = mcp_execute_plan(env, &plan.id, &digest, None).unwrap();
    for f in &files {
        assert!(f.exists(), "{} moved ({rep:?})", f.display());
    }
}

#[test]
fn privacy_never_preselects_passwords_bookmarks_or_autofill() {
    let m = privacy_mac();
    let env = &m.env;
    let plan = features::scan(env, &Settings::default(), FeatureId::Privacy).unwrap();
    for it in plan.selected() {
        let name = it.path.file_name().unwrap().to_string_lossy();
        assert!(
            !["Login Data", "logins.json", "Web Data", "places.sqlite"].contains(&name.as_ref()),
            "{} preselected",
            env.display(&it.path)
        );
    }
    for it in &plan.items {
        let p = it.path.to_string_lossy();
        assert!(
            !p.ends_with("Bookmarks")
                && !p.ends_with("Bookmarks.plist")
                && !p.ends_with("Preferences"),
            "{p} listed"
        );
    }
    assert!(
        plan.items
            .iter()
            .any(|i| i.path.ends_with("Default/Login Data") && !i.selected),
        "passwords are listed, not selected"
    );
}

#[test]
fn ignore_list_is_honoured_by_every_plan_scan() {
    fn check(m: &FakeMac, f: FeatureId) {
        let env = &m.env;
        let plan = features::scan(env, &Settings::default(), f).unwrap();
        assert!(!plan.items.is_empty(), "{f:?} fixture finds something");
        let s = Settings {
            ignore: plan.items.iter().map(|i| i.path.clone()).collect(),
            ..Settings::default()
        };
        let again = features::scan(env, &s, f).unwrap();
        let back: Vec<String> = again
            .items
            .iter()
            .filter(|i| s.is_ignored(&i.path))
            .map(|i| env.display(&i.path))
            .collect();
        assert!(back.is_empty(), "{f:?} proposes ignored paths: {back:?}");
    }
    check(&junk_mac(), FeatureId::SystemJunk);
    check(&mail_mac(), FeatureId::MailAttachments);
    check(&privacy_mac(), FeatureId::Privacy);
    let st = startup_mac();
    check(&st, FeatureId::LoginItems);
    check(&st, FeatureId::Malware);
    let files = FakeMac::new();
    let a = files.home_file("Downloads/a.bin", 300_000);
    fs::copy(&a, files.env.home_path("Movies/a copy.bin")).unwrap();
    check(&files, FeatureId::Duplicates);
}

#[test]
fn every_plan_module_puts_back_what_it_moved() {
    fn round(m: &FakeMac, f: FeatureId) {
        let env = &m.env;
        let s = Settings::default();
        let mut plan = features::scan(env, &s, f).unwrap();
        if plan.selected_count() == 0 {
            let first = plan
                .items
                .iter()
                .find(|i| i.locked.is_none() && i.path.exists())
                .map(|i| i.id)
                .expect("something selectable");
            plan.set_selected(first, true);
        }
        let before: Vec<(PathBuf, u64)> = plan
            .selected()
            .map(|i| (i.path.clone(), tree_size(&i.path).0))
            .collect();
        let rep = features::run_plan(env, &s, &plan, &plan.digest(), &mut |_, _| {}).unwrap();
        assert!(!rep.moved.is_empty(), "{f:?}: {:?}", rep.skipped);
        let (n, errs) = executor::undo_plan(env, &plan.id);
        assert!(errs.is_empty(), "{f:?}: {errs:?}");
        assert_eq!(n, rep.moved.len(), "{f:?}");
        for (p, size) in before {
            if rep.skipped.iter().any(|(s, _)| s == &p) {
                continue;
            }
            assert_eq!(tree_size(&p).0, size, "{f:?} {}", p.display());
        }
    }
    round(&junk_mac(), FeatureId::SystemJunk);
    round(&mail_mac(), FeatureId::MailAttachments);
    round(&privacy_mac(), FeatureId::Privacy);
    let files = FakeMac::new();
    let a = files.home_file("Downloads/a.bin", 300_000);
    fs::copy(&a, files.env.home_path("Downloads/b.bin")).unwrap();
    round(&files, FeatureId::Duplicates);
}

#[test]
fn trash_bins_and_feature_less_plans_never_move() {
    let m = FakeMac::new();
    let env = &m.env;
    let t = m.home_file(".Trash/old.zip", 10_000);
    for f in [
        FeatureId::TrashBins,
        FeatureId::Maintenance,
        FeatureId::Monitor,
    ] {
        let mut p = Plan::new(f);
        push(&mut p, &t, "x");
        let _ = features::run_plan(env, &Settings::default(), &p, &p.digest(), &mut |_, _| {});
        assert!(t.exists(), "{f:?}");
    }
    let mut p = Plan::new(FeatureId::SystemJunk);
    push(&mut p, &t, "x");
    p.feature = None;
    assert!(
        features::run_plan(env, &Settings::default(), &p, &p.digest(), &mut |_, _| {}).is_err()
    );
    assert!(t.exists());
}
