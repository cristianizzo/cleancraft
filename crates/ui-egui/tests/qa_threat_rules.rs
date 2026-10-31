//! What Threat Check recognizes threats with, by clicks: old Apple rules shown in words with
//! an update button (through macOS, with the administrator password), then an abuse.ch key
//! pasted in Settings, the sample list downloaded at once, its status on Threat Check, and the
//! key forgotten.

mod common;

use cleancraft_core::known_malware;
use cleancraft_core::testkit_perf as tp;
use cleancraft_core::xprotect;
use common::*;
use egui_kittest::kittest::Queryable;

const KEY: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn rules_status_update_and_abuse_ch_key_by_clicks() {
    let m = tp::startup_mac();
    let fake = m.env.fake_sys().expect("fake");
    let dir = m.env.sys_path(xprotect::DIRS[0]);
    m.write(
        &dir.join(xprotect::FILE_RULES),
        br#"rule XProtect_NOTHING { strings: $a = "never-here" condition: $a }"#,
    );
    m.write(
        &dir.join("../Info.plist"),
        br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>CFBundleShortVersionString</key><string>5368</string></dict></plist>"#,
    );
    cleancraft_core::testkit::age(&dir.join(xprotect::FILE_RULES), 60);
    m.write(&m.env.sys_path(xprotect::TOOL), b"#!/bin/sh\n");
    fake.answer("admin:/usr/bin/xprotect update", "");
    fake.answer(
        &format!("/usr/bin/curl -fsS --max-time 60 https://mb-api.abuse.ch/v2/files/exports/{KEY}/recent.csv"),
        "# \"first_seen_utc\",\"sha256_hash\",\"md5_hash\",\"sha1_hash\",\"reporter\",\"file_name\",\"file_type_guess\",\"mime_type\",\"signature\"\n\
         \"2026-10-30 10:00:00\", \"aa00000000000000000000000000000000000000000000000000000000000001\", \"x\", \"x\", \"r\", \"f\", \"macho\", \"m\", \"AMOS\"\n",
    );

    let mut h = harness(&m);
    click(&mut h, "Protect");
    click(&mut h, "Threat Check");
    assert!(has_text(
        &h,
        "Apple's malware rules: version 5368, 60 days old."
    ));
    assert!(has_text(&h, "Add a free abuse.ch key in Settings"));
    click(&mut h, "Update Apple's Rules...");
    assert_eq!(
        fake.admin_calls(),
        vec!["/usr/bin/xprotect update".to_string()]
    );
    assert!(has_text(&h, "macOS checked for new malware rules."));

    click(&mut h, "Settings");
    h.get_by_label("abuse.ch key").scroll_to_me();
    steps(&mut h);
    type_into(&mut h, "abuse.ch key", KEY);
    click(&mut h, "Save Key");
    assert_eq!(known_malware::key(&m.env).as_deref(), Some(KEY));
    assert_eq!(known_malware::load(&m.env).samples.len(), 1);
    assert!(has_text(&h, "1 new known malware sample from abuse.ch."));
    assert!(has_text(&h, "Key ending in cdef"));
    assert!(!has_text(&h, KEY), "the whole key is never shown");

    click(&mut h, "Protect");
    click(&mut h, "Threat Check");
    assert!(has_text(
        &h,
        "1 known Mac malware samples from abuse.ch, updated today."
    ));

    click(&mut h, "Settings");
    h.get_by_label("Forget Key").scroll_to_me();
    steps(&mut h);
    click(&mut h, "Forget Key");
    assert!(known_malware::key(&m.env).is_none());
    assert!(!has(&h, "Forget Key"));
}
