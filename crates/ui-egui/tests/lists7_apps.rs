//! lists7: the shared list toolbar on App Remover and App Updates, by clicks and keys on a
//! FakeMac. Search narrows the list and its counts, every filter and sort works and is
//! remembered after a relaunch, apps of macOS are hidden until asked for (and never
//! selectable), ticked apps the filters hide are still counted and still go only through a
//! confirmation that lists them, Clear Filters restores the whole list.

mod common;

use std::path::PathBuf;

use cleancraft_core::testkit::FakeMac;
use cleancraft_core::testkit_apps::{AppSpec, FakeMacApps};
use common::*;
use egui_kittest::kittest::{NodeT, Queryable};

struct Mac {
    mac: FakeMac,
    editor: PathBuf,
    viewer: PathBuf,
    jotter: PathBuf,
    safari: PathBuf,
}

/// Editor (other source, 3 MB, opened 2 days ago, has Library files, running), Viewer (App
/// Store, 1.2 GB, opened 300 days ago, by Maker), Jotter (Setapp, 1 MB, never opened),
/// Safari (part of macOS).
fn mac() -> Mac {
    let mac = FakeMac::new();
    let editor = mac.app_spec(&AppSpec {
        size: 3_000_000,
        ..AppSpec::new("Editor", "com.example.Editor", "2.0")
    });
    let viewer = mac.app_spec(&AppSpec {
        mas: true,
        size: 1_200_000_000,
        ..AppSpec::new("Viewer", "com.maker.Viewer", "1.0")
    });
    let jotter = mac.app_spec(&AppSpec {
        dir: "/Applications/Setapp",
        ..AppSpec::new("Jotter", "org.jot.Jotter", "3.1")
    });
    let safari = mac.app("Safari", "com.apple.Safari", "18.0");
    mac.last_opened(&editor, 2);
    mac.last_opened(&viewer, 300);
    mac.spotlight_finds_nothing();
    mac.home_file("Library/Caches/com.example.Editor/blob", 900_000);
    mac.run_app(&editor);
    Mac {
        mac,
        editor,
        viewer,
        jotter,
        safari,
    }
}

fn go(h: &mut H) {
    click(h, "Apps");
    click(h, "App Remover");
    settle(h);
}

fn settle(h: &mut H) {
    for _ in 0..3 {
        steps(h);
    }
}

/// The app rows shown, top to bottom.
fn rows(h: &H) -> Vec<String> {
    let mut v: Vec<(f32, String)> = ["Editor", "Viewer", "Jotter", "Safari"]
        .iter()
        .filter_map(|n| {
            h.query_all_by_label(n)
                .find(|x| x.accesskit_node().role() == egui::accesskit::Role::Button)
                .map(|x| (x.rect().center().y, n.to_string()))
        })
        .collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    v.into_iter().map(|(_, n)| n).collect()
}

fn filter(h: &mut H, entry: &str) {
    click(h, "Filter");
    click(h, entry);
    // Close the pop-up (it stays open while filters are switched).
    h.key_press(egui::Key::Escape);
    settle(h);
}

fn sort(h: &mut H, entry: &str) {
    click(h, "Sort");
    click(h, entry);
    settle(h);
}

fn relaunch(m: &Mac) -> H {
    let mut h = harness(&m.mac);
    go(&mut h);
    h
}

#[test]
fn macos_apps_are_hidden_until_asked_for_and_never_selectable() {
    let m = mac();
    let mut h = harness(&m.mac);
    go(&mut h);
    // Default: hidden, out of the totals, and said so.
    assert_eq!(rows(&h), ["Editor", "Jotter", "Viewer"]);
    assert!(
        has_text(&h, "in 3 apps (apps only), 1 from macOS hidden"),
        "headline"
    );
    assert!(has_text(&h, "All Apps  3  "), "group count without Safari");
    assert!(has(&h, "1 macOS app hidden"));
    // Shown on request: locked, with its tag.
    click(&mut h, "Show apps that are part of macOS");
    assert!(rows(&h).contains(&"Safari".to_string()));
    assert!(has(&h, "Part of macOS"));
    assert!(
        h.get_by_label("Select Safari")
            .accesskit_node()
            .is_disabled()
    );
    assert!(has_text(&h, "All Apps  4  "));
    assert!(has_text(&h, "in 4 apps"));
    // Never selectable: its padlock, and select all in a group leaves it out.
    h.get_by_label("Select Safari").click();
    settle(&mut h);
    assert!(h.state().apps.picked.is_empty());
    // (The padlock does nothing; a click that reaches the row opens the app's detail.)
    if has(&h, "Back to Apps") {
        click(&mut h, "Back to Apps");
    }
    click(&mut h, "Select all in Other Sources");
    assert!(!h.state().apps.picked.contains(&m.safari));
    assert_eq!(
        h.state().apps.picked.iter().collect::<Vec<_>>(),
        vec![&m.editor]
    );
    // Remembered after a relaunch.
    let h2 = relaunch(&m);
    assert!(rows(&h2).contains(&"Safari".to_string()));
    assert!(
        h2.state().settings.lists["apps"]
            .filters
            .contains(&"show_macos".to_string())
    );
    // And hidden again.
    let mut h3 = relaunch(&m);
    click(&mut h3, "Show apps that are part of macOS");
    assert!(!rows(&h3).contains(&"Safari".to_string()));
    assert!(!relaunch(&m).state().settings.lists.contains_key("apps"));
}

#[test]
fn search_narrows_the_list_and_the_counts_and_esc_clears_it() {
    let m = mac();
    let mut h = harness(&m.mac);
    go(&mut h);
    assert!(
        !has_text(&h, "Showing"),
        "no line while nothing narrows the list"
    );
    type_into(&mut h, "Search apps", "maker");
    assert_eq!(rows(&h), ["Viewer"], "developer and bundle id are searched");
    assert!(has(&h, "Showing 1 of 3 apps"));
    // Cmd+F focuses the field, Esc clears it.
    h.get_by_label("Viewer").focus();
    steps(&mut h);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::F);
    settle(&mut h);
    h.key_press(egui::Key::Escape);
    settle(&mut h);
    assert!(h.state().apps.search.is_empty(), "Esc clears the search");
    assert_eq!(rows(&h).len(), 3);
    assert!(!has_text(&h, "Showing"));
}

#[test]
fn every_filter_narrows_with_its_count_and_is_remembered() {
    let m = mac();
    let mut h = harness(&m.mac);
    go(&mut h);
    let cases: [(&str, &[&str]); 7] = [
        ("App Store (1)", &["Viewer"]),
        ("Setapp (1)", &["Jotter"]),
        ("Other sources (1)", &["Editor"]),
        ("Unused (1)", &["Viewer"]),
        ("Running (1)", &["Editor"]),
        ("Large (1 GB or more) (1)", &["Viewer"]),
        ("Has files in Library (1)", &["Editor"]),
    ];
    for (entry, want) in cases {
        filter(&mut h, entry);
        assert_eq!(rows(&h), want, "{entry}");
        assert!(has(&h, "Showing 1 of 3 apps"), "{entry}");
        assert!(
            h.get_by_label("Filter")
                .value()
                .is_some_and(|v| v != "None"),
            "Filter announces what is on"
        );
        click(&mut h, "Clear Filters");
        assert_eq!(rows(&h).len(), 3, "Clear Filters restores after {entry}");
    }
    // Two sources: either one. Source and Unused: both.
    click(&mut h, "Filter");
    click(&mut h, "App Store (1)");
    click(&mut h, "Setapp (1)");
    h.key_press(egui::Key::Escape);
    settle(&mut h);
    assert_eq!(rows(&h), ["Jotter", "Viewer"]);
    assert!(has(&h, "Remove filter App Store"), "a chip per filter");
    filter(&mut h, "Unused (1)");
    assert_eq!(rows(&h), ["Viewer"]);
    // A chip turns its filter off.
    click(&mut h, "Remove filter Unused");
    assert_eq!(rows(&h), ["Jotter", "Viewer"]);
    // Remembered after a relaunch, the search is not.
    type_into(&mut h, "Search apps", "j");
    let mut h2 = relaunch(&m);
    assert_eq!(rows(&h2), ["Jotter", "Viewer"]);
    assert!(h2.state().apps.search.is_empty());
    assert!(has(&h2, "Showing 2 of 3 apps"));
    click(&mut h2, "Clear Filters");
    assert_eq!(rows(&h2).len(), 3);
    assert!(!relaunch(&m).state().settings.lists.contains_key("apps"));
    // Selected: the ticked apps only.
    click(&mut h2, "Select Jotter");
    filter(&mut h2, "Selected (1)");
    assert_eq!(rows(&h2), ["Jotter"]);
}

#[test]
fn every_sort_and_direction_works_and_is_remembered() {
    let m = mac();
    // Install dates: Jotter newest, Viewer oldest.
    let now = std::time::SystemTime::now();
    let day = std::time::Duration::from_secs(86_400);
    for (p, days) in [(&m.viewer, 30), (&m.editor, 10), (&m.jotter, 1)] {
        std::fs::File::open(p)
            .and_then(|f| f.set_modified(now - day * days))
            .expect("set time");
    }
    let mut h = harness(&m.mac);
    go(&mut h);
    assert_eq!(
        h.get_by_label("Sort").value().as_deref(),
        Some("Sort by name")
    );
    assert_eq!(rows(&h), ["Editor", "Jotter", "Viewer"]);
    sort(&mut h, "Sort by size");
    sort(&mut h, "Smallest first");
    assert_eq!(rows(&h), ["Jotter", "Editor", "Viewer"]);
    assert_eq!(
        h.get_by_label("Sort").value().as_deref(),
        Some("Sort by size, smallest first")
    );
    sort(&mut h, "Sort by name");
    assert_eq!(rows(&h), ["Editor", "Jotter", "Viewer"]);
    sort(&mut h, "Z to A");
    assert_eq!(rows(&h), ["Viewer", "Jotter", "Editor"]);
    // Longest unused first; never opened (Jotter) last either way.
    sort(&mut h, "Sort by last opened");
    assert_eq!(rows(&h), ["Viewer", "Editor", "Jotter"]);
    sort(&mut h, "Recently opened first");
    assert_eq!(rows(&h), ["Editor", "Viewer", "Jotter"]);
    sort(&mut h, "Sort by developer");
    let vendors: Vec<String> = rows(&h)
        .iter()
        .map(|n| {
            h.state()
                .apps
                .list
                .iter()
                .flatten()
                .find(|a| &a.name == n)
                .map(|a| a.vendor.to_lowercase())
                .unwrap_or_default()
        })
        .collect();
    let mut sorted = vendors.clone();
    sorted.sort();
    assert_eq!(vendors, sorted, "developers A to Z");
    // Install date: when the bundle came (the file system's birth time where it keeps one,
    // else its change time).
    sort(&mut h, "Sort by install date");
    let dates: Vec<i64> = rows(&h)
        .iter()
        .map(|n| {
            let st = &h.state().apps;
            let p = &st
                .list
                .iter()
                .flatten()
                .find(|a| &a.name == n)
                .unwrap()
                .path;
            st.facts.installed[p]
        })
        .collect();
    assert!(
        dates.windows(2).all(|w| w[0] >= w[1]),
        "newest first: {dates:?}"
    );
    sort(&mut h, "Oldest first");
    // Remembered after a relaunch.
    let h2 = relaunch(&m);
    assert_eq!(
        h2.get_by_label("Sort").value().as_deref(),
        Some("Sort by install date, oldest first")
    );
    let p = &h2.state().settings.lists["apps"];
    assert_eq!((p.sort.as_str(), p.reverse), ("installed", true));
}

#[test]
fn ticked_apps_the_filters_hide_are_counted_and_listed_before_they_go() {
    let m = mac();
    let mut h = harness(&m.mac);
    go(&mut h);
    click(&mut h, "Select Jotter");
    click(&mut h, "Select Viewer");
    filter(&mut h, "Setapp (1)");
    assert_eq!(rows(&h), ["Jotter"]);
    assert!(
        has_text(&h, "2 apps selected (1 hidden by filters)"),
        "the bar counts every ticked app"
    );
    click(&mut h, "Review and Uninstall");
    // The review and the confirmation list both apps, filters or not.
    assert!(has(&h, "Uninstall 2 apps"));
    assert!(has(&h, &m.mac.env.display(&m.viewer)));
    assert!(has(&h, &m.mac.env.display(&m.jotter)));
    click(&mut h, "Uninstall 2 Apps");
    assert_eq!(
        h.query_all_by_label(&m.mac.env.display(&m.viewer)).count(),
        2,
        "the hidden app is in the confirmation"
    );
    click(&mut h, "Move to Trash");
    assert!(!m.viewer.exists() && !m.jotter.exists());
    assert!(m.editor.exists() && m.safari.exists());
}

#[test]
fn app_updates_search_filter_and_sort() {
    use cleancraft_core::testkit_apps::appcast;
    let mac = FakeMac::new();
    let feeds = [
        (
            "Alpha",
            "com.a.Alpha",
            "https://a.example/cast.xml",
            5_000_000,
        ),
        (
            "Beta",
            "com.b.Beta",
            "https://b.example/cast.xml",
            1_000_000,
        ),
        (
            "Gamma",
            "com.g.Gamma",
            "https://g.example/cast.xml",
            9_000_000,
        ),
    ];
    for (name, id, feed, size) in feeds {
        mac.app_spec(&AppSpec {
            feed: Some(feed),
            size,
            ..AppSpec::new(name, id, "1.0")
        });
        mac.web(feed, &appcast(&[("2.0", "2.0", "<p>New</p>")]));
    }
    mac.spotlight_finds_nothing();
    let mut h = harness(&mac);
    click(&mut h, "Apps");
    click(&mut h, "App Remover");
    settle(&mut h);
    click(&mut h, "App Updates");
    click(&mut h, "Check for Updates");
    settle(&mut h);
    let order = |h: &H| {
        let mut v: Vec<(f32, &str)> = ["Alpha", "Beta", "Gamma"]
            .iter()
            .filter_map(|n| {
                h.query_all_by_label(&format!("What's new in {n}"))
                    .next()
                    .map(|x| (x.rect().center().y, *n))
            })
            .collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        v.into_iter()
            .map(|(_, n)| n.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(order(&h), ["Alpha", "Beta", "Gamma"], "by name");
    sort(&mut h, "Sort by size");
    assert_eq!(order(&h), ["Gamma", "Alpha", "Beta"]);
    type_into(&mut h, "Search updates", "bet");
    assert_eq!(order(&h), ["Beta"]);
    assert!(has(&h, "Showing 1 of 3 updates"));
    click(&mut h, "Clear Filters");
    assert_eq!(order(&h).len(), 3);
    filter(&mut h, "App Store (0)");
    assert!(order(&h).is_empty());
    assert!(has(&h, "Showing 0 of 3 updates"));
    let h2 = {
        let mut h2 = harness(&mac);
        click(&mut h2, "Apps");
        click(&mut h2, "App Remover");
        h2
    };
    let p = &h2.state().settings.lists["updates"];
    assert_eq!(p.sort, "size");
    assert_eq!(p.filters, ["app_store"]);
}
