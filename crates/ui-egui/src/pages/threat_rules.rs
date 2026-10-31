//! What Threat Check recognizes threats with: Apple's malware rules (their version and age,
//! and a button asking macOS for newer ones) and the opt-in abuse.ch sample list (the key in
//! Settings, the status line on both pages).

use cleancraft_core::{known_malware, xprotect};
use egui::Ui;

use crate::app::CleanApp;
use crate::jobs::Job;
use crate::kit;

pub const UPDATE_APPLE: &str = "Update Apple's Rules...";
pub const SAVE_KEY: &str = "Save Key";
pub const FORGET_KEY: &str = "Forget Key";
pub const UPDATE_LIST: &str = "Download Now";
pub const KEY_HINT: &str = "abuse.ch key";

#[derive(Default)]
pub struct State {
    /// The key being typed in Settings.
    pub key: String,
    /// An update running: Apple's rules or the abuse.ch list. `Ok` and `Err` are shown as is.
    job: Option<Job<Result<String, String>>>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Finish the running update and say how it went.
fn poll(app: &mut CleanApp, ui: &Ui) {
    let Some(job) = app.threat_rules.job.as_mut() else {
        return;
    };
    if let Some(out) = job.poll() {
        app.threat_rules.job = None;
        match out {
            Ok(s) => app.tell(s),
            Err(e) => app.warn(e),
        }
    } else if job.is_dead() {
        app.threat_rules.job = None;
        app.warn("The update stopped unexpectedly.");
    } else {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(300));
    }
}

fn start(
    app: &mut CleanApp,
    work: impl FnOnce(&cleancraft_core::Env) -> Result<String, String> + Send + 'static,
) {
    app.act();
    let env = app.env.clone();
    app.threat_rules.job = Some(Job::start(app.sync, move |_| work(&env)));
}

fn update_list(env: &cleancraft_core::Env) -> Result<String, String> {
    known_malware::update(env, now()).map(|n| match n {
        1 => "1 new known malware sample from abuse.ch.".to_string(),
        n => format!("{n} new known malware samples from abuse.ch."),
    })
}

/// Apple's rules line with its button, and the abuse.ch line (Threat Check and Settings).
fn rules_lines(app: &mut CleanApp, ui: &mut Ui, in_settings: bool) {
    let t = app.t;
    poll(app, ui);
    let busy = app.threat_rules.job.is_some();
    let wall = std::time::SystemTime::now();
    match xprotect::version(&app.env) {
        Some(v) => {
            let old = v.is_old(wall);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    crate::rich(v.text(wall))
                        .font(crate::theme::font(
                            crate::theme::size::FOOTNOTE,
                            crate::theme::Weight::Regular,
                        ))
                        .color(if old { t.warning } else { t.text_faint }),
                );
                let go = ui
                    .add_enabled_ui(!busy, |ui| kit::ghost(ui, &t, UPDATE_APPLE, None))
                    .inner
                    .on_hover_text(crate::tr(
                        "Ask macOS for Apple's newest malware rules (asks for the administrator password)",
                    ))
                    .clicked();
                if go && !busy {
                    start(app, xprotect::update);
                }
            });
        }
        None => kit::dim(
            ui,
            &t,
            "Apple's malware rules (XProtect) were not found on this Mac.",
        ),
    }
    match known_malware::status(&app.env, now()) {
        Some(s) => kit::dim(ui, &t, &s),
        None if !in_settings => kit::dim(
            ui,
            &t,
            "Add a free abuse.ch key in Settings to also compare files with known Mac malware samples.",
        ),
        None => {}
    }
}

/// Threat Check's start screen, under "How deep".
pub fn lines(app: &mut CleanApp, ui: &mut Ui) {
    rules_lines(app, ui, false);
}

/// The Settings group: the abuse.ch key and both status lines.
pub fn card(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        super::settings::heading(ui, &t, crate::icons::Icon::Shield, "Malware rules");
        kit::subtitle(
            ui,
            &t,
            "Besides Apple's rules, Threat Check can compare files with the Mac malware samples abuse.ch MalwareBazaar collects. abuse.ch asks each user for a free key. CleanCraft downloads the list once a day and never uploads anything.",
        );
        ui.add_space(crate::theme::space::XS);
        let key = known_malware::key(&app.env);
        let busy = app.threat_rules.job.is_some();
        ui.horizontal_wrapped(|ui| {
            let r = kit::text_field(ui, &t, &mut app.threat_rules.key, KEY_HINT, 300.0);
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, crate::tr(KEY_HINT))
            });
            if r.changed() {
                app.act();
            }
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let typed = !app.threat_rules.key.trim().is_empty();
            let save = ui
                .add_enabled_ui(typed, |ui| kit::secondary(ui, &t, SAVE_KEY))
                .inner;
            if (save || enter) && typed {
                app.act();
                match known_malware::set_key(&app.env, &app.threat_rules.key) {
                    Ok(()) => {
                        app.threat_rules.key.clear();
                        // Fetch now rather than at the agent's next round.
                        start(app, update_list);
                    }
                    Err(e) => app.warn(e),
                }
            }
            if kit::link(ui, &t, "Get a free key at auth.abuse.ch").clicked() {
                app.act();
                if let Err(e) = app.env.sys.open_url("https://auth.abuse.ch/") {
                    app.warn(format!("The page could not be opened: {e}"));
                }
            }
        });
        if let Some(k) = key {
            ui.horizontal_wrapped(|ui| {
                kit::dim(ui, &t, &known_malware::key_hint(&k));
                let go = ui
                    .add_enabled_ui(!busy, |ui| kit::ghost(ui, &t, UPDATE_LIST, None))
                    .inner
                    .clicked();
                if go && !busy {
                    start(app, update_list);
                }
                if kit::ghost(ui, &t, FORGET_KEY, Some(crate::icons::Icon::Close)).clicked() {
                    app.act();
                    match known_malware::forget_key(&app.env) {
                        Ok(()) => app.tell("The abuse.ch key is forgotten. The samples already downloaded stay."),
                        Err(e) => app.warn(e),
                    }
                }
            });
        }
        ui.add_space(crate::theme::space::XS);
        rules_lines(app, ui, true);
    });
}
