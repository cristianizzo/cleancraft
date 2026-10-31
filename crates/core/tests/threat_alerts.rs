//! threat3: a download that looks like malware is told within seconds and lands in Threat
//! Check, selected, where the user moves it to quarantine. The watcher itself never moves it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cleancraft_core::features::{self, malware::ThreatDepth};
use cleancraft_core::progress::Progress;
use cleancraft_core::testkit::FakeMac;
use cleancraft_core::watch::{self, AppRoute, WatchEvent, WatchKind, WatchSettings, Watcher};
use cleancraft_core::{FeatureId, Settings, executor, xprotect};

const RULES: &str = r#"rule XProtect_MACOS_TESTBAD {
    meta: description = "MACOS.TESTBAD"
    strings: $a = "evil-test-payload"
    condition: $a
}"#;

const BAD: &[u8] = b"\xcf\xfa\xed\xfe evil-test-payload";

type Events = Arc<Mutex<Vec<WatchEvent>>>;

fn wait_events(evs: &Events, n: usize) -> Vec<WatchEvent> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(6) && evs.lock().unwrap().len() < n {
        std::thread::sleep(Duration::from_millis(30));
    }
    std::thread::sleep(Duration::from_millis(900));
    std::mem::take(&mut *evs.lock().unwrap())
}

#[test]
fn a_download_that_looks_like_malware_is_told_and_listed_for_quarantine() {
    let m = FakeMac::new();
    m.write(
        &m.env.sys_path(xprotect::DIRS[0]).join(xprotect::FILE_RULES),
        RULES.as_bytes(),
    );
    let downloads = m.env.home_path("Downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let evs: Events = Arc::default();
    let e2 = evs.clone();
    let s = WatchSettings {
        trash_offer: false,
        debounce: Duration::from_millis(300),
        coalesce: Duration::from_millis(300),
        tick: Duration::from_millis(40),
        ..Default::default()
    };
    let h = Watcher::start(m.env.clone(), s, move |ev| e2.lock().unwrap().push(ev)).unwrap();

    // A clean script and a file still downloading say nothing; the matching program does.
    m.write(&downloads.join("clean.sh"), b"#!/bin/sh\necho hello\n");
    m.write(&downloads.join("Setup.crdownload"), BAD);
    let player = downloads.join("Player");
    m.write(&player, BAD);
    let got = wait_events(&evs, 1);
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!(got[0].kind, WatchKind::Download);
    assert_eq!(got[0].title, "Player looks like malware");
    assert_eq!(
        got[0].body,
        "It matches Testbad (malware), Apple's rule MACOS.TESTBAD. Review it in Threat Check."
    );
    assert_eq!(got[0].open, AppRoute::ThreatReview(player.clone()));

    // An unpacked folder: the app inside it is what Threat Check lists.
    let app = downloads.join("Tool/Tool.app");
    m.write(&app.join("Contents/MacOS/Tool"), BAD);
    let got = wait_events(&evs, 1);
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!(got[0].title, "Tool.app looks like malware");
    assert!(
        got[0]
            .body
            .starts_with("Contents/MacOS/Tool inside it matches Testbad"),
        "{}",
        got[0].body
    );
    assert_eq!(got[0].open, AppRoute::ThreatReview(app.clone()));
    h.stop();
    assert_eq!(watch::flagged(&m.env), vec![player.clone(), app.clone()]);
    assert!(player.exists() && executor::history(&m.env).is_empty());

    // A Quick check lists both: the program selected, the app only for review.
    let plan = features::scan_metered(
        &m.env,
        &Settings::default(),
        FeatureId::Malware,
        ThreatDepth::Quick,
        &Progress::new(),
    )
    .expect("scan");
    let find = |p: &std::path::Path| plan.items.iter().find(|i| i.path == p);
    let p = find(&player).expect("program listed");
    assert!(p.selected && p.locked.is_none(), "{p:#?}");
    assert_eq!(p.category, "content");
    let a = find(&app).expect("app listed");
    assert!(!a.selected, "{a:#?}");

    // Quarantined through the plan, then undone.
    let rep = features::run_plan(
        &m.env,
        &Settings::default(),
        &plan,
        &plan.digest(),
        &mut |_, _| {},
    )
    .expect("run");
    assert_eq!(rep.moved.len(), 1, "{:?}", rep.skipped);
    assert!(!player.exists());
    assert_eq!(watch::flagged(&m.env), vec![app.clone()]);
    let (n, errs) = executor::undo_plan(&m.env, &plan.id);
    assert!(errs.is_empty() && n == 1, "{errs:?}");
    assert!(player.exists());
}
