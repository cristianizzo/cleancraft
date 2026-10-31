//! Threat Check's deep and full disk scans: files that can run are read with Apple's malware
//! rules (a fake XProtect.yara on the fake Mac). Signature matches outside apps are selected,
//! matches in places Threat Check does not move from are locked, and a plan edited to point at
//! a clean file outside the quick check's folders is refused when it runs.

use std::path::{Path, PathBuf};

use cleancraft_core::features::{self, malware::ThreatDepth};
use cleancraft_core::progress::Progress;
use cleancraft_core::testkit::FakeMac;
use cleancraft_core::{FeatureId, Plan, Settings, executor, xprotect};

const RULES: &str = r#"
private rule Macho { condition: uint32(0) == 0xfeedfacf or uint32(0) == 0xfeedface }
rule XProtect_TEST_A {
    meta: description = "MACOS.ADLOAD.TEST"
    strings: $a = "cleancraft-test-marker"
    condition: Macho and $a
}
"#;
const SCRIPT_RULES: &str = r#"
rule XPScripts_TEST {
    meta: description = "MACOS.OSASCRIPT.TEST"
    strings: $a = "cleancraft-script-marker"
    condition: $a
}
"#;

fn bad_macho() -> Vec<u8> {
    let mut b = vec![0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0];
    b.extend_from_slice(b"padding cleancraft-test-marker padding");
    b
}

fn mac_with_rules() -> FakeMac {
    let m = FakeMac::new();
    let dir = m.env.sys_path(xprotect::DIRS[0]);
    m.write(&dir.join(xprotect::FILE_RULES), RULES.as_bytes());
    m.write(&dir.join(xprotect::SCRIPT_RULES), SCRIPT_RULES.as_bytes());
    m
}

fn scan(m: &FakeMac, depth: ThreatDepth) -> Plan {
    features::scan_metered(
        &m.env,
        &Settings::default(),
        FeatureId::Malware,
        depth,
        &Progress::new(),
    )
    .unwrap()
}

fn find<'a>(p: &'a Plan, path: &Path) -> Option<&'a cleancraft_core::Item> {
    p.items.iter().find(|i| i.path == path)
}

fn put(m: &FakeMac, path: PathBuf, bytes: &[u8]) -> PathBuf {
    m.write(&path, bytes);
    path
}

#[test]
fn deep_scan_finds_matching_programs_and_selects_only_safe_ones() {
    let m = mac_with_rules();
    let dl = put(&m, m.env.home_path("Downloads/Installer"), &bad_macho());
    let lib = put(
        &m,
        m.env.home_path("Library/Application Support/Helper/agent"),
        &bad_macho(),
    );
    let doc = put(&m, m.env.home_path("Documents/tool"), &bad_macho());
    let script = put(
        &m,
        m.env.home_path("Downloads/run.sh"),
        b"#!/bin/sh\necho cleancraft-script-marker\n",
    );
    // Not a program: never read with the rules.
    let text = put(
        &m,
        m.env.home_path("Downloads/notes.txt"),
        b"cleancraft-test-marker",
    );
    // Inside an app: the app is listed, never selected.
    let app_bin = put(
        &m,
        m.env
            .sys_path("/Applications/Shady.app/Contents/MacOS/Shady"),
        &bad_macho(),
    );
    // In the quarantine and the Trash: never read.
    let q = put(&m, m.env.quarantine_dir().join("old/agent"), &bad_macho());
    let trash = put(&m, m.env.home_path(".Trash/agent"), &bad_macho());

    let quick = scan(&m, ThreatDepth::Quick);
    assert!(find(&quick, &dl).is_none(), "quick reads no file contents");

    let p = scan(&m, ThreatDepth::Deep);
    let d = find(&p, &dl).expect("Downloads finding");
    assert_eq!(d.category, "content");
    assert!(d.selected && d.locked.is_none(), "{d:#?}");
    assert!(d.reason.contains("Adload (adware)"), "{}", d.reason);
    assert!(d.reason.contains("MACOS.ADLOAD.TEST"), "{}", d.reason);
    let l = find(&p, &lib).expect("Library finding");
    assert!(l.selected && l.locked.is_none(), "{l:#?}");
    let o = find(&p, &doc).expect("Documents finding");
    assert!(!o.selected && o.locked.is_some(), "{o:#?}");
    let s = find(&p, &script).expect("script finding");
    assert!(!s.selected, "script matches are never selected: {s:#?}");
    assert!(find(&p, &text).is_none());
    let app = m.env.sys_path("/Applications/Shady.app");
    assert!(find(&p, &app_bin).is_none());
    let a = find(&p, &app).expect("app finding");
    assert!(
        !a.selected && a.reason.contains("Contents/MacOS/Shady"),
        "{a:#?}"
    );
    assert!(find(&p, &q).is_none() && find(&p, &trash).is_none());
    assert!(
        p.notes.iter().any(|n| n.contains("Apple's malware rules")),
        "{:?}",
        p.notes
    );

    // The selected ones go to the quarantine, and come back with undo.
    let rep = features::run_plan(
        &m.env,
        &Settings::default(),
        &p,
        &p.digest(),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(rep.skipped.is_empty(), "{:?}", rep.skipped);
    assert_eq!(rep.moved.len(), 2);
    assert!(!dl.exists() && !lib.exists());
    assert!(doc.exists() && script.exists() && app_bin.exists());
    let (n, errs) = executor::undo_plan(&m.env, &p.id);
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(n, 2);
    assert!(dl.exists() && lib.exists());
}

#[test]
fn a_plan_pointing_at_a_clean_file_outside_the_quick_folders_is_refused() {
    let m = mac_with_rules();
    let bad = put(&m, m.env.home_path("Downloads/Installer"), &bad_macho());
    let clean = put(
        &m,
        m.env.home_path("Library/Preferences/clean"),
        &[0xcf, 0xfa, 0xed, 0xfe, 1, 2, 3],
    );
    let mut p = scan(&m, ThreatDepth::Deep);
    let id = find(&p, &bad).expect("finding").id;
    // A changed plan: the item now points at a clean file.
    p.items[id].path = clean.clone();
    let rep = features::run_plan(
        &m.env,
        &Settings::default(),
        &p,
        &p.digest(),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(rep.moved.is_empty(), "{:?}", rep.moved);
    assert_eq!(rep.skipped.len(), 1);
    let why = format!("{:?}", rep.skipped[0].1);
    assert!(why.contains("no longer matches"), "{why}");
    assert!(clean.exists());
}

#[test]
fn full_disk_reads_usr_local_and_leaves_macos_alone() {
    let m = mac_with_rules();
    let local = put(&m, m.env.sys_path("/usr/local/bin/tool"), &bad_macho());
    let opt = put(&m, m.env.sys_path("/opt/thing/bin/tool"), &bad_macho());
    let system = put(&m, m.env.sys_path("/System/Library/tool"), &bad_macho());
    let usr = put(&m, m.env.sys_path("/usr/libexec/tool"), &bad_macho());
    let deep = scan(&m, ThreatDepth::Deep);
    assert!(
        find(&deep, &local).is_none(),
        "deep does not read /usr/local"
    );
    let p = scan(&m, ThreatDepth::Full);
    assert!(find(&p, &local).is_some_and(|i| i.locked.is_none()));
    assert!(find(&p, &opt).is_some_and(|i| i.locked.is_none()));
    assert!(find(&p, &system).is_none() && find(&p, &usr).is_none());
}

#[test]
fn without_apple_rules_the_scan_says_so_and_keeps_the_quick_findings() {
    let m = FakeMac::new();
    put(&m, m.env.home_path("Downloads/Installer"), &bad_macho());
    let p = scan(&m, ThreatDepth::Deep);
    assert!(p.items.iter().all(|i| i.category != "content"));
    assert!(
        p.notes
            .iter()
            .any(|n| n.contains("were not found on this Mac")),
        "{:?}",
        p.notes
    );
}
