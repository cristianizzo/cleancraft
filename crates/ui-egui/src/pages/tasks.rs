//! Tune-up Tasks: a checklist of maintenance commands. Each shows what it does, the exact
//! command, whether macOS will ask for the administrator password and when it last ran.
//! "Run Selected" opens a confirmation with the exact commands; the results show per task,
//! with the output log and, for purgeable space, free space before and after.

use std::collections::{BTreeMap, BTreeSet};

use cleancraft_core::fsutil::human_size;
use cleancraft_core::tasks::{self, Outcome, Prepared, RunResult, TASKS};
use cleancraft_core::{Env, FeatureId};
use egui::Ui;

use crate::app::CleanApp;
use crate::jobs::{Job, Progress};
use crate::kit;
use crate::theme::{Weight, font, size};

/// What the availability check found for one task.
pub type Ready = Result<Prepared, String>;

#[derive(Default)]
pub struct State {
    pub loaded: bool,
    loading: Option<Job<BTreeMap<String, Ready>>>,
    /// Availability per task id.
    pub ready: BTreeMap<String, Ready>,
    pub selected: BTreeSet<String>,
    /// Last run per task, from the journal.
    pub last: BTreeMap<String, (i64, bool)>,
    /// The confirmation: what will run, and the tasks that cannot run now with why.
    pub confirm: Option<(Vec<Prepared>, Vec<(String, String)>)>,
    running: Option<Job<Vec<RunResult>>>,
    pub results: BTreeMap<String, RunResult>,
    pub show_log: BTreeSet<String>,
    /// A task to tick and confirm once the list is checked (route `task:<id>`).
    pub ask: Option<String>,
}

impl State {
    pub fn is_busy(&self) -> bool {
        self.loading.is_some() || self.running.is_some()
    }
}

fn check_all(env: &Env, meter: &Progress) -> BTreeMap<String, Ready> {
    TASKS
        .iter()
        .enumerate()
        .map(|(k, t)| {
            meter.set(k as u64, TASKS.len() as u64);
            meter.set_step(t.title);
            (t.id.to_string(), tasks::prepare(env, t.id))
        })
        .collect()
}

fn start_loading(app: &mut CleanApp) {
    let env = app.env.clone();
    app.tasks.loading = Some(Job::start_metered(app.sync, move |m| check_all(&env, m)));
    app.tasks.last = tasks::last_runs(&app.env);
}

/// Finish background work. Returns true while something runs.
pub fn poll(app: &mut CleanApp) -> bool {
    let mut busy = false;
    if let Some(job) = app.tasks.loading.as_mut() {
        if let Some(r) = job.poll() {
            let st = &mut app.tasks;
            st.selected
                .retain(|id| r.get(id).is_some_and(|x| x.is_ok()));
            st.ready = r;
            st.loaded = true;
            st.loading = None;
        } else if job.is_dead() {
            app.tasks.loading = None;
            app.tasks.loaded = true;
        } else {
            busy = true;
        }
    }
    if let Some(job) = app.tasks.running.as_mut() {
        if let Some(results) = job.poll() {
            app.tasks.running = None;
            let cancelled = results
                .iter()
                .filter(|r| r.outcome == Outcome::Cancelled)
                .count();
            let failed = results
                .iter()
                .filter(|r| r.outcome == Outcome::Failed)
                .count();
            let done = results.len() - cancelled - failed;
            for r in results {
                app.tasks.results.insert(r.id.clone(), r);
            }
            let msg = format!("{done} done, {failed} failed, {cancelled} cancelled.");
            if failed > 0 {
                app.warn(msg);
            } else {
                app.tell(msg);
                crate::sound::play(&app.env, crate::sound::Cue::CleanDone);
            }
            start_loading(app);
            busy = true;
        } else if job.is_dead() {
            app.tasks.running = None;
            app.warn("The tasks stopped unexpectedly.");
        } else {
            busy = true;
        }
    }
    busy
}

pub fn ui(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    let t = app.t;
    let (icon, _) = kit::feature_look(&t, f);
    kit::header(
        ui,
        &t,
        icon,
        f.title(),
        "Housekeeping commands that come with macOS. You see each exact command before it runs; commands cannot be undone, and History records them.",
    );
    if !app.tasks.loaded && app.tasks.loading.is_none() {
        start_loading(app);
    }
    if let Some(job) = &app.tasks.running {
        let status = job.status();
        kit::progress(
            ui,
            &t,
            f,
            "Running the tasks",
            &status,
            &job.meter.clone(),
            false,
        );
        return;
    }
    if !app.tasks.loaded {
        let meter = app
            .tasks
            .loading
            .as_ref()
            .map(|j| j.meter.clone())
            .unwrap_or_default();
        kit::progress(
            ui,
            &t,
            f,
            "Checking which tasks can run on this Mac...",
            "",
            &meter,
            false,
        );
        return;
    }
    if let Some(id) = app.tasks.ask.take() {
        app.tasks.selected = BTreeSet::from([id]);
        open_confirm(app);
    }
    let n = app.tasks.selected.len();
    // The actions stay in reach at the bottom, like every other page's.
    kit::bottom_bar(ui, &t, egui::Id::new("tasks-bottom"), |ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.add_space(2.0);
            ui.label(
                crate::rich(match n {
                    0 => "Nothing selected".to_string(),
                    1 => "1 task selected".to_string(),
                    n => format!("{n} tasks selected"),
                })
                .font(font(size::BODY, Weight::SemiBold))
                .color(if n > 0 { t.text } else { t.text_dim }),
            );
            kit::dim(
                ui,
                &t,
                if n > 0 {
                    "You see the exact commands before anything runs."
                } else {
                    "Tick the tasks you want to run."
                },
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let run = ui
                .add_enabled_ui(n > 0, |ui| {
                    kit::pill(
                        ui,
                        &t,
                        "Run Selected",
                        if n > 0 {
                            kit::Kind::Primary
                        } else {
                            kit::Kind::Secondary
                        },
                        None,
                    )
                })
                .inner
                .on_disabled_hover_text(crate::tr("Tick a task first."));
            if run.clicked() && n > 0 {
                app.act();
                open_confirm(app);
            }
            if kit::pill(ui, &t, "Scan Again", kit::Kind::Secondary, None)
                .on_hover_text(crate::tr("Check again which tasks can run on this Mac"))
                .clicked()
            {
                app.act();
                start_loading(app);
            }
            let ready_ids: Vec<&str> = TASKS
                .iter()
                .filter(|info| matches!(app.tasks.ready.get(info.id), Some(Ok(_))))
                .map(|info| info.id)
                .collect();
            let all_selected = !ready_ids.is_empty()
                && ready_ids.iter().all(|id| app.tasks.selected.contains(*id));
            if !ready_ids.is_empty() {
                if all_selected {
                    if kit::secondary(ui, &t, "Select None") {
                        app.act();
                        app.tasks.selected.clear();
                    }
                } else if kit::secondary(ui, &t, "Select All") {
                    app.act();
                    for id in ready_ids {
                        app.tasks.selected.insert(id.to_string());
                    }
                }
            }
        });
    });
    ui.add_space(6.0);
    let fade_out = egui::ScrollArea::vertical()
        .id_salt("tasks")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for info in TASKS {
                task_card(app, ui, info);
                ui.add_space(crate::theme::space::S);
            }
        });
    kit::scroll_fade_page(ui, &fade_out, &t);
    if app.tasks.confirm.is_some() {
        confirm(app, ui);
    }
}

fn task_card(app: &mut CleanApp, ui: &mut Ui, info: &tasks::TaskInfo) {
    let t = app.t;
    let ready = app.tasks.ready.get(info.id).cloned();
    kit::card(&t)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            // Line 1: the checkbox with the title, the tags, and when it last ran.
            ui.horizontal(|ui| {
                let can = matches!(ready, Some(Ok(_)));
                let mut on = app.tasks.selected.contains(info.id);
                let cb = ui
                    .add_enabled_ui(can, |ui| {
                        kit::check_text_font(
                            ui,
                            &t,
                            &mut on,
                            info.title,
                            font(size::BODY, Weight::SemiBold),
                            true,
                        )
                    })
                    .inner;
                if cb.changed() {
                    app.act();
                    if on {
                        app.tasks.selected.insert(info.id.to_string());
                    } else {
                        app.tasks.selected.remove(info.id);
                    }
                }
                if info.admin {
                    kit::tag(ui, "Administrator password", t.warning);
                }
                kit::tag(
                    ui,
                    &format!("{} risk", info.risk.label()),
                    kit::risk_color(&t, info.risk),
                );
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| match app.tasks.last.get(info.id) {
                        Some((ts, ok)) => kit::dim(
                            ui,
                            &t,
                            &format!(
                                "Last run {} ({})",
                                tasks::local_time(*ts),
                                if *ok { "done" } else { "failed" }
                            ),
                        ),
                        None => kit::dim(ui, &t, "Never run by CleanCraft"),
                    },
                );
            });
            let command = match &ready {
                Some(Ok(p)) => p.commands().join("\n"),
                _ => info.command.to_string(),
            };
            let wide = ui.available_width() >= 720.0;
            let describe = |ui: &mut Ui| {
                ui.add(
                    egui::Label::new(
                        crate::rich(info.description)
                            .font(font(size::CALLOUT, Weight::Regular))
                            .color(t.text_dim),
                    )
                    .wrap(),
                );
            };
            let command_well = |ui: &mut Ui| {
                kit::well(&t)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(
                            egui::Label::new(
                                crate::rich(&command)
                                    .font(crate::theme::mono(size::FOOTNOTE))
                                    .color(t.text),
                            )
                            .wrap(),
                        );
                    });
            };
            // Line 2: what it does, and the exact command beside it when there is room.
            ui.horizontal_top(|ui| {
                ui.add_space(28.0);
                let w = ui.available_width();
                if wide {
                    let left = (w * 0.55).min(crate::theme::space::MEASURE);
                    ui.allocate_ui_with_layout(
                        egui::vec2(left, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(left);
                            describe(ui);
                        },
                    );
                    ui.add_space(crate::theme::space::M);
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| command_well(ui),
                    );
                } else {
                    ui.vertical(|ui| {
                        describe(ui);
                        command_well(ui);
                    });
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(28.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    if let Some(Err(why)) = &ready {
                        // A plain sentence; the tool's own words in the tooltip (REVIEW-C D4).
                        let r = ui.label(
                            crate::rich(plain_reason(why))
                                .font(font(size::FOOTNOTE, Weight::Medium))
                                .color(t.warning),
                        );
                        if plain_reason(why) != *why {
                            r.on_hover_text(crate::tr(why));
                        }
                    }
                    // Same rule as Quick Sweep's tune-up count.
                    if let Some(why) = tasks::suggestion(
                        info.id,
                        &app.tasks.last,
                        matches!(ready, Some(Ok(_))),
                        chrono::Utc::now().timestamp(),
                    ) {
                        ui.label(
                            crate::rich(why)
                                .font(font(size::FOOTNOTE, Weight::Medium))
                                .color(t.sec_a),
                        );
                    }
                    if let Some(r) = app.tasks.results.get(info.id).cloned() {
                        task_result(app, ui, info, &r);
                    }
                });
            });
        });
}

/// The result of the last run on this screen: the message, before and after, the log.
fn task_result(app: &mut CleanApp, ui: &mut Ui, info: &tasks::TaskInfo, r: &RunResult) {
    let t = app.t;
    let color = match r.outcome {
        Outcome::Done => t.success,
        Outcome::Failed => t.danger,
        Outcome::Cancelled => t.warning,
    };
    ui.label(
        crate::rich(&r.message)
            .font(font(size::CALLOUT, Weight::SemiBold))
            .color(color),
    );
    if let (Some(b), Some(a)) = (r.free_before, r.free_after) {
        kit::dim(
            ui,
            &t,
            &format!(
                "Free space before {}, after {}",
                human_size(b),
                human_size(a)
            ),
        );
    }
    if let (Some(b), Some(a)) = (r.file_before, r.file_after) {
        kit::dim(
            ui,
            &t,
            &format!(
                "Mail index before {}, after {}",
                human_size(b),
                human_size(a)
            ),
        );
    }
    if !r.output.is_empty() {
        let open = app.tasks.show_log.contains(info.id);
        let label = if open {
            format!("Hide log of {}", info.title)
        } else {
            format!("Show log of {}", info.title)
        };
        if kit::ghost(ui, &t, &label, None).clicked() {
            app.act();
            if open {
                app.tasks.show_log.remove(info.id);
            } else {
                app.tasks.show_log.insert(info.id.to_string());
            }
        }
        if open {
            kit::well(&t).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    crate::rich(&r.output)
                        .font(font(size::FOOTNOTE, Weight::Medium))
                        .color(t.text_dim),
                );
            });
        }
    }
}

fn open_confirm(app: &mut CleanApp) {
    let mut ok = Vec::new();
    let mut not = Vec::new();
    for info in TASKS {
        if !app.tasks.selected.contains(info.id) {
            continue;
        }
        // Checked again now: the Mac may have changed since the list was drawn.
        match tasks::prepare(&app.env, info.id) {
            Ok(p) => ok.push(p),
            Err(e) => not.push((info.title.to_string(), e)),
        }
    }
    app.tasks.confirm = Some((ok, not));
}

fn confirm(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let Some((ok, not)) = app.tasks.confirm.clone() else {
        return;
    };
    let mut go = false;
    let mut close = false;
    let max_list = (ui.ctx().content_rect().height() - 300.0).clamp(120.0, 380.0);
    let dlg = kit::modal(egui::Id::new("tasks-confirm"), &t).show(ui.ctx(), |ui| {
        ui.set_width((ui.ctx().content_rect().width() - 120.0).clamp(420.0, 640.0));
        kit::dialog_head(
            ui,
            &t,
            crate::icons::Icon::Tasks,
            t.sec_a,
            &match ok.len() {
                1 => "Run 1 task?".to_string(),
                n => format!("Run {n} tasks?"),
            },
            "These exact commands will run, in this order. They cannot be undone.",
        );
        ui.add_space(12.0);
        egui::ScrollArea::vertical()
            .max_height(max_list)
            .show(ui, |ui| {
                for p in &ok {
                    let Some(info) = tasks::info(&p.id) else {
                        continue;
                    };
                    ui.add_space(4.0);
                    ui.label(crate::rich(info.title).font(font(size::BODY, Weight::SemiBold)));
                    ui.label(crate::rich(info.description).color(t.text_dim));
                    for c in p.commands() {
                        ui.label(
                            crate::rich(c)
                                .font(crate::theme::mono(size::FOOTNOTE))
                                .color(t.text),
                        );
                    }
                    if p.needs_admin() {
                        ui.label(
                            crate::rich("macOS will ask for the administrator password.")
                                .color(t.warning),
                        );
                    }
                }
                for (title, why) in &not {
                    ui.add_space(4.0);
                    ui.label(crate::rich(format!("{title} will not run: {why}")).color(t.warning));
                }
            });
        ui.add_space(14.0);
        if ok.is_empty() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = kit::secondary(ui, &t, "Cancel");
            });
        } else {
            let (g, c) = kit::dialog_actions(ui, &t, "Run", kit::Kind::Primary);
            go = g;
            close = c;
        }
    });
    if dlg.should_close() {
        close = true;
    }
    if go {
        app.act();
        app.tasks.confirm = None;
        for p in &ok {
            app.tasks.results.remove(&p.id);
        }
        let env = app.env.clone();
        app.tasks.running = Some(Job::start_metered(app.sync, move |meter| {
            ok.iter()
                .enumerate()
                .map(|(k, p)| {
                    meter.set(k as u64, ok.len() as u64);
                    meter.set_step(tasks::info(&p.id).map(|i| i.title).unwrap_or(""));
                    tasks::run(&env, p)
                })
                .collect()
        }));
    } else if close {
        app.act();
        app.tasks.confirm = None;
    }
}

/// The first clause of a task's error, as a sentence: "Could not list local snapshots: fake:
/// no answer for ..." reads "Could not list local snapshots. This task cannot run now."
pub fn plain_reason(why: &str) -> String {
    match why.split_once(": ") {
        Some((head, _)) if !head.is_empty() => {
            format!("{}. This task cannot run now.", head.trim_end_matches('.'))
        }
        _ => super::plan_page::sentence(why),
    }
}
