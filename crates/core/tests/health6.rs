//! Mac Health facts, thresholds and recommendations on fake Macs; Disk Map growth.

use cleancraft_core::FeatureId;
use cleancraft_core::Settings;
use cleancraft_core::health::{self, Card, Facts, Level, State};
use cleancraft_core::monitor::{Battery, BatteryReading, Pressure};
use cleancraft_core::route::AppRoute;
use cleancraft_core::testkit_health::{self as th, GB};

const NOW: i64 = 1_790_000_000;

fn facts(mac: &cleancraft_core::testkit::FakeMac) -> Facts {
    health::gather(&mac.env, &Settings::default(), NOW, &|_| {})
}

fn card<'a>(cards: &'a [Card], rule: &str) -> Option<&'a Card> {
    cards.iter().find(|c| c.rule == rule)
}

#[test]
fn calm_mac_is_good_with_no_cards() {
    let mac = th::calm_mac(NOW);
    let f = facts(&mac);
    assert_eq!(f.disk_free, Some(300 * GB / 1024 * 1024));
    assert_eq!(f.unused_apps, Some(0));
    assert_eq!(f.apps_dated, 2);
    assert_eq!(f.macos.as_deref(), Some("15.1"));
    assert_eq!(f.startup.as_ref().map(|s| s.enabled), Some(1));
    assert!(f.startup.as_ref().unwrap().weak.is_empty());
    let cards = health::recommendations(&f);
    assert!(cards.is_empty(), "{cards:#?}");
    let (state, why) = health::summary(&cards);
    assert_eq!(state, State::Good);
    assert!(why.is_empty());
}

#[test]
fn low_space_levels_follow_the_thresholds() {
    let total = 500 * GB;
    // 15% line and 20 GB line.
    assert_eq!(health::space_level(80 * GB, Some(total)), None);
    assert_eq!(
        health::space_level(74 * GB, Some(total)),
        Some(Level::Attention)
    );
    assert_eq!(
        health::space_level(49 * GB, Some(total)),
        Some(Level::Urgent)
    );
    assert_eq!(health::space_level(19 * GB, None), Some(Level::Attention));
    assert_eq!(health::space_level(9 * GB, None), Some(Level::Urgent));
    assert_eq!(health::space_level(21 * GB, None), None);

    let mac = th::calm_mac(NOW);
    th::set_disk(&mac.env, total, 40 * GB);
    let cards = health::recommendations(&facts(&mac));
    let c = card(&cards, "free_low").expect("low space card");
    assert_eq!(c.level, Level::Urgent);
    // fix7 U7-1: Disk Map opens measuring.
    assert_eq!(c.route, AppRoute::Scan(FeatureId::SpaceLens));
    assert_eq!(cards[0].rule, "free_low", "urgent first");
    let (state, why) = health::summary(&cards);
    assert_eq!(state, State::NeedsAttention);
    assert_eq!(why, vec![c.headline.clone()]);

    th::set_disk(&mac.env, total, 70 * GB);
    let cards = health::recommendations(&facts(&mac));
    assert_eq!(
        card(&cards, "free_getting_low").unwrap().level,
        Level::Attention
    );
}

#[test]
fn fast_drop_needs_ten_gb_over_two_days() {
    let mac = th::calm_mac(NOW);
    th::history(&mac.env, NOW, &[(6.0, 320 * GB), (0.0, 305 * GB)], 500 * GB);
    let cards = health::recommendations(&facts(&mac));
    let c = card(&cards, "free_drop").expect("drop card");
    assert!(
        c.headline.contains("15.0 GB") && c.headline.contains("6 days"),
        "{}",
        c.headline
    );
    assert_eq!(c.route, AppRoute::Scan(FeatureId::SpaceLens));
    // 9 GB is under the line; samples over one day are too short for a trend.
    th::history(&mac.env, NOW, &[(6.0, 314 * GB), (0.0, 305 * GB)], 500 * GB);
    assert!(card(&health::recommendations(&facts(&mac)), "free_drop").is_none());
    th::history(&mac.env, NOW, &[(1.0, 330 * GB), (0.0, 305 * GB)], 500 * GB);
    assert!(card(&health::recommendations(&facts(&mac)), "free_drop").is_none());
}

#[test]
fn threats_found_are_urgent_and_open_threat_check() {
    let mac = th::calm_mac(NOW);
    th::note(&mac.env, FeatureId::Malware, 1, 0, 2, NOW);
    let cards = health::recommendations(&facts(&mac));
    let c = card(&cards, "threats").unwrap();
    assert_eq!(c.level, Level::Urgent);
    assert_eq!(c.headline, "Threat Check found 2 items yesterday");
    // fix7 U7-3: the scan starts (no path was noted here, so nothing to preselect).
    assert_eq!(c.route, AppRoute::Scan(FeatureId::Malware));
    assert!(c.landing.is_some());
    assert!(card(&cards, "threat_scan").is_none());
}

#[test]
fn stale_or_missing_scans_suggest_a_scan_that_only_looks() {
    let mac = th::calm_mac(NOW);
    th::note(&mac.env, FeatureId::Malware, 31, 0, 0, NOW);
    th::note(&mac.env, FeatureId::SystemJunk, 30, 0, 0, NOW);
    th::note(&mac.env, FeatureId::Updater, 14, 0, 0, NOW);
    let cards = health::recommendations(&facts(&mac));
    assert_eq!(
        card(&cards, "threat_scan").unwrap().route,
        AppRoute::Scan(FeatureId::Malware)
    );
    assert_eq!(
        card(&cards, "junk").unwrap().route,
        AppRoute::Scan(FeatureId::SystemJunk)
    );
    let u = card(&cards, "updates").unwrap();
    assert_eq!(u.route, AppRoute::Feature(FeatureId::Updater));
    assert!(cards.iter().all(|c| c.level == Level::Tip));
    assert_eq!(
        health::summary(&cards).0,
        State::Good,
        "tips never change the state"
    );
    // Day 29 and 13: still fresh.
    th::note(&mac.env, FeatureId::Malware, 29, 0, 0, NOW + 1);
    th::note(&mac.env, FeatureId::SystemJunk, 29, 0, 0, NOW + 1);
    th::note(&mac.env, FeatureId::Updater, 13, 0, 0, NOW + 1);
    assert!(health::recommendations(&facts(&mac)).is_empty());
}

#[test]
fn junk_found_at_the_last_scan() {
    let mac = th::calm_mac(NOW);
    th::note(&mac.env, FeatureId::SystemJunk, 3, 1_500_000_000, 812, NOW);
    let cards = health::recommendations(&facts(&mac));
    let c = card(&cards, "junk").unwrap();
    assert_eq!(c.headline, "1.50 GB of junk found 3 days ago");
    assert_eq!(c.route, AppRoute::Scan(FeatureId::SystemJunk));
    // 999 MB: under the 1 GB line.
    th::note(&mac.env, FeatureId::SystemJunk, 2, 999_000_000, 812, NOW);
    assert!(card(&health::recommendations(&facts(&mac)), "junk").is_none());
}

#[test]
fn unused_apps_count_only_known_dates_over_180_days() {
    let mac = th::calm_mac(NOW);
    th::app_opened(&mac, "Atlas", 181, NOW);
    th::app_opened(&mac, "Beacon", 400, NOW);
    th::app_opened(&mac, "Comet", 179, NOW);
    // No Spotlight date: never counted.
    mac.app("Dusty", "com.example.dusty", "1.0");
    let f = facts(&mac);
    assert_eq!(f.unused_apps, Some(2));
    assert_eq!(f.apps_dated, 5);
    let cards = health::recommendations(&f);
    let c = card(&cards, "unused_apps").unwrap();
    assert_eq!(c.headline, "2 apps not opened for 6 months");
    assert_eq!(c.action, "Review in App Remover");
    assert_eq!(c.route, AppRoute::UnusedApps);
}

#[test]
fn unsigned_startup_items_need_attention() {
    let mac = th::calm_mac(NOW);
    th::launch_agent(&mac, "com.shady.agent", false);
    let f = facts(&mac);
    assert_eq!(f.startup.as_ref().unwrap().weak, vec!["com.shady.agent"]);
    let cards = health::recommendations(&f);
    let c = card(&cards, "startup_unsigned").unwrap();
    assert_eq!(c.level, Level::Attention);
    assert_eq!(
        c.headline,
        "1 startup item is not signed by a known developer"
    );
    assert_eq!(c.route, AppRoute::Feature(FeatureId::LoginItems));
}

#[test]
fn ignored_startup_items_stop_the_warning() {
    let mac = th::calm_mac(NOW);
    th::launch_agent(&mac, "sh.brew.postgresql@16", false);
    assert_eq!(
        facts(&mac).startup.as_ref().unwrap().weak,
        vec!["sh.brew.postgresql@16"]
    );
    let mut settings = Settings::default();
    settings.ignore.push(
        mac.env
            .home_path("Library/LaunchAgents/sh.brew.postgresql@16.plist"),
    );
    let f = health::gather(&mac.env, &settings, NOW, &|_| {});
    assert!(f.startup.as_ref().unwrap().weak.is_empty());
    assert!(card(&health::recommendations(&f), "startup_unsigned").is_none());
}

#[test]
fn many_startup_items_is_a_tip_at_fifteen() {
    let mac = th::calm_mac(NOW);
    for i in 0..13 {
        th::launch_agent(&mac, &format!("com.example.a{i}"), true);
    }
    assert!(card(&health::recommendations(&facts(&mac)), "startup_many").is_none());
    th::launch_agent(&mac, "com.example.last", true);
    let cards = health::recommendations(&facts(&mac));
    assert_eq!(
        card(&cards, "startup_many").unwrap().headline,
        "15 items start with your Mac"
    );
}

#[test]
fn memory_pressure_cards() {
    let mac = th::calm_mac(NOW);
    th::set_pressure(&mac.env, 2);
    let cards = health::recommendations(&facts(&mac));
    let c = card(&cards, "memory").unwrap();
    assert_eq!(c.level, Level::Attention);
    assert_eq!(c.route, AppRoute::Feature(FeatureId::Monitor));
    th::set_pressure(&mac.env, 4);
    assert_eq!(
        card(&health::recommendations(&facts(&mac)), "memory")
            .unwrap()
            .level,
        Level::Urgent
    );
}

#[test]
fn battery_wear_and_cycles() {
    let mac = th::calm_mac(NOW);
    th::set_battery(&mac.env, 3400, 4382, 400);
    let f = facts(&mac);
    let cards = health::recommendations(&f);
    let c = card(&cards, "battery").unwrap();
    assert_eq!(
        c.headline,
        "The battery holds 78% of its design capacity and has 400 charge cycles"
    );
    th::set_battery(&mac.env, 4300, 4382, 1000);
    assert!(card(&health::recommendations(&facts(&mac)), "battery").is_some());
    th::set_battery(&mac.env, 4300, 4382, 999);
    assert!(card(&health::recommendations(&facts(&mac)), "battery").is_none());
    th::set_desktop(&mac.env);
    let f = facts(&mac);
    assert_eq!(f.battery, Some(BatteryReading::None));
    assert!(card(&health::recommendations(&f), "battery").is_none());
}

#[test]
fn big_trash_is_a_tip() {
    // 5 GB cannot be written in a test folder: the rule is checked on the facts.
    let mut f = Facts {
        now: NOW,
        trash: Some(5 * GB),
        ..Default::default()
    };
    let c = card(&health::recommendations(&f), "trash")
        .cloned()
        .unwrap();
    assert_eq!(c.route, AppRoute::Scan(FeatureId::TrashBins));
    assert_eq!(c.level, Level::Tip);
    f.trash = Some(5 * GB - 1);
    assert!(card(&health::recommendations(&f), "trash").is_none());
    // And the page reads the real Trash size.
    let mac = th::calm_mac(NOW);
    mac.home_file(".Trash/old.zip", 300_000);
    assert!(facts(&mac).trash.unwrap() >= 300_000);
}

#[test]
fn unreadable_facts_are_unknown_never_guessed() {
    let mac = cleancraft_core::testkit::FakeMac::new();
    let f = facts(&mac);
    assert_eq!(f.disk_free, None);
    assert_eq!(f.memory, None);
    assert_eq!(f.macos, None);
    assert!(!f.problems.is_empty());
    let cards = health::recommendations(&f);
    assert!(card(&cards, "free_low").is_none());
    assert!(card(&cards, "memory").is_none());
}

#[test]
fn every_card_has_a_documented_rule_and_one_action() {
    let f = Facts {
        now: NOW,
        disk_total: Some(500 * GB),
        disk_free: Some(5 * GB),
        free_change: Some((-(20 * GB as i64), 7 * 86_400)),
        trash: Some(6 * GB),
        junk: None,
        threats: Some(health::ScanNote {
            at: NOW,
            bytes: 0,
            items: 1,
            plan_id: String::new(),
            paths: Vec::new(),
        }),
        updates: None,
        unused_apps: Some(3),
        apps_dated: 3,
        startup: Some(health::StartupFacts {
            enabled: 20,
            weak: vec!["a".into(), "b".into(), "c".into(), "d".into()],
        }),
        battery: Some(BatteryReading::Present(Battery {
            percent: 50,
            state: "discharging".into(),
            on_ac: false,
            remaining_minutes: None,
            cycles: Some(1200),
            health: Some(70),
        })),
        memory: Some(health::Memory {
            pressure: Some(Pressure::Critical),
            used_fraction: 0.95,
        }),
        macos: Some("15.1".into()),
        problems: vec![],
    };
    let cards = health::recommendations(&f);
    // Every rule but "threat_scan" (a found threat replaces it).
    assert_eq!(cards.len(), 11, "{cards:#?}");
    for c in &cards {
        let rule = health::RULES.iter().find(|r| r.id == c.rule);
        assert!(rule.is_some(), "rule {} documented", c.rule);
        assert!(!c.action.is_empty());
        for s in [&c.headline, &c.detail, &c.action] {
            assert!(
                !s.contains('\u{2014}') && !s.to_lowercase().contains(" nor "),
                "{s}"
            );
        }
    }
    // Levels never go down the list.
    assert!(cards.windows(2).all(|w| w[0].level >= w[1].level));
}

#[test]
fn disk_map_growth_names_the_inner_folder() {
    use cleancraft_core::health::{MapSummary, compare};
    use std::path::PathBuf;
    let p = |s: &str| PathBuf::from(format!("/Users/tester{s}"));
    let old = MapSummary {
        at: 1,
        root: p(""),
        entries: vec![
            (p(""), 100 * GB),
            (p("/Library"), 40 * GB),
            (p("/Library/Caches"), 5 * GB),
            (p("/Movies"), 10 * GB),
            (p("/Music"), 10 * GB),
        ],
    };
    let new = MapSummary {
        at: 2,
        root: p(""),
        entries: vec![
            (p(""), 120 * GB),
            (p("/Library"), 50 * GB),
            (p("/Library/Caches"), 14 * GB),
            (p("/Movies"), 14 * GB),
            (p("/Music"), 10 * GB + 1),
            (p("/Downloads"), 3 * GB),
        ],
    };
    let g = compare(&old, &new);
    let names: Vec<_> = g.grew.iter().map(|x| x.path.clone()).collect();
    // Caches holds 9 of Library's 10 GB: Caches is named, Library is not.
    assert_eq!(
        names,
        vec![p("/Library/Caches"), p("/Movies"), p("/Downloads")]
    );
    assert_eq!(g.grew[2].before, 0);
}

#[test]
fn disk_map_summaries_keep_two_per_folder() {
    use cleancraft_core::space;
    let mac = cleancraft_core::testkit::FakeMac::sample();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let tree = space::build(mac.env.home(), &stop, &|_, _| {});
    assert!(health::note_disk_map(&mac.env, &tree, 10).unwrap());
    assert!(
        !health::note_disk_map(&mac.env, &tree, 11).unwrap(),
        "same sizes: no new entry"
    );
    assert!(health::what_grew(&mac.env).is_none());
    mac.home_file("Movies/big.mov", 1_000_000);
    let tree2 = space::build(mac.env.home(), &stop, &|_, _| {});
    assert!(health::note_disk_map(&mac.env, &tree2, 20).unwrap());
    mac.home_file("Movies/big2.mov", 1_000_000);
    let tree3 = space::build(mac.env.home(), &stop, &|_, _| {});
    assert!(health::note_disk_map(&mac.env, &tree3, 30).unwrap());
    // fix7 U7-1: the two newest, plus the first of each day (kept for the 7/30/90-day
    // comparisons).
    assert_eq!(health::map_summaries(&mac.env).len(), 3);
    let g = health::what_grew(&mac.env).unwrap();
    assert_eq!((g.from, g.to), (20, 30));
    // 1 MB is under the 500 MB line: nothing is named.
    assert!(g.grew.is_empty());
    // A stopped scan is never kept.
    let mut stopped = tree3.clone();
    stopped.stopped = true;
    assert!(!health::note_disk_map(&mac.env, &stopped, 40).unwrap());
}

#[test]
fn scan_notes_follow_plans_and_ignore_older_ones() {
    let mac = cleancraft_core::testkit::FakeMac::new();
    let mut plan = cleancraft_core::Plan::new(FeatureId::SystemJunk);
    plan.created = NOW;
    health::note_plan(&mac.env, &plan).unwrap();
    health::note_count(&mac.env, FeatureId::SystemJunk, 1, NOW - 5 * 86_400).unwrap();
    let n = &health::scan_notes(&mac.env)[&FeatureId::SystemJunk];
    assert_eq!(n.at, NOW, "an older note does not replace a newer one");
    // A damaged notes file reads as no notes.
    std::fs::write(mac.env.data_dir().join("scan-notes.json"), b"{oops").unwrap();
    assert!(health::scan_notes(&mac.env).is_empty());
}
