//! Settings: where removed items go, the ignore list, thresholds, agents, menu bar.

use cleancraft_core::executor::{self, Method};
use cleancraft_core::fsutil::human_size;
use cleancraft_core::schedule::{self, ScanEvery};
use cleancraft_core::{login_agent, notify};
use egui::{Sense, Ui, vec2};

use crate::app::CleanApp;
use crate::icons::{self, Icon};
use crate::kit;
use crate::theme::{Weight, font, size, space};

/// A card's heading with its icon.
pub(crate) fn heading(ui: &mut Ui, t: &crate::theme::Tokens, icon: Icon, text: &str) {
    ui.horizontal(|ui| {
        kit::badge(ui, t.sec_a, icon, 28.0);
        ui.add_space(2.0);
        ui.label(
            crate::rich(text)
                .font(font(size::TITLE3, Weight::SemiBold))
                .color(t.text),
        );
    });
    ui.add_space(2.0);
}

/// A form row: the label in a leading column, the slider and its value with the unit after
/// it. The slider keeps `name` as its accessible name.
/// A form row: label in the right-aligned label column, a switch after it, and `text`
/// beside the switch. Returns true when clicked. The switch is announced as `text`.
pub(crate) fn switch_row(
    ui: &mut Ui,
    t: &crate::theme::Tokens,
    on: bool,
    text: &str,
    label: &str,
) -> bool {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(260.0, crate::theme::hit::FIELD), Sense::hover());
        ui.painter().text(
            r.right_center() - vec2(12.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            crate::tr(label),
            font(size::BODY, Weight::Regular),
            t.text_dim,
        );
        let c = kit::switch(ui, t, on, text).clicked();
        // Painted: the switch already carries the words for VoiceOver.
        kit::heading(ui, text, font(size::CALLOUT, Weight::Medium), t.text);
        c
    })
    .inner
}

fn slider_row(
    ui: &mut Ui,
    t: &crate::theme::Tokens,
    name: &str,
    label: &str,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    unit: &str,
) -> bool {
    slider_row_with(ui, t, name, label, value, range, &|v| format!("{v}{unit}"))
}

/// [`slider_row`] with its own value text ("10:00", "10% free").
fn slider_row_with(
    ui: &mut Ui,
    t: &crate::theme::Tokens,
    name: &str,
    label: &str,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    show: &dyn Fn(u32) -> String,
) -> bool {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(260.0, crate::theme::hit::FIELD), Sense::hover());
        ui.painter().text(
            r.right_center() - vec2(12.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            crate::tr(label),
            font(size::BODY, Weight::Regular),
            t.text_dim,
        );
        ui.spacing_mut().slider_width = (ui.available_width() - 140.0).clamp(160.0, 360.0);
        // Wide ranges (10 MB to 5 GB) move in even steps of scale, not of megabytes.
        let wide = *range.end() >= range.start().saturating_mul(50).max(1);
        let r = kit::slider(
            ui,
            egui::Slider::new(value, range)
                .show_value(false)
                .logarithmic(wide)
                .trailing_fill(true),
        );
        // A slider senses drags only, so kit::slider's click check misses it.
        if r.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let v = *value as f64;
        let name = name.to_string();
        r.widget_info(|| egui::WidgetInfo::slider(true, v, crate::tr(name.clone())));
        // The value, beside the slider: shown "50 MB", announced with what it sets.
        let shown = show(*value);
        let full = format!("{label}: {shown}");
        let fid = font(size::CALLOUT, Weight::SemiBold);
        let w = kit::galley(ui, &shown, fid.clone(), t.text).size().x + 4.0;
        let (vr, _) = ui.allocate_exact_size(vec2(w.max(64.0), 20.0), Sense::hover());
        kit::text_at(ui, vr, &shown, &full, fid, t.text, egui::Align::Min);
        r.changed()
    })
    .inner
}

/// What the quarantine holds now, a way to see it in Finder, and "Move Quarantine to
/// Trash..." (REVIEW-U U5): every item moves to the Trash, journaled, with Undo. Nothing is
/// deleted here; Finder's Empty Trash is what frees the space.
fn quarantine_line(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let changed = app.settings_page.watch.changes(&app.env).is_some();
    if app.settings_page.quarantine.is_none() || changed {
        app.settings_page.watch.prime(&app.env);
        app.settings_page.quarantine = Some(executor::quarantine_size(&app.env));
    }
    let (bytes, items) = app.settings_page.quarantine.unwrap_or((0, 0));
    ui.add_space(space::XS);
    ui.horizontal(|ui| {
        ui.add_space(28.0);
        let text = if items == 0 {
            "The quarantine is empty.".to_string()
        } else {
            format!(
                "The quarantine holds {} ({}). Items stay there until you put them back from History or move them to the Trash.",
                human_size(bytes),
                cleancraft_core::fsutil::items(items)
            )
        };
        ui.add(
            egui::Label::new(
                crate::rich(text)
                    .font(font(size::FOOTNOTE, Weight::Regular))
                    .color(t.text_faint),
            )
            .wrap(),
        );
    });
    if items == 0 {
        return;
    }
    ui.add_space(space::XS);
    ui.horizontal(|ui| {
        ui.add_space(28.0);
        if kit::pill(
            ui,
            &t,
            "Show Quarantine in Finder",
            kit::Kind::Secondary,
            Some(Icon::Reveal),
        )
        .clicked()
        {
            app.act();
            let dir = app.env.quarantine_dir().to_string_lossy().into_owned();
            match app.env.sys.run("/usr/bin/open", &[&dir]) {
                Ok(out) if out.ok() => {}
                Ok(out) => app.warn(format!("Could not open Finder: {}", out.stderr.trim())),
                Err(e) => app.warn(format!("Could not open Finder: {e}")),
            }
        }
        if kit::pill(
            ui,
            &t,
            MOVE_QUARANTINE,
            kit::Kind::Secondary,
            Some(Icon::Trash),
        )
        .on_hover_text(crate::tr(
            "Move everything in the quarantine to the Trash. You can put it back from History.",
        ))
        .clicked()
        {
            app.act();
            app.settings_page.confirm_quarantine = true;
        }
    });
    if app.settings_page.confirm_quarantine {
        confirm_quarantine(app, ui, bytes, items);
    }
}

pub const MOVE_QUARANTINE: &str = "Move Quarantine to Trash...";

/// The AI assistants switch, also its accessible name.
pub const AGENTS_SWITCH: &str = "Let AI assistants clean after you confirm";

/// The confirmation, then the move, with Undo in the notice and in History. The dialog lists
/// every item that moves, with its size, as it was when the dialog opened; exactly those move
/// (CHK7-5).
fn confirm_quarantine(app: &mut CleanApp, ui: &mut Ui, _bytes: u64, _items: usize) {
    let t = app.t;
    let list_id = egui::Id::new("confirm-quarantine-list");
    let list: Vec<(std::path::PathBuf, u64)> = match ui
        .ctx()
        .data_mut(|d| d.get_temp::<Vec<(std::path::PathBuf, u64)>>(list_id))
    {
        Some(l) => l,
        None => {
            let l = executor::quarantine_list(&app.env);
            ui.ctx().data_mut(|d| d.insert_temp(list_id, l.clone()));
            l
        }
    };
    let bytes: u64 = list.iter().map(|(_, s)| s).sum();
    let items = list.len();
    let mut go = false;
    let mut cancel = false;
    let dlg = kit::modal(egui::Id::new("confirm-quarantine-trash"), &t).show(ui.ctx(), |ui| {
        ui.set_width(560.0);
        kit::dialog_head(
            ui,
            &t,
            Icon::Trash,
            t.warning,
            &format!(
                "Move the quarantine to the Trash ({}, {})?",
                human_size(bytes),
                cleancraft_core::fsutil::items(items)
            ),
            "These items move from the CleanCraft quarantine to the Trash. Nothing is deleted: you can put them back from History until the Trash is emptied.",
        );
        ui.add_space(12.0);
        let rows: Vec<(String, String, u64)> = list
            .iter()
            .map(|(p, size)| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (name, String::new(), *size)
            })
            .collect();
        crate::pages::extras_cleanup::erase_list(ui, &t, &rows);
        ui.add_space(16.0);
        let (ok, no) = kit::dialog_actions(ui, &t, "Move to Trash", kit::Kind::Danger);
        go |= ok;
        cancel |= no;
    });
    if dlg.should_close() {
        cancel = true;
    }
    if cancel || go {
        ui.ctx()
            .data_mut(|d| d.remove::<Vec<(std::path::PathBuf, u64)>>(list_id));
    }
    if cancel {
        app.act();
        app.settings_page.confirm_quarantine = false;
    } else if go {
        app.act();
        app.settings_page.confirm_quarantine = false;
        let confirmed: Vec<std::path::PathBuf> = list.into_iter().map(|(p, _)| p).collect();
        match executor::move_quarantine_items_to_trash(&app.env, &confirmed, &mut |_, _| {}) {
            Ok(rep) => {
                app.settings_page.quarantine = None;
                let moved: Vec<std::path::PathBuf> = CleanApp::moved_paths(&rep);
                app.files_moved(&moved);
                if rep.moved.is_empty() {
                    app.warn(format!(
                        "Nothing moved: {}",
                        rep.skipped
                            .first()
                            .map(|(_, w)| w.to_string())
                            .unwrap_or_else(|| "the quarantine is empty".into())
                    ));
                } else {
                    let plan_id = rep.plan_id.clone();
                    let mut msg = format!(
                        "{} ({}) moved from the quarantine to the Trash.",
                        cleancraft_core::fsutil::items(rep.moved.len()),
                        human_size(rep.freed)
                    );
                    if !rep.skipped.is_empty() {
                        msg.push_str(&format!(
                            " {} stayed: {}.",
                            cleancraft_core::fsutil::items(rep.skipped.len()),
                            rep.skipped[0].1
                        ));
                    }
                    app.tell_undo(msg, "Move to Trash", move |app| {
                        let (n, errs) = executor::undo_plan(&app.env, &plan_id);
                        app.files_back(&[]);
                        app.settings_page.quarantine = None;
                        if errs.is_empty() {
                            app.tell(format!(
                                "{} back in the quarantine.",
                                cleancraft_core::fsutil::items(n)
                            ));
                        } else {
                            app.warn(format!(
                                "{} back in the quarantine, {} could not be: {}",
                                cleancraft_core::fsutil::items(n),
                                errs.len(),
                                errs[0].1
                            ));
                        }
                    });
                }
            }
            Err(e) => app.warn(format!("Nothing moved: {e}")),
        }
    }
}

#[derive(Default)]
pub struct State {
    /// Text box for adding an ignore-list path.
    pub new_ignore: String,
    /// What the quarantine holds (bytes, items), read again after each move or undo.
    quarantine: Option<(u64, usize)>,
    /// "Move Quarantine to Trash..." asks first.
    confirm_quarantine: bool,
    watch: super::history::JournalWatch,
    /// The macOS folder dialog, while it is open.
    pick: Option<crate::jobs::Job<Result<Option<std::path::PathBuf>, String>>>,
    /// The folder dialog of the Developer Clutter group (a code folder to add).
    pick_dev: Option<crate::jobs::Job<Result<Option<std::path::PathBuf>, String>>>,
    /// Text box for adding a code folder by its path.
    pub new_dev_root: String,
    /// A route asked for the Full Disk Access group: scroll it into view once.
    pub focus_fda: bool,
    /// Developer Clutter asked for the code folders group: scroll it into view once.
    pub focus_dev: bool,
    /// "Open the app at this page" requests from the menu bar agent and `cleancraft open`.
    inbox: notify::OpenInbox,
    /// When the inbox was last read (egui time, seconds).
    inbox_at: Option<f64>,
}

/// macOS's own folder dialog (through osascript and env.sys): the folder chosen, or None when
/// the person cancels.
pub fn choose_folder(
    env: &cleancraft_core::Env,
    prompt: &str,
) -> Result<Option<std::path::PathBuf>, String> {
    let script = format!(
        "POSIX path of (choose folder with prompt \"{}\")",
        prompt.replace(['\\', '"'], "")
    );
    let out = env
        .sys
        .run("/usr/bin/osascript", &["-e", &script])
        .map_err(|e| format!("The folder dialog could not open: {e}"))?;
    if out.status != 0 {
        // -128: the person chose Cancel.
        if out.stderr.contains("-128") {
            return Ok(None);
        }
        return Err(format!(
            "The folder dialog could not open: {}",
            out.stderr.trim()
        ));
    }
    let path = out.stdout.trim().trim_end_matches('/');
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(env.sys_path(path)))
}

pub fn ui(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    kit::header(
        ui,
        &t,
        Icon::Settings,
        "Settings",
        "Where removed items go, Full Disk Access, what scans leave alone, how big and old files are judged, code folders, AI assistants, the menu bar, updates and sounds. Changes are saved as you make them.",
    );
    let mut changed = false;
    // The folder dialog answered.
    if let Some(job) = app.settings_page.pick.as_mut() {
        if let Some(r) = job.poll() {
            app.settings_page.pick = None;
            match r {
                Ok(Some(p)) if !app.settings.ignore.contains(&p) => {
                    app.settings.ignore.push(p);
                    changed = true;
                }
                Ok(_) => {}
                Err(e) => app.warn(e),
            }
        } else if job.is_dead() {
            app.settings_page.pick = None;
        } else {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }
    }
    changed |= poll_dev_pick(app, ui.ctx());
    let out = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            kit::card(&t).show(ui, |ui| {
                ui.set_width(ui.available_width());
                heading(ui, &t, Icon::Trash, "Removed items go to");
                let mut m = app.settings.method;
                for (value, text) in [
                    (Method::Trash, "The Trash (empty it yourself when you are sure)"),
                    (Method::Quarantine, "CleanCraft quarantine"),
                ] {
                    if kit::radio(ui, &t, m == value, text).clicked() {
                        m = value;
                    }
                    // Where the quarantine is, as a dim line under its choice (REVIEW-U U26).
                    if value == Method::Quarantine {
                        ui.horizontal(|ui| {
                            ui.add_space(28.0);
                            kit::dim(ui, &t, &app.env.display(&app.env.quarantine_dir()));
                        });
                    }
                }
                if m != app.settings.method {
                    app.settings.method = m;
                    changed = true;
                }
                quarantine_line(app, ui);
            });
            ui.add_space(space::M);
            fda_card(app, ui);
            ui.add_space(space::M);
            kit::card(&t).show(ui, |ui| {
                ui.set_width(ui.available_width());
                heading(ui, &t, Icon::Shield, "Ignore list");
                kit::dim(
                    ui,
                    &t,
                    "Scans never propose these paths or anything inside them.",
                );
                ui.add_space(space::XS);
                let list = app.settings.ignore.clone();
                if list.is_empty() {
                    ui.label(
                        crate::rich("Nothing is ignored yet. Add a folder or file below, or choose Ignore on an app in App Remover.")
                            .font(font(size::CALLOUT, Weight::Regular))
                            .color(t.text_faint),
                    );
                }
                for (i, p) in list.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.set_min_height(crate::theme::hit::FIELD);
                        let shown = app.env.display(p);
                        let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                        icons::paint(ui.painter(), r, Icon::Folder, t.text_dim);
                        // The path, shortened in the middle so the Remove button stays in
                        // reach; the whole path in the tooltip and for VoiceOver.
                        let fid = font(size::CALLOUT, Weight::Regular);
                        let room = (ui.available_width() - 120.0).max(80.0);
                        let seen = super::history::short_path(&app.env, p);
                        let short = kit::truncate_middle(ui.ctx(), &seen, &fid, room);
                        let w = kit::galley(ui, &short, fid.clone(), t.text).size().x + 2.0;
                        let (pr, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
                        kit::text_at(ui, pr, &short, &shown, fid, t.text, egui::Align::Min)
                            .on_hover_text(crate::tr(&seen));
                        let r = kit::ghost(ui, &t, "Remove", Some(Icon::Close));
                        r.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                crate::tr(format!("Remove {shown}")),
                            )
                        });
                        if r.clicked() {
                            app.settings.ignore.remove(i);
                            changed = true;
                        }
                    });
                }
                ui.horizontal(|ui| {
                    let r = kit::text_field(
                        ui,
                        &t,
                        &mut app.settings_page.new_ignore,
                        "Path to ignore",
                        380.0,
                    );
                    r.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::TextEdit,
                            true,
                            crate::tr("Path to ignore"),
                        )
                    });
                    if r.changed() {
                        app.act();
                    }
                    let enter = r.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && !app.settings_page.new_ignore.trim().is_empty();
                    if kit::secondary(ui, &t, "Add to Ignore List") || enter {
                        let raw = app.settings_page.new_ignore.trim().to_string();
                        app.act();
                        if raw.is_empty() {
                            app.warn("Type a path first.");
                        } else {
                            let p = if raw == "~" {
                                app.env.home().to_path_buf()
                            } else if let Some(rest) = raw.strip_prefix("~/") {
                                app.env.home_path(rest)
                            } else {
                                app.env.sys_path(&raw)
                            };
                            if std::fs::symlink_metadata(&p).is_err() {
                                app.warn(format!(
                                    "Nothing is at {raw}. Check the path, or use Choose Folder."
                                ));
                            } else if app.settings.ignore.contains(&p) {
                                app.warn(format!("{raw} is on the list already."));
                            } else {
                                app.settings.ignore.push(p);
                                app.settings_page.new_ignore.clear();
                                changed = true;
                            }
                        }
                    }
                    let picking = app.settings_page.pick.is_some();
                    let ch = ui
                        .add_enabled_ui(!picking, |ui| {
                            kit::pill(ui, &t, "Choose Folder...", kit::Kind::Secondary, Some(Icon::Folder))
                        })
                        .inner;
                    if ch.on_hover_text(crate::tr("Pick a folder in the macOS file dialog")).clicked() && !picking {
                        app.act();
                        let env = app.env.clone();
                        app.settings_page.pick = Some(crate::jobs::Job::start(app.sync, move |_| {
                            choose_folder(&env, "Choose a folder CleanCraft should leave alone")
                        }));
                    }
                });
            });
            ui.add_space(space::M);
            kit::card(&t).show(ui, |ui| {
                ui.set_width(ui.available_width());
                heading(ui, &t, Icon::LargeOld, "Thresholds");
                let mut mb = (app.settings.large_min_bytes / 1_000_000) as u32;
                if slider_row(ui, &t, "Large file minimum (MB)", "Large files start at", &mut mb, 10..=5000, " MB") {
                    app.settings.large_min_bytes = mb as u64 * 1_000_000;
                    changed = true;
                }
                let mut days = app.settings.old_days as u32;
                if slider_row(ui, &t, "Old means not opened for (days)", "Old means not opened for", &mut days, 30..=1825, " days") {
                    app.settings.old_days = days as u64;
                    changed = true;
                }
                let mut dl = app.settings.downloads_days as u32;
                if slider_row(ui, &t, "Unused disk images after (days)", "Disk images count as unused after", &mut dl, 7..=730, " days") {
                    app.settings.downloads_days = dl as u64;
                    changed = true;
                }
            });
            ui.add_space(space::M);
            changed |= dev_clutter_card(app, ui);
            ui.add_space(space::M);
            super::threat_rules::card(app, ui);
            ui.add_space(space::M);
            kit::card(&t).show(ui, |ui| {
                ui.set_width(ui.available_width());
                heading(ui, &t, Icon::Sweep, "AI assistants");
                // A switch like the Sounds group, in plain words (REVIEW-U U26).
                if switch_row(
                    ui,
                    &t,
                    app.settings.agents_may_clean,
                    AGENTS_SWITCH,
                    "Assistants",
                ) {
                    app.settings.agents_may_clean = !app.settings.agents_may_clean;
                    changed = true;
                }
                ui.horizontal(|ui| {
                    ui.add_space(272.0);
                    ui.add(
                        egui::Label::new(
                            crate::rich(if app.settings.agents_may_clean {
                                "Assistants can move the items of a plan you confirmed. Everything they move is in History, with Undo."
                            } else {
                                "Off: assistants can scan and propose, but nothing moves unless you clean here."
                            })
                            .font(font(size::FOOTNOTE, Weight::Regular))
                            .color(t.text_faint),
                        )
                        .wrap(),
                    );
                });
                ai_links(app, ui);
            });
            ui.add_space(space::M);
            changed |= menu_bar_card(app, ui);
            ui.add_space(space::M);
            changed |= super::updates::card(app, ui);
            ui.add_space(space::M);
            changed |= language_card(app, ui);
            ui.add_space(space::M);
            kit::card(&t).show(ui, |ui| {
                ui.set_width(ui.available_width());
                heading(ui, &t, Icon::Monitor, "Sounds");
                if switch_row(ui, &t, app.settings.sounds, "Play sounds", "Sounds") {
                    app.settings.sounds = !app.settings.sounds;
                    changed = true;
                }
                ui.add_enabled_ui(app.settings.sounds, |ui| {
                    ui.horizontal(|ui| {
                        let (r, _) =
                            ui.allocate_exact_size(vec2(260.0, crate::theme::hit::FIELD), Sense::hover());
                        ui.painter().text(
                            r.right_center() - vec2(12.0, 0.0),
                            egui::Align2::RIGHT_CENTER,
                            crate::tr("Volume"),
                            font(size::BODY, Weight::Regular),
                            t.text_dim,
                        );
                        ui.spacing_mut().slider_width =
                            (ui.available_width() - 160.0).clamp(160.0, 300.0);
                        let mut v = app.settings.sound_volume;
                        let r = kit::slider(
                            ui,
                            egui::Slider::new(&mut v, 0.0..=1.0)
                                .show_value(false)
                                .trailing_fill(true),
                        );
                        let shown = v as f64;
                        r.widget_info(|| egui::WidgetInfo::slider(true, shown, crate::tr("Volume")));
                        if r.changed() {
                            app.settings.sound_volume = v;
                            changed = true;
                        }
                        let p = kit::pill(ui, &t, "Preview", kit::Kind::Secondary, None);
                        p.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, crate::tr("Preview sound"))
                        });
                        if p.clicked() {
                            app.act();
                            crate::sound::preview(&app.env, app.settings.sound_volume);
                        }
                    });
                    if switch_row(
                        ui,
                        &t,
                        app.settings.sound_ticks,
                        "Click sounds on switches",
                        "Clicks",
                    ) {
                        app.settings.sound_ticks = !app.settings.sound_ticks;
                        changed = true;
                    }
                });
            });
            ui.add_space(space::M);
        });
    kit::scroll_fade_page(ui, &out, &t);
    if changed {
        app.act();
        app.save_settings();
    }
}

/// The links of the AI assistants group: the guide and the "Connect your AI apps" step.
pub const AGENT_GUIDE_LINK: &str = "How to connect an AI assistant";
pub const CONNECT_AI_BUTTON: &str = "Connect AI Apps...";

fn ai_links(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    ui.add_space(space::XS);
    let mut go = None;
    ui.horizontal_wrapped(|ui| {
        ui.add_space(272.0);
        if kit::pill(
            ui,
            &t,
            CONNECT_AI_BUTTON,
            kit::Kind::Secondary,
            Some(Icon::Sweep),
        )
        .on_hover_text(crate::tr(
            "Add CleanCraft to Claude Desktop or Claude Code, after you see the exact change",
        ))
        .clicked()
        {
            go = Some(cleancraft_core::route::AppRoute::ConnectAi);
        }
        if kit::ghost(ui, &t, AGENT_GUIDE_LINK, Some(Icon::Chevron))
            .on_hover_text(crate::tr("Step by step, with the tools an agent can use"))
            .clicked()
        {
            go = Some(cleancraft_core::route::AppRoute::AgentGuide);
        }
    });
    if let Some(r) = go {
        app.act();
        app.open_route(&r);
    }
}

/// The Full Disk Access group: whether CleanCraft has it, and the way to turn it on
/// (the "full_disk_access" route of the Trash watch lands here).
pub const FDA_OPEN: &str = "Open Privacy Settings";
pub const FDA_CHECK: &str = "Check Again";

fn fda_card(app: &mut CleanApp, ui: &mut Ui) {
    use cleancraft_core::permissions::{self, Access};
    let t = app.t;
    let r = kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        heading(ui, &t, Icon::Lock, "Full Disk Access");
        let text = match app.fda {
            Access::Granted => "On: CleanCraft can read Mail, Safari and the Trash, and the Trash watch can see apps you drag there.",
            Access::Unknown => "CleanCraft could not tell yet: none of the private places macOS protects exists on this Mac.",
            Access::Missing => "Off: macOS keeps Mail, Safari and the Trash private. In System Settings, open Privacy & Security, then Full Disk Access, turn on CleanCraft, and come back to Check Again.",
        };
        ui.add(
            egui::Label::new(
                crate::rich(text)
                    .font(font(size::CALLOUT, Weight::Regular))
                    .color(if app.fda == Access::Missing { t.text } else { t.text_dim }),
            )
            .wrap(),
        );
        ui.add_space(space::XS);
        ui.horizontal(|ui| {
            if app.fda != Access::Granted && kit::secondary(ui, &t, FDA_OPEN) {
                app.act();
                if let Err(e) = permissions::open_fda_settings(&app.env) {
                    app.warn(format!("Could not open System Settings: {e}"));
                }
            }
            if kit::ghost(ui, &t, FDA_CHECK, Some(Icon::Undo))
                .on_hover_text(crate::tr("Read the Full Disk Access setting again"))
                .clicked()
            {
                app.act();
                app.fda = permissions::full_disk_access(&app.env);
                match app.fda {
                    Access::Missing => app.warn("Full Disk Access is still off."),
                    _ => app.tell("Full Disk Access is on."),
                }
            }
        });
    });
    if std::mem::take(&mut app.settings_page.focus_fda) {
        r.response.scroll_to_me(Some(egui::Align::TOP));
    }
}

/// The Developer Clutter group: code folders to look in besides the usual ones, and when a
/// project counts as inactive. Returns true when a setting changed.
pub const DEV_ADD: &str = "Add Code Folder";
pub const DEV_CHOOSE: &str = "Choose Code Folder...";

fn dev_clutter_card(app: &mut CleanApp, ui: &mut Ui) -> bool {
    let t = app.t;
    let mut changed = false;
    let focus = std::mem::take(&mut app.settings_page.focus_dev);
    let card = kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        heading(ui, &t, Icon::Junk, "Code folders for Developer Clutter");
        let usual: Vec<String> = cleancraft_core::features::dev_clutter::DEFAULT_ROOTS
            .iter()
            .map(|d| format!("~/{d}"))
            .collect();
        ui.add(
            egui::Label::new(
                crate::rich(format!(
                    "Projects are looked for in {} and in the code folders below.",
                    usual.join(", ")
                ))
                .font(font(size::CALLOUT, Weight::Regular))
                .color(t.text_dim),
            )
            .wrap(),
        );
        ui.add_space(space::XS);
        let list = app.settings.dev_roots.clone();
        if list.is_empty() {
            kit::dim(ui, &t, "No code folders of your own yet.");
        }
        for (i, p) in list.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.set_min_height(crate::theme::hit::FIELD);
                let shown = app.env.display(p);
                let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                icons::paint(ui.painter(), r, Icon::Folder, t.text_dim);
                let fid = font(size::CALLOUT, Weight::Regular);
                let room = (ui.available_width() - 120.0).max(80.0);
                let seen = super::history::short_path(&app.env, p);
                let short = kit::truncate_middle(ui.ctx(), &seen, &fid, room);
                let w = kit::galley(ui, &short, fid.clone(), t.text).size().x + 2.0;
                let (pr, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
                kit::text_at(ui, pr, &short, &shown, fid, t.text, egui::Align::Min)
                    .on_hover_text(crate::tr(&seen));
                let r = kit::ghost(ui, &t, "Remove", Some(Icon::Close));
                r.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        crate::tr(format!("Remove code folder {shown}")),
                    )
                });
                if r.clicked() {
                    app.act();
                    app.settings.dev_roots.remove(i);
                    dev_roots_changed(app);
                    changed = true;
                }
            });
        }
        ui.horizontal(|ui| {
            let r = kit::text_field(
                ui,
                &t,
                &mut app.settings_page.new_dev_root,
                "Path of a code folder",
                380.0,
            );
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, crate::tr("Path of a code folder"))
            });
            let enter = r.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                && !app.settings_page.new_dev_root.trim().is_empty();
            if kit::secondary(ui, &t, DEV_ADD) || enter {
                app.act();
                let raw = app.settings_page.new_dev_root.trim().to_string();
                if raw.is_empty() {
                    app.warn("Type the path of a code folder first, or use Choose Code Folder.");
                } else {
                    let p = if raw == "~" {
                        app.env.home().to_path_buf()
                    } else if let Some(rest) = raw.strip_prefix("~/") {
                        app.env.home_path(rest)
                    } else {
                        app.env.sys_path(&raw)
                    };
                    if add_dev_root(app, p) {
                        app.settings_page.new_dev_root.clear();
                        changed = true;
                    }
                }
            }
            let picking = app.settings_page.pick_dev.is_some();
            let ch = ui
                .add_enabled_ui(!picking, |ui| {
                    kit::pill(ui, &t, DEV_CHOOSE, kit::Kind::Secondary, Some(Icon::Folder))
                })
                .inner;
            if ch.on_hover_text(crate::tr("Pick a folder that holds your projects")).clicked() && !picking {
                app.act();
                pick_dev_root(app);
            }
        });
        let mut days = app.settings.dev_inactive_days.clamp(1, 365) as u32;
        if slider_row(
            ui,
            &t,
            "Projects count as inactive after (days)",
            "Projects count as inactive after",
            &mut days,
            7..=365,
            " days",
        ) {
            app.settings.dev_inactive_days = days as u64;
            changed = true;
        }
        ui.horizontal(|ui| {
            ui.add_space(272.0);
            ui.add(
                egui::Label::new(
                    crate::rich("Build folders of an inactive project are selected for you. Active projects are listed but left unticked.")
                        .font(font(size::FOOTNOTE, Weight::Regular))
                        .color(t.text_faint),
                )
                .wrap(),
            );
        });
    });
    if focus {
        card.response.scroll_to_me(Some(egui::Align::TOP));
    }
    changed
}

/// Open macOS's folder dialog to add a code folder (Settings and Developer Clutter's empty
/// result). The answer is read by [`poll_dev_pick`].
pub fn picking_dev(app: &CleanApp) -> bool {
    app.settings_page.pick_dev.is_some()
}

/// Open the folder dialog (see [`poll_dev_pick`]).
pub fn pick_dev_root(app: &mut CleanApp) {
    if app.settings_page.pick_dev.is_some() {
        return;
    }
    let env = app.env.clone();
    app.settings_page.pick_dev = Some(crate::jobs::Job::start(app.sync, move |_| {
        choose_folder(&env, "Choose a folder that holds your code projects")
    }));
}

/// The folder dialog's answer, added when one was chosen (the settings are saved). Returns
/// true when a code folder was added.
pub fn poll_dev_pick(app: &mut CleanApp, ctx: &egui::Context) -> bool {
    let Some(job) = app.settings_page.pick_dev.as_mut() else {
        return false;
    };
    if let Some(r) = job.poll() {
        app.settings_page.pick_dev = None;
        match r {
            Ok(Some(p)) => {
                if add_dev_root(app, p) {
                    app.save_settings();
                    return true;
                }
            }
            Ok(None) => {}
            Err(e) => app.warn(e),
        }
    } else if job.is_dead() {
        app.settings_page.pick_dev = None;
    } else {
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }
    false
}

/// The code folders changed: Developer Clutter's list is out of date and scans again when it
/// is shown (U7-6).
fn dev_roots_changed(app: &mut CleanApp) {
    if let Some(st) = app.plans.get_mut(&cleancraft_core::FeatureId::DevClutter)
        && st.plan.is_some()
    {
        st.stale = true;
    }
}

/// Add a code folder after checking it: a real folder (not a link), not the home folder or
/// a folder holding it, not on the list yet. Says why when it is refused.
fn add_dev_root(app: &mut CleanApp, p: std::path::PathBuf) -> bool {
    let shown = app.env.display(&p);
    let meta = std::fs::symlink_metadata(&p);
    if meta.is_err() {
        app.warn(format!(
            "Nothing is at {shown}. Check the path, or use Choose Code Folder."
        ));
        return false;
    }
    if meta.is_ok_and(|m| !m.is_dir()) {
        app.warn(format!(
            "{shown} is not a folder (or it is a link). Add the folder that holds your projects."
        ));
        return false;
    }
    if cleancraft_core::policy::starts_with_ci(app.env.home(), &p) {
        app.warn(format!(
            "{shown} holds your whole home folder. Add the folders that hold your projects instead."
        ));
        return false;
    }
    if app.settings.dev_roots.contains(&p) {
        app.warn(format!("{shown} is on the list already."));
        return false;
    }
    app.settings.dev_roots.push(p);
    dev_roots_changed(app);
    app.tell(format!(
        "{shown} is added: Developer Clutter looks there the next time you open it."
    ));
    true
}

/// The menu bar switch, also its accessible name.
pub const MENU_BAR_SWITCH: &str = "Show CleanCraft in the menu bar";
pub const DISK_ALERT_SWITCH: &str = "Warn when the startup disk is almost full";
pub const MEMORY_ALERT_SWITCH: &str = "Warn when memory stays under pressure";
pub const TRASH_ALERT_SWITCH: &str = "Warn when the Trash holds over 5 GB";
pub const PAUSE_SWITCH: &str = "Pause scheduled scans and notifications";
pub const TRASH_OFFER_SWITCH: &str = "Offer to clean up after an app goes to the Trash";
pub const NEW_ITEMS_SWITCH: &str = "Check new apps and startup items";
/// Said under the menu bar group: where a notification leads (U7-2).
pub const NOTIFY_NOTE: &str = "Clicking a notification opens CleanCraft on the page it is about. What it found also waits in the menu bar panel, and the icon shows a dot until you open it. While paused, scheduled scans are skipped and nothing is notified.";
/// Said under the Trash offer switch.
pub const TRASH_FDA_NOTE: &str = "Needs Full Disk Access (macOS keeps the Trash private).";
/// The button beside it while Full Disk Access is off.
pub const TRASH_FDA_BUTTON: &str = "Open Full Disk Access Settings";

/// The program the login item starts: the installed app's agent (a fixed path on the fake
/// Mac of the tests).
fn agent_program(env: &cleancraft_core::Env) -> std::path::PathBuf {
    if env.is_fake() {
        return env.sys_path("/Applications/CleanCraft.app/Contents/MacOS/CleanCraft");
    }
    match std::env::current_exe() {
        Ok(exe) => login_agent::agent_program(&exe),
        Err(_) => env.sys_path("/Applications/CleanCraft.app/Contents/MacOS/CleanCraft"),
    }
}

/// Turn the menu bar agent on or off (login item and launchctl), with Undo in the notice.
fn toggle_menu_bar(app: &mut CleanApp) {
    let on = login_agent::is_on(&app.env);
    let res = if on {
        login_agent::turn_off(&app.env)
    } else {
        login_agent::turn_on(&app.env, &agent_program(&app.env))
    };
    match res {
        Ok(ch) => {
            let id = ch.record.id.clone();
            let msg = match (&ch.note, on) {
                (Some(n), _) => format!("CleanCraft is set to show in the menu bar. {n}"),
                (None, false) => {
                    "CleanCraft is in the menu bar now and starts there at login.".to_string()
                }
                (None, true) => {
                    "CleanCraft left the menu bar. Its login item is in the Trash.".to_string()
                }
            };
            app.tell_undo(
                msg,
                "Menu Bar Change",
                move |app| match cleancraft_core::executor::undo(&app.env, &id) {
                    Ok(_) => app.tell("The menu bar change was undone."),
                    Err(e) => app.warn(format!("Could not undo: {e}")),
                },
            );
        }
        Err(e) => app.warn(e),
    }
}

/// Menu bar, scheduled scans and alerts. Returns true when a setting changed.
fn menu_bar_card(app: &mut CleanApp, ui: &mut Ui) -> bool {
    let t = app.t;
    let mut changed = false;
    kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        heading(ui, &t, Icon::Monitor, "Menu bar and notifications");
        let on = login_agent::is_on(&app.env);
        if switch_row(ui, &t, on, MENU_BAR_SWITCH, "Menu bar") {
            app.act();
            toggle_menu_bar(app);
        }
        note(ui, &t, if on {
            "A small CleanCraft icon in the menu bar, from login on. Click it for a live panel with CPU, memory, free space, battery, network and the busiest apps. It only looks: it scans, warns and opens CleanCraft, and never cleans by itself."
        } else {
            "Off: nothing of CleanCraft runs while the app is closed. The options below need the menu bar."
        });
        ui.add_enabled_ui(on, |ui| {
            // Scheduled scans.
            ui.horizontal(|ui| {
                label_cell(ui, &t, "Scan automatically");
                for every in ScanEvery::ALL {
                    if kit::radio(ui, &t, app.settings.scan_every == every, every.label()).clicked()
                        && app.settings.scan_every != every
                    {
                        app.settings.scan_every = every;
                        changed = true;
                    }
                }
            });
            if app.settings.scan_every != ScanEvery::Off {
                let mut h = u32::from(app.settings.scan_hour.min(23));
                if slider_row_with(ui, &t, "Scan time", "At", &mut h, 0..=23, &|v| {
                    schedule::hour_text(v as u8)
                }) {
                    app.settings.scan_hour = h as u8;
                    changed = true;
                }
                if app.settings.scan_every == ScanEvery::Weekly {
                    ui.horizontal_wrapped(|ui| {
                        label_cell(ui, &t, "On");
                        for d in 1..=7u8 {
                            let name = schedule::weekday_name(d);
                            if kit::radio(ui, &t, app.settings.scan_weekday == d, name).clicked()
                                && app.settings.scan_weekday != d
                            {
                                app.settings.scan_weekday = d;
                                changed = true;
                            }
                        }
                    });
                }
                let next = schedule::next_text(&app.settings, schedule::local_now())
                    .unwrap_or_default();
                note(ui, &t, &format!(
                    "{next}. A scheduled scan only looks: nothing moves until you review it in Quick Sweep and confirm."
                ));
            }
            // Disk alert. While the menu bar is off the switches below show Off (nothing would
            // run); the saved choices stay and show again when it comes on (U7-15).
            let alert = app.settings.disk_low_percent > 0;
            if switch_row(ui, &t, on && alert, DISK_ALERT_SWITCH, "Disk alert") {
                app.settings.disk_low_percent = if alert {
                    0
                } else {
                    cleancraft_core::settings::DEFAULT_DISK_LOW_PERCENT
                };
                changed = true;
            }
            if on && alert {
                let mut p = u32::from(app.settings.disk_low_percent.clamp(1, 50));
                if slider_row_with(ui, &t, "Disk alert threshold (percent free)", "When free space is under", &mut p, 2..=30, &|v| {
                    format!("{v}%")
                }) {
                    app.settings.disk_low_percent = p as u8;
                    changed = true;
                }
                note(ui, &t, "At most one alert a day.");
            }
            if switch_row(ui, &t, on && app.settings.memory_alert, MEMORY_ALERT_SWITCH, "Memory alert") {
                app.settings.memory_alert = !app.settings.memory_alert;
                changed = true;
            }
            note(ui, &t, "After 5 minutes of high memory pressure, at most once a day. It opens Live Monitor.");
            if switch_row(ui, &t, on && app.settings.trash_alert, TRASH_ALERT_SWITCH, "Trash alert") {
                app.settings.trash_alert = !app.settings.trash_alert;
                changed = true;
            }
            note(ui, &t, "At most once a week. It opens Trash Bins, which only looks until you confirm.");
            // watch6's watchers, run by the agent.
            if switch_row(ui, &t, on && app.settings.watch_trash_offer, TRASH_OFFER_SWITCH, "App in the Trash") {
                app.settings.watch_trash_offer = !app.settings.watch_trash_offer;
                changed = true;
            }
            note(ui, &t, "When you drag an app to the Trash, CleanCraft offers to show the files it left behind. Nothing moves until you confirm the list.");
            // The Trash is private without Full Disk Access (U7-2).
            ui.horizontal_wrapped(|ui| {
                ui.add_space(272.0);
                let missing = app.fda == cleancraft_core::permissions::Access::Missing;
                ui.label(
                    crate::rich(if missing {
                        TRASH_FDA_NOTE.to_string()
                    } else {
                        format!("{TRASH_FDA_NOTE} It is on.")
                    })
                    .font(font(size::FOOTNOTE, Weight::Regular))
                    .color(if missing { t.warning } else { t.text_faint }),
                );
                if missing && kit::ghost(ui, &t, TRASH_FDA_BUTTON, None).clicked() {
                    app.act();
                    if let Err(e) = cleancraft_core::permissions::open_fda_settings(&app.env) {
                        app.warn(format!("Could not open System Settings: {e}"));
                    }
                }
            });
            if switch_row(ui, &t, on && app.settings.watch_new_items, NEW_ITEMS_SWITCH, "New items") {
                app.settings.watch_new_items = !app.settings.watch_new_items;
                changed = true;
            }
            note(ui, &t, "New apps and new startup items are checked with Threat Check's rules; a finding opens Threat Check.");
            if switch_row(ui, &t, on && app.settings.notifications_paused, PAUSE_SWITCH, "Pause") {
                app.settings.notifications_paused = !app.settings.notifications_paused;
                changed = true;
            }
            note(ui, &t, NOTIFY_NOTE);
        });
    });
    changed
}

/// The first choice of the language card: follow the Mac's preferred languages.
pub const LANGUAGE_SYSTEM: &str = "Same as the Mac";

/// Language: the Mac's, or one CleanCraft speaks. Applies at once (the menu bar follows
/// within seconds).
fn language_card(app: &mut CleanApp, ui: &mut Ui) -> bool {
    let t = app.t;
    let mut changed = false;
    kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        heading(ui, &t, Icon::Settings, "Language");
        ui.horizontal(|ui| {
            label_cell(ui, &t, "Show CleanCraft in");
            let choices = std::iter::once(("", LANGUAGE_SYSTEM))
                .chain(cleancraft_core::i18n::LANGUAGES.iter().copied());
            for (code, name) in choices {
                if kit::radio(ui, &t, app.settings.language == code, name).clicked()
                    && app.settings.language != code
                {
                    app.settings.language = code.to_string();
                    cleancraft_core::i18n::apply(&app.env, &app.settings);
                    changed = true;
                }
            }
        });
        note(ui, &t, "Texts without a translation yet stay in English.");
    });
    changed
}

/// The label column of a form row (260 pt, right-aligned), like [`switch_row`]'s.
pub(crate) fn label_cell(ui: &mut Ui, t: &crate::theme::Tokens, label: &str) {
    let (r, _) = ui.allocate_exact_size(vec2(260.0, crate::theme::hit::FIELD), Sense::hover());
    ui.painter().text(
        r.right_center() - vec2(12.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        crate::tr(label),
        font(size::BODY, Weight::Regular),
        t.text_dim,
    );
}

/// A small dim line under a row, in the value column.
pub(crate) fn note(ui: &mut Ui, t: &crate::theme::Tokens, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(272.0);
        ui.add(
            egui::Label::new(
                crate::rich(text)
                    .font(font(size::FOOTNOTE, Weight::Regular))
                    .color(t.text_faint),
            )
            .wrap(),
        );
    });
}

/// Requests to open the app at a page (from the menu bar agent, `cleancraft open` or a
/// notification's menu item): read twice a second, each handled once, then the window comes
/// to the front. Called from [`CleanApp::logic`].
pub fn follow_open_requests(app: &mut CleanApp, ctx: &egui::Context) {
    let now = ctx.input(|i| i.time);
    if app.settings_page.inbox_at.is_some_and(|at| now - at < 0.5) {
        return;
    }
    app.settings_page.inbox_at = Some(now);
    let env = app.env.clone();
    if let Some(route) = app
        .settings_page
        .inbox
        .poll(&env, chrono::Utc::now().timestamp())
    {
        open_route(app, &route);
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
}

/// Go where a route says, through the app's one router ([`CleanApp::open_route`]):
/// "scan:quick_sweep" opens Quick Sweep and starts its scan, "uninstall:<app in the Trash>"
/// opens App Remover on what it left, "threat:<path>" opens Threat Check with that finding
/// selected, "full_disk_access" the Full Disk Access group. Empty or unknown: the page stays.
pub fn open_route(app: &mut CleanApp, route: &str) {
    if let Some(r) = cleancraft_core::route::AppRoute::parse(route) {
        app.open_route(&r);
    }
}
