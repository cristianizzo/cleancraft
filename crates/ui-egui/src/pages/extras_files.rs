//! Extra controls for the file modules (Large & Old Files, Duplicates, Similar Images): where
//! to look, before the scan, and the "Select Copies" menu above duplicate and look-alike sets.
//! Also how deep Threat Check looks, kept with the same choices.
//!
//! The chosen location lives in core ([`ScanOptions`], `<data>/files.json`) because the scan
//! reads it; the text being typed and its error message live in egui memory.

use cleancraft_core::FeatureId;
use cleancraft_core::features::duplicates::{KeepRule, apply_keep_rule};
use cleancraft_core::features::large_old::{
    self, GroupBy, Location, ScanOptions, grouped_by, is_by_source, parse_folder, regroup,
};
use cleancraft_core::features::malware::ThreatDepth;
use cleancraft_core::fsutil::human_size;
use egui::{Id, Ui};

use crate::app::CleanApp;
use crate::kit;

#[derive(Clone, Default)]
struct Picker {
    /// "Another folder" is chosen but not confirmed yet.
    other: bool,
    text: String,
    error: Option<String>,
}

fn picker_id(f: FeatureId) -> Id {
    Id::new(("files-location", f.as_str()))
}

fn save_location(app: &mut CleanApp, f: FeatureId, loc: Location) -> bool {
    let mut o = ScanOptions::load(&app.env);
    o.set_location(f, loc);
    match o.save(&app.env) {
        Ok(()) => true,
        Err(e) => {
            app.warn(format!("The location could not be saved: {e}"));
            false
        }
    }
}

pub fn before_scan(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    match f {
        FeatureId::LargeOldFiles | FeatureId::Duplicates => location_picker(app, ui, f),
        FeatureId::SimilarPhotos => {
            let t = app.t;
            kit::dim(
                ui,
                &t,
                "Looks at pictures over 100 KB in Pictures and Downloads. Photos libraries are never opened.",
            );
            ui.add_space(6.0);
        }
        _ => {}
    }
}

fn location_picker(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    let t = app.t;
    let id = picker_id(f);
    let current = ScanOptions::load(&app.env).location(f).clone();
    let mut p: Picker = ui.data_mut(|d| d.get_temp(id)).unwrap_or_default();
    if let Location::Folder(dir) = &current
        && !p.other
        && p.text.is_empty()
    {
        p.text = app.env.display(dir);
    }
    kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width().min(600.0));
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        let mut choices: Vec<Location> = Location::PRESETS.to_vec();
        if f == FeatureId::LargeOldFiles {
            choices.push(Location::DownloadsBySource);
        }
        let folder_on = p.other || matches!(current, Location::Folder(_));
        let shown = if folder_on {
            "Another folder".to_string()
        } else {
            current.label(&app.env)
        };
        // One row: the question and a pop-up with the places (REVIEW-C C38: the radio rows
        // pushed the start screen's object out at 680 pt and wrapped raggedly).
        ui.horizontal(|ui| {
            ui.label(
                crate::rich("Where to look")
                    .font(crate::theme::font(crate::theme::size::BODY, crate::theme::Weight::SemiBold))
                    .color(t.text),
            );
            let r = kit::menu_pill(ui, &t, &shown, |ui| {
                ui.set_min_width(220.0);
                for loc in &choices {
                    let on = !folder_on && current == *loc;
                    if kit::menu_choice(ui, &loc.label(&app.env), on) {
                        app.act();
                        p.other = false;
                        p.error = None;
                        save_location(app, f, loc.clone());
                        ui.close();
                    }
                }
                if kit::menu_choice(ui, "Another folder", folder_on) {
                    app.act();
                    p.other = true;
                    p.error = None;
                    ui.close();
                }
            });
            if r.response.clicked() {
                app.act();
            }
        });
        if p.other || matches!(current, Location::Folder(_)) {
            let mut apply = false;
            ui.horizontal(|ui| {
                let r = kit::text_field(ui, &t, &mut p.text, "~/Projects", 300.0);
                r.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, crate::tr("Folder path"))
                });
                if p.other && !r.has_focus() && p.text.is_empty() && p.error.is_none() {
                    r.request_focus();
                }
                if r.changed() {
                    app.act();
                    p.error = None;
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    apply = true;
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    app.act();
                    p.error = None;
                    if !matches!(current, Location::Folder(_)) {
                        p.other = false;
                        p.text.clear();
                    }
                }
                let can = !p.text.trim().is_empty();
                if ui
                    .add_enabled_ui(can, |ui| kit::secondary(ui, &t, "Use Folder"))
                    .inner
                {
                    apply = true;
                }
            });
            if apply {
                app.act();
                match parse_folder(&app.env, &p.text) {
                    Ok(dir) => {
                        if save_location(app, f, Location::Folder(dir.clone())) {
                            p.other = false;
                            p.error = None;
                            p.text = app.env.display(&dir);
                        }
                    }
                    Err(e) => p.error = Some(e),
                }
            }
            if let Some(e) = &p.error {
                ui.label(crate::rich(e).color(t.danger));
            }
        }
        let now = ScanOptions::load(&app.env).location(f).clone();
        kit::dim(
            ui,
            &t,
            &format!(
                "Looking in: {}",
                match &now {
                    Location::Folder(d) => app.env.display(d),
                    other => other.label(&app.env),
                }
            ),
        );
        if f == FeatureId::LargeOldFiles && now != Location::DownloadsBySource {
            kit::dim(
                ui,
                &t,
                &format!(
                    "Large means {} or more; old means not opened for {} days (Settings).",
                    human_size(app.settings.large_min_bytes),
                    app.settings.old_days
                ),
            );
        }
        kit::dim(
            ui,
            &t,
            "Library, Documents, Desktop, iCloud, Photos and Music libraries and apps are left out.",
        );
    });
    });
    ui.data_mut(|d| d.insert_temp(id, p));
    ui.add_space(8.0);
}

/// Threat Check: how deep the next scan looks, kept in [`ScanOptions`].
pub fn threat_depth_picker(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let current = ScanOptions::load(&app.env).threat_depth;
    kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width().min(600.0));
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.horizontal(|ui| {
                ui.label(
                    crate::rich("How deep")
                        .font(crate::theme::font(
                            crate::theme::size::BODY,
                            crate::theme::Weight::SemiBold,
                        ))
                        .color(t.text),
                );
                let r = kit::menu_pill(ui, &t, current.label(), |ui| {
                    ui.set_min_width(180.0);
                    for d in ThreatDepth::ALL {
                        if kit::menu_choice(ui, d.label(), d == current) {
                            app.act();
                            let mut o = ScanOptions::load(&app.env);
                            o.threat_depth = d;
                            if let Err(e) = o.save(&app.env) {
                                app.warn(format!("The choice could not be saved: {e}"));
                            }
                            ui.close();
                        }
                    }
                });
                if r.response.clicked() {
                    app.act();
                }
            });
            kit::dim(ui, &t, ScanOptions::load(&app.env).threat_depth.blurb());
        });
    });
    ui.add_space(8.0);
}

pub fn results_toolbar(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    if f == FeatureId::LargeOldFiles {
        group_menu(app, ui);
        return;
    }
    if !matches!(f, FeatureId::Duplicates | FeatureId::SimilarPhotos) {
        return;
    }
    let mut rules = vec![
        ("Keep Suggested", KeepRule::Smart),
        ("Keep Newest", KeepRule::Newest),
        ("Keep Oldest", KeepRule::Oldest),
    ];
    if f == FeatureId::SimilarPhotos {
        rules.push(("Keep Largest File", KeepRule::Largest));
    }
    let mut chosen = None;
    // A pill like the buttons beside it (egui's menu button, restyled).
    let r = kit::menu_pill(ui, &app.t, "Select Copies", |ui| {
        for (label, rule) in &rules {
            if kit::menu_item(ui, label).clicked() {
                chosen = Some(*rule);
                ui.close();
            }
        }
    });
    if r.response.clicked() {
        app.act();
    }
    if let Some(rule) = chosen {
        app.act();
        let env = app.env.clone();
        if let Some(p) = app.plan_state(f).plan.as_mut() {
            apply_keep_rule(&env, p, rule);
        }
    }
}

/// Large & Old Files: "Group by: Kind / Size / Age". Only the groups on the left change; the
/// list, the sizes and what is ticked stay (each group's checkbox selects all or none of it).
/// Downloads by source app keeps its groups by app, so the menu is not shown there.
fn group_menu(app: &mut CleanApp, ui: &mut Ui) {
    let f = FeatureId::LargeOldFiles;
    let Some(current) = app
        .plan_state(f)
        .plan
        .as_ref()
        .filter(|p| !is_by_source(p))
        .map(grouped_by)
    else {
        return;
    };
    let mut chosen = None;
    let r = kit::menu_pill(
        ui,
        &app.t,
        &format!("Group by: {}", current.label()),
        |ui| {
            ui.set_min_width(160.0);
            for by in GroupBy::ALL {
                if kit::menu_choice(ui, by.label(), by == current) {
                    chosen = Some(by);
                    ui.close();
                }
            }
        },
    );
    if r.response.clicked() {
        app.act();
    }
    if let Some(by) = chosen {
        app.act();
        set_grouping(app, by);
    }
}

/// Group the Large & Old list by `by` now, and remember it for the next scans.
pub fn set_grouping(app: &mut CleanApp, by: GroupBy) {
    let f = FeatureId::LargeOldFiles;
    let mut o = ScanOptions::load(&app.env);
    if o.large_old_group != by {
        o.large_old_group = by;
        if let Err(e) = o.save(&app.env) {
            app.warn(format!("The grouping could not be saved: {e}"));
        }
    }
    let st = app.plan_state(f);
    if let Some(p) = st.plan.as_mut() {
        regroup(p, by, large_old::now_secs());
    }
    // The groups changed: show them all.
    st.category = None;
}
