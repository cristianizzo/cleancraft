//! threat2: what Threat Check recognizes threats with. Apple's rules: their version and age,
//! the weekly notification when they are old, and the update through macOS. abuse.ch: the key,
//! the daily list from a fake `recent.csv`, the key never in errors, and deep scans finding
//! exact copies of listed samples (a program and a disk image).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::NaiveDateTime;
use cleancraft_core::features::{self, malware::ThreatDepth};
use cleancraft_core::progress::Progress;
use cleancraft_core::schedule::Agent;
use cleancraft_core::sweep::Sources;
use cleancraft_core::sys::CmdOut;
use cleancraft_core::testkit::FakeMac;
use cleancraft_core::{FeatureId, Plan, Settings, executor, known_malware, self_update, xprotect};

const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";
const RULES: &str =
    r#"rule XProtect_NOTHING { strings: $a = "never-in-any-test-file" condition: $a }"#;

fn url() -> String {
    format!(
        "/usr/bin/curl -fsS --max-time 60 https://mb-api.abuse.ch/v2/files/exports/{KEY}/recent.csv"
    )
}

fn mac_with_rules(days_old: u64) -> FakeMac {
    let m = FakeMac::new();
    let dir = m.env.sys_path(xprotect::DIRS[0]);
    m.write(&dir.join(xprotect::FILE_RULES), RULES.as_bytes());
    m.write(
        &dir.join("../Info.plist"),
        br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>CFBundleShortVersionString</key><string>5368</string></dict></plist>"#,
    );
    cleancraft_core::testkit::age(&dir.join(xprotect::FILE_RULES), days_old);
    m
}

fn csv(rows: &[(&str, &str, &str)]) -> String {
    let mut s = String::from(
        "# \"first_seen_utc\",\"sha256_hash\",\"md5_hash\",\"sha1_hash\",\"reporter\",\"file_name\",\"file_type_guess\",\"mime_type\",\"signature\"\n",
    );
    for (sha, kind, family) in rows {
        s.push_str(&format!(
            "\"2026-10-30 10:00:00\", \"{sha}\", \"x\", \"x\", \"r\", \"f\", \"{kind}\", \"m\", \"{family}\"\n"
        ));
    }
    s
}

fn sha(p: &Path) -> String {
    self_update::sha256_file(p).expect("hash")
}

fn at(s: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").expect("time")
}

#[test]
fn apples_rules_show_their_version_and_age() {
    let m = mac_with_rules(3);
    let v = xprotect::version(&m.env).expect("rules");
    assert_eq!(v.version.as_deref(), Some("5368"));
    let now = SystemTime::now();
    assert_eq!(v.age_days(now), Some(3));
    assert!(!v.is_old(now));
    assert_eq!(
        v.text(now),
        "Apple's malware rules: version 5368, 3 days old."
    );
    let later = now + Duration::from_secs(60 * 86_400);
    assert!(v.is_old(later));
    assert!(xprotect::version(&FakeMac::new().env).is_none());
}

#[test]
fn old_rules_notify_once_a_week_and_open_threat_check() {
    let m = mac_with_rules(60);
    let fake = m.env.fake_sys().expect("fake");
    fake.answer("osascript", "");
    let s = Settings::default();
    let mut a = Agent::new(&m.env, Sources::registered());
    let wall = SystemTime::now();
    let mut rep = Default::default();
    a.rules_alert(&m.env, &s, at("2026-10-20 10:00"), wall, &mut rep);
    assert_eq!(rep.shown.len(), 1, "{:?}", rep.failed);
    assert_eq!(rep.shown[0].title, "Apple's malware rules are 60 days old");
    assert_eq!(rep.shown[0].route.as_deref(), Some("malware"));
    let menu = a.menu(None, &s);
    assert_eq!(
        menu.reviews[0].text,
        "Review: Apple's malware rules are 60 days old"
    );
    // Not again within the week.
    let mut rep = Default::default();
    a.rules_alert(&m.env, &s, at("2026-10-26 10:00"), wall, &mut rep);
    assert!(rep.shown.is_empty());
    let mut rep = Default::default();
    a.rules_alert(&m.env, &s, at("2026-10-27 10:00"), wall, &mut rep);
    assert_eq!(rep.shown.len(), 1);
    // Fresh rules: nothing.
    let fresh = mac_with_rules(2);
    fresh.env.fake_sys().expect("fake").answer("osascript", "");
    let mut a = Agent::new(&fresh.env, Sources::registered());
    let mut rep = Default::default();
    a.rules_alert(&fresh.env, &s, at("2026-10-20 10:00"), wall, &mut rep);
    assert!(rep.shown.is_empty());
}

#[test]
fn updating_apples_rules_goes_through_macos() {
    // Older macOS without the xprotect tool: Software Update opens.
    let m = mac_with_rules(60);
    let fake = m.env.fake_sys().expect("fake");
    let said = xprotect::update(&m.env).expect("opened");
    assert!(said.starts_with("Software Update is open."), "{said}");
    fake.with(|s| {
        assert_eq!(
            s.opened_urls,
            vec![xprotect::SOFTWARE_UPDATE_URL.to_string()]
        )
    });
    assert!(fake.admin_calls().is_empty());
    // With the tool: `xprotect update` with the administrator password.
    m.write(&m.env.sys_path(xprotect::TOOL), b"#!/bin/sh\n");
    fake.answer("admin:/usr/bin/xprotect update", "");
    assert_eq!(
        xprotect::update(&m.env).as_deref(),
        Ok("macOS checked for new malware rules.")
    );
    assert_eq!(
        fake.admin_calls(),
        vec!["/usr/bin/xprotect update".to_string()]
    );
    fake.answer_full(
        "admin:/usr/bin/xprotect update",
        CmdOut {
            status: 1,
            stdout: String::new(),
            stderr: "no network".into(),
        },
    );
    assert_eq!(
        xprotect::update(&m.env),
        Err("macOS could not update the rules: no network".into())
    );
    fake.with(|s| s.admin_cancelled = true);
    assert_eq!(
        xprotect::update(&m.env),
        Err("Cancelled: nothing was changed.".into())
    );
}

#[test]
fn the_key_is_checked_kept_private_and_forgotten() {
    let m = FakeMac::new();
    assert!(known_malware::key(&m.env).is_none());
    assert!(known_malware::status(&m.env, 0).is_none());
    assert!(!known_malware::due(&m.env, 0));
    assert_eq!(
        known_malware::update(&m.env, 0),
        Err("No abuse.ch key is saved".into())
    );
    assert!(known_malware::set_key(&m.env, "short").is_err());
    assert!(known_malware::set_key(&m.env, "has spaces in it, 0123456789").is_err());
    known_malware::set_key(&m.env, &format!("  {KEY}\n")).expect("saved");
    assert_eq!(known_malware::key(&m.env).as_deref(), Some(KEY));
    let file = m.env.data_dir().join(known_malware::KEY_FILE);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&file).expect("file").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(known_malware::due(&m.env, 0));
    assert_eq!(
        known_malware::status(&m.env, 0).as_deref(),
        Some("0 known Mac malware samples from abuse.ch, not downloaded yet.")
    );
    known_malware::forget_key(&m.env).expect("forgotten");
    assert!(known_malware::key(&m.env).is_none());
    assert_eq!(std::fs::read(&file).expect("emptied, never deleted"), b"");
    known_malware::forget_key(&m.env).expect("twice is fine");
}

#[test]
fn the_daily_list_keeps_mac_samples_and_hides_the_key() {
    let m = FakeMac::new();
    let fake = m.env.fake_sys().expect("fake");
    known_malware::set_key(&m.env, KEY).expect("saved");
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    fake.answer(
        &url(),
        &csv(&[(&a, "macho", "AMOS"), (&b, "exe", "AgentTesla")]),
    );
    let day = 1_800_000_000;
    assert_eq!(known_malware::update(&m.env, day), Ok(1));
    let l = known_malware::load(&m.env);
    assert_eq!(l.samples.len(), 1);
    assert_eq!(l.samples[&a].family, "AMOS");
    assert!(!known_malware::due(&m.env, day + 3600));
    assert!(known_malware::due(&m.env, day + 86_400));
    assert_eq!(
        known_malware::status(&m.env, day + 60).as_deref(),
        Some("1 known Mac malware samples from abuse.ch, updated today.")
    );
    // The same sample again is not new.
    assert_eq!(known_malware::update(&m.env, day + 86_400), Ok(0));

    // A refused key.
    fake.answer_full(
        &url(),
        CmdOut {
            status: 22,
            stdout: String::new(),
            stderr: format!("curl: (22) The requested URL returned error: 401 for {KEY}"),
        },
    );
    let e = known_malware::update(&m.env, day + 2 * 86_400).expect_err("refused");
    assert_eq!(e, "abuse.ch refused the key. Check it at auth.abuse.ch");
    // Another failure names abuse.ch, never the key.
    fake.answer_full(
        &url(),
        CmdOut {
            status: 6,
            stdout: String::new(),
            stderr: format!("curl: (6) Could not resolve host for {KEY}"),
        },
    );
    let e = known_malware::update(&m.env, day + 2 * 86_400 + 10).expect_err("offline");
    assert!(!e.contains(KEY), "{e}");
    let st = known_malware::status(&m.env, day + 2 * 86_400 + 20).expect("status");
    assert!(!st.contains(KEY), "{st}");
    assert!(st.contains("The last download failed"), "{st}");
    assert!(
        st.starts_with("1 known Mac malware samples from abuse.ch, updated yesterday."),
        "{st}"
    );
    // An hour later it tries again; the samples stayed.
    assert!(!known_malware::due(&m.env, day + 2 * 86_400 + 100));
    assert!(known_malware::due(&m.env, day + 2 * 86_400 + 3700));
    assert_eq!(known_malware::load(&m.env).samples.len(), 1);
}

fn put(m: &FakeMac, path: PathBuf, bytes: &[u8]) -> PathBuf {
    m.write(&path, bytes);
    path
}

fn scan(m: &FakeMac) -> Plan {
    features::scan_metered(
        &m.env,
        &Settings::default(),
        FeatureId::Malware,
        ThreatDepth::Deep,
        &Progress::new(),
    )
    .expect("scan")
}

#[test]
fn deep_scans_find_exact_copies_of_listed_samples() {
    let m = mac_with_rules(3);
    let fake = m.env.fake_sys().expect("fake");
    let prog = put(
        &m,
        m.env.home_path("Downloads/Player"),
        b"\xcf\xfa\xed\xfe listed program",
    );
    let dmg = put(
        &m,
        m.env.home_path("Downloads/Setup.dmg"),
        b"listed disk image",
    );
    let other = put(
        &m,
        m.env.home_path("Downloads/Other.dmg"),
        b"another disk image",
    );
    // Without a list nothing matches (the rules match nothing), and disk images are not read.
    let p = scan(&m);
    assert!(
        p.items.iter().all(|i| i.category != "content"),
        "{:#?}",
        p.items
    );

    known_malware::set_key(&m.env, KEY).expect("saved");
    fake.answer(
        &url(),
        &csv(&[(&sha(&prog), "macho", "AMOS"), (&sha(&dmg), "dmg", "n/a")]),
    );
    known_malware::update(&m.env, 1_800_000_000).expect("list");
    let p = scan(&m);
    let find = |path: &Path| p.items.iter().find(|i| i.path == path);
    let a = find(&prog).expect("program");
    assert!(a.selected && a.locked.is_none(), "{a:#?}");
    assert!(
        a.reason
            .contains("a known malware sample (AMOS), listed by abuse.ch MalwareBazaar"),
        "{}",
        a.reason
    );
    let d = find(&dmg).expect("disk image");
    assert!(d.selected, "{d:#?}");
    assert!(
        d.reason
            .contains("a known malware sample, listed by abuse.ch MalwareBazaar"),
        "{}",
        d.reason
    );
    assert!(find(&other).is_none());
    assert!(
        p.notes
            .iter()
            .any(|n| n.contains("compared with 2 known Mac malware samples from abuse.ch")),
        "{:?}",
        p.notes
    );

    // A copy changed after the scan is no longer a listed sample: refused when it runs.
    std::fs::write(&dmg, b"changed since").expect("write");
    let rep = features::run_plan(
        &m.env,
        &Settings::default(),
        &p,
        &p.digest(),
        &mut |_, _| {},
    )
    .expect("run");
    assert_eq!(rep.moved.len(), 1, "{:?}", rep.moved);
    assert_eq!(rep.skipped.len(), 1);
    let why = format!("{:?}", rep.skipped[0].1);
    assert!(why.contains("no longer matches"), "{why}");
    assert!(dmg.exists() && !prog.exists());
    let (n, errs) = executor::undo_plan(&m.env, &p.id);
    assert!(errs.is_empty() && n == 1, "{errs:?}");
    assert!(prog.exists());
}
