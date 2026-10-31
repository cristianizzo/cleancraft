//! threat3 by clicks: the watcher's notification for a download that looks like malware opens
//! Threat Check with it selected; Review and Clean, Move to Quarantine, and Undo puts it back.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cleancraft_core::notify;
use cleancraft_core::testkit::FakeMac;
use cleancraft_core::watch::{WatchEvent, WatchSettings, Watcher};
use cleancraft_core::{FeatureId, xprotect};
use cleancraft_ui_egui::Page;
use common::*;

const RULES: &str = r#"rule XProtect_MACOS_TESTBAD {
    meta: description = "MACOS.TESTBAD"
    strings: $a = "evil-test-payload"
    condition: $a
}"#;

#[test]
fn a_flagged_download_is_quarantined_from_its_notification() {
    let m = FakeMac::new();
    let f = m.env.fake_sys().expect("fake");
    f.answer("id -u", "501\n");
    f.answer("launchctl", "");
    f.answer("open", "");
    f.answer("sh", "");
    m.write(
        &m.env.sys_path(xprotect::DIRS[0]).join(xprotect::FILE_RULES),
        RULES.as_bytes(),
    );
    let downloads = m.env.home_path("Downloads");
    std::fs::create_dir_all(&downloads).unwrap();

    // The menu bar agent's watcher sees the download and sends its notification.
    let evs: Arc<Mutex<Vec<WatchEvent>>> = Arc::default();
    let e2 = evs.clone();
    let s = WatchSettings {
        trash_offer: false,
        debounce: Duration::from_millis(300),
        coalesce: Duration::from_millis(300),
        tick: Duration::from_millis(40),
        ..Default::default()
    };
    let w = Watcher::start(m.env.clone(), s, move |ev| e2.lock().unwrap().push(ev)).unwrap();
    let player = downloads.join("Player");
    m.write(&player, b"\xcf\xfa\xed\xfe evil-test-payload");
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(6) && evs.lock().unwrap().is_empty() {
        std::thread::sleep(Duration::from_millis(30));
    }
    w.stop();
    let ev = evs.lock().unwrap().first().cloned().expect("notified");
    assert!(player.exists(), "the watcher never moves anything");

    // Clicking it opens Threat Check with the download selected.
    let mut h = harness(&m);
    notify::request_open(&m.env, &ev.open.to_arg()).expect("request");
    for _ in 0..20 {
        if h.state().page == Page::Feature(FeatureId::Malware) {
            break;
        }
        h.step();
    }
    steps(&mut h);
    assert!(has_text(&h, "Player is selected for review"));
    assert!(has_text(
        &h,
        "Matches Testbad (malware), Apple's rule MACOS.TESTBAD"
    ));

    click(&mut h, "Review and Clean");
    click(&mut h, "Move to Quarantine");
    assert!(!player.exists());
    assert!(has_text(&h, "1 item moved."));
    click(&mut h, "Undo");
    assert!(player.exists(), "put back");
}
