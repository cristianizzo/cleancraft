//! Mac Health (a tool of the Live Monitor section): the state of the Mac from facts CleanCraft
//! already reads, its recommendations (one action each, opening the right page), the facts
//! themselves, free space over 7, 30 or 90 days, and what grew between two Disk Map scans.
//!
//! No score: the state is "Good" or "Needs attention" by the rules in
//! `cleancraft_core::health::RULES`, listed on the page under "How this is decided".

use std::time::{Duration, Instant};

use cleancraft_core::fsutil::{count, human_size};
use cleancraft_core::health::{self, Card, Facts, Growth, Level, MapSummary, State as Verdict};
use cleancraft_core::monitor::BatteryReading;
use cleancraft_core::space_history::{self, History};
use egui::{Color32, Ui, vec2};

use crate::app::CleanApp;
use crate::icons::{self, Icon};
use crate::jobs::Job;
use crate::kit;
use crate::theme::{Tokens, Weight, font, size, space};

/// Facts older than this are read again when the page opens.
pub const FRESH_FOR: Duration = Duration::from_secs(300);
pub const RANGES: [(&str, i64); 3] = [("7 days", 7), ("30 days", 30), ("90 days", 90)];

pub struct Read {
    pub facts: Facts,
    pub history: History,
    /// The kept measures of folders ("what grew" compares two of them).
    pub maps: Vec<MapSummary>,
}

#[derive(Default)]
pub struct State {
    pub facts: Option<Facts>,
    pub cards: Vec<Card>,
    pub history: History,
    pub maps: Vec<MapSummary>,
    /// What grew over the chart's range (from `maps`).
    pub growth: Option<Growth>,
    job: Option<Job<Read>>,
    pub checked: Option<Instant>,
    /// Index in [`RANGES`].
    pub range: usize,
    pub rules_open: bool,
}

impl State {
    pub fn is_busy(&self) -> bool {
        self.job.is_some()
    }

    /// Show these facts (tests, renders).
    pub fn set(&mut self, r: Read) {
        self.cards = health::recommendations(&r.facts);
        self.facts = Some(r.facts);
        self.history = r.history;
        self.maps = r.maps;
        self.checked = Some(Instant::now());
        self.regrow();
    }

    /// Compare the measures again for the chart's range.
    pub fn regrow(&mut self) {
        let now = self
            .facts
            .as_ref()
            .map(|f| f.now)
            .unwrap_or_else(|| chrono::Utc::now().timestamp());
        self.growth = health::what_grew_in(&self.maps, now, RANGES[self.range].1);
    }
}

/// Read every fact again (in the background).
pub fn refresh(app: &mut CleanApp) {
    if app.health.job.is_some() {
        return;
    }
    let env = app.env.clone();
    let settings = app.settings.clone();
    app.health.job = Some(Job::start_metered(app.sync, move |meter| {
        let now = chrono::Utc::now().timestamp();
        Read {
            facts: health::gather_metered(&env, &settings, now, &|s| meter.set_step(s), meter),
            history: space_history::load(&env).history,
            maps: health::map_summaries(&env),
        }
    }));
}

/// Read again when nothing is shown yet or what is shown is older than [`FRESH_FOR`].
pub fn refresh_if_old(app: &mut CleanApp) {
    if app.health.checked.is_none_or(|c| c.elapsed() >= FRESH_FOR) {
        refresh(app);
    }
}

pub fn poll(app: &mut CleanApp) -> bool {
    let Some(j) = app.health.job.as_mut() else {
        return false;
    };
    if let Some(r) = j.poll() {
        app.health.job = None;
        app.health.set(r);
        false
    } else if j.is_dead() {
        app.health.job = None;
        app.warn("Reading the Mac's facts stopped unexpectedly.");
        false
    } else {
        true
    }
}

fn level_color(t: &Tokens, l: Level) -> Color32 {
    match l {
        Level::Tip => t.sec_a,
        Level::Attention => t.warning,
        Level::Urgent => icons::lerp(t.warning, t.danger, 0.6),
    }
}

pub fn ui(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    if app.health.facts.is_none() {
        if app.health.job.is_none() {
            refresh(app);
        }
        let (status, meter) = app
            .health
            .job
            .as_ref()
            .map(|j| (j.status(), j.meter.clone()))
            .unwrap_or_default();
        kit::progress(
            ui,
            &t,
            cleancraft_core::FeatureId::Monitor,
            "Reading your Mac...",
            &status,
            &meter,
            false,
        );
        return;
    }
    let out = egui::ScrollArea::vertical()
        .id_salt("health")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let w = ui.available_width();
            summary(app, ui);
            ui.add_space(space::M);
            // Two columns when there is room: what to do on the left, the figures on the
            // right, so the chart is on the first screen.
            if w >= 900.0 {
                // Card frames draw their stroke just outside the width they are given.
                let gap = space::GUTTER;
                let left_w = ((w - gap) * 0.56).floor();
                let right_w = w - gap - left_w - 8.0;
                // With few recommendations the facts fill the left column.
                let few = app.health.cards.len() <= 3;
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        vec2(left_w, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(left_w);
                            recs(app, ui);
                            if few {
                                ui.add_space(space::M);
                                facts_card(app, ui);
                            }
                        },
                    );
                    ui.add_space(gap);
                    ui.allocate_ui_with_layout(
                        vec2(right_w, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(right_w);
                            chart_card(app, ui);
                            ui.add_space(space::M);
                            grew_card(app, ui);
                            if !few {
                                ui.add_space(space::M);
                                facts_card(app, ui);
                            }
                        },
                    );
                });
            } else {
                recs(app, ui);
                ui.add_space(space::M);
                chart_card(app, ui);
                ui.add_space(space::M);
                grew_card(app, ui);
                ui.add_space(space::M);
                facts_card(app, ui);
            }
            ui.add_space(space::XL);
        });
    kit::scroll_fade_page(ui, &out, &t);
}

fn summary(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let (verdict, why) = health::summary(&app.health.cards);
    let color = match verdict {
        Verdict::Good => t.success,
        Verdict::NeedsAttention => t.warning,
    };
    kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            kit::badge(
                ui,
                color,
                if verdict == Verdict::Good {
                    Icon::Check
                } else {
                    Icon::Warning
                },
                44.0,
            );
            ui.add_space(space::S);
            let text_w = (ui.available_width() - 170.0).max(200.0);
            ui.allocate_ui_with_layout(
                vec2(text_w, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_max_width(text_w);
                    ui.label(
                        crate::rich(verdict.label())
                            .font(font(size::TITLE, Weight::Bold))
                            .color(t.text),
                    );
                    if why.is_empty() {
                        kit::dim(ui, &t, "No rule that needs attention applies right now.");
                    } else {
                        // A short list of reasons, not one long sentence.
                        ui.spacing_mut().item_spacing.y = 2.0;
                        kit::dim(
                            ui,
                            &t,
                            &format!(
                                "{} need attention:",
                                count(why.len(), "thing", "things")
                            ),
                        );
                        for w in why.iter().take(4) {
                            ui.add(
                                egui::Label::new(
                                    crate::rich(format!("\u{2022}  {w}"))
                                        .font(font(size::CALLOUT, Weight::Regular))
                                        .color(t.text_dim),
                                )
                                .wrap(),
                            );
                        }
                        if why.len() > 4 {
                            kit::dim(
                                ui,
                                &t,
                                &format!(
                                    "and {} more, in Recommendations below.",
                                    why.len() - 4
                                ),
                            );
                        }
                    }
                },
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if app.health.is_busy() {
                    kit::dim(ui, &t, "Checking...");
                } else if kit::secondary(ui, &t, "Check Again") {
                    app.act();
                    refresh(app);
                }
            });
        });
        ui.add_space(space::XS);
        let label = if app.health.rules_open {
            "Hide how this is decided"
        } else {
            "How this is decided"
        };
        if kit::ghost(ui, &t, label, Some(Icon::Chevron)).clicked() {
            app.act();
            app.health.rules_open = !app.health.rules_open;
        }
        if app.health.rules_open {
            ui.add_space(space::XS);
            kit::dim(
                ui,
                &t,
                "There is no score. The state is Needs attention when a rule marked Needs attention or Act soon applies; tips never change it.",
            );
            ui.add_space(space::XS);
            for r in health::RULES {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(118.0, 20.0), egui::Sense::hover());
                    kit::place(ui, rect, |ui| {
                        kit::tag(ui, r.level.label(), level_color(&t, r.level));
                    });
                    ui.add(
                        egui::Label::new(
                            crate::rich(r.text)
                                .font(font(size::CALLOUT, Weight::Regular))
                                .color(t.text_dim),
                        )
                        .wrap(),
                    );
                });
            }
        }
    });
}

/// The recommendations: one card, one row each (most important first).
fn recs(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let cards = app.health.cards.clone();
    kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        kit::card_head(ui, &t, Icon::Check, t.sec_a, "Recommendations");
        ui.add_space(space::S);
        if cards.is_empty() {
            kit::dim(
                ui,
                &t,
                "Nothing to do right now. Mac Health checks again each time you open it.",
            );
            return;
        }
        for (i, c) in cards.iter().enumerate() {
            if i > 0 {
                let y = ui.cursor().min.y + space::XS;
                ui.painter().hline(
                    ui.max_rect().x_range(),
                    y,
                    egui::Stroke::new(1.0, Color32::from_white_alpha(12)),
                );
                ui.add_space(space::S + 1.0);
            }
            rec_row(app, ui, c, i);
        }
    });
}

fn rec_row(app: &mut CleanApp, ui: &mut Ui, c: &Card, i: usize) {
    let t = app.t;
    let color = level_color(&t, c.level);
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(4.0, 36.0), egui::Sense::hover());
        ui.painter().rect_filled(r, 2.0, color);
        ui.add_space(space::S);
        let btn_w = kit::pill_size(ui, &c.action, kit::Kind::Secondary, false).x;
        let text_w = (ui.available_width() - btn_w - space::M).max(160.0);
        ui.allocate_ui_with_layout(
            vec2(text_w, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_max_width(text_w);
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        crate::rich(&c.headline)
                            .font(font(size::BODY, Weight::SemiBold))
                            .color(t.text),
                    );
                    kit::tag(ui, c.level.label(), color);
                });
                ui.add(
                    egui::Label::new(
                        crate::rich(&c.detail)
                            .font(font(size::CALLOUT, Weight::Regular))
                            .color(t.text_dim),
                    )
                    .wrap(),
                );
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.push_id(("rec", i, c.rule), |ui| {
                if kit::secondary(ui, &t, &c.action) {
                    app.act();
                    // Opens the page (a scan from here only looks); nothing runs here.
                    app.open_route(&c.route);
                    if let Some(l) = &c.landing {
                        app.tell(l.clone());
                    }
                }
            });
        });
    });
}

/// Value text for each fact, "Unknown" when it could not be read.
pub fn fact_rows(f: &Facts) -> Vec<(&'static str, String)> {
    let unknown = || cleancraft_core::monitor::UNKNOWN.to_string();
    let now = f.now;
    let note = |n: &Option<health::ScanNote>, what: &dyn Fn(&health::ScanNote) -> String| match n {
        Some(n) => format!("{}, {}", what(n), health::ago(n.at, now)),
        None => "Never".into(),
    };
    vec![
        (
            "Free space",
            match (f.disk_free, f.disk_total) {
                (Some(fr), Some(tot)) if tot > 0 => format!(
                    "{} of {} ({:.0}%)",
                    human_size(fr),
                    human_size(tot),
                    fr as f64 * 100.0 / tot as f64
                ),
                (Some(fr), _) => human_size(fr),
                _ => unknown(),
            },
        ),
        (
            "Change in 7 days",
            match f.free_change {
                Some((d, _)) if d < 0 => format!("{} less free", human_size(d.unsigned_abs())),
                Some((d, _)) if d > 0 => format!("{} more free", human_size(d as u64)),
                Some(_) => "No change".into(),
                None => "Not enough readings yet".into(),
            },
        ),
        ("Trash", f.trash.map(human_size).unwrap_or_else(unknown)),
        (
            "Last junk scan",
            note(&f.junk, &|n| format!("{} found", human_size(n.bytes))),
        ),
        (
            "Last Threat Check",
            note(&f.threats, &|n| {
                if n.items == 0 {
                    "Nothing found".into()
                } else {
                    format!("{} found", count(n.items, "item", "items"))
                }
            }),
        ),
        (
            "Updates",
            note(&f.updates, &|n| {
                if n.items == 0 {
                    "Up to date".into()
                } else {
                    format!("{} available", count(n.items, "update", "updates"))
                }
            }),
        ),
        (
            "Apps not opened for 6 months",
            match f.unused_apps {
                Some(n) => format!("{n} of {} with a known date", f.apps_dated),
                None => unknown(),
            },
        ),
        (
            "Startup items turned on",
            match &f.startup {
                Some(s) if s.weak.is_empty() => format!("{}, all signed", s.enabled),
                Some(s) => format!(
                    "{}, {} not signed by a known developer",
                    s.enabled,
                    s.weak.len()
                ),
                None => unknown(),
            },
        ),
        (
            "Battery",
            match &f.battery {
                Some(BatteryReading::None) => "No battery (desktop Mac)".into(),
                Some(BatteryReading::Present(b)) => match (b.health, b.cycles) {
                    (Some(h), Some(c)) => format!("{h}% of design capacity, {c} cycles"),
                    (Some(h), None) => format!("{h}% of design capacity"),
                    (None, Some(c)) => format!("{c} cycles"),
                    _ => unknown(),
                },
                None => unknown(),
            },
        ),
        (
            "Memory pressure",
            match &f.memory {
                Some(m) => format!(
                    "{}, {:.0}% in use",
                    m.pressure.map(|p| p.label()).unwrap_or("Unknown"),
                    m.used_fraction * 100.0
                ),
                None => unknown(),
            },
        ),
        (
            "macOS",
            f.macos
                .as_ref()
                .map(|v| format!("macOS {v}"))
                .unwrap_or_else(unknown),
        ),
    ]
}

fn facts_card(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let Some(f) = app.health.facts.clone() else {
        return;
    };
    kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        kit::card_head(ui, &t, Icon::Monitor, t.sec_a, "Facts");
        ui.add_space(space::S);
        let w = ui.available_width();
        for (k, v) in fact_rows(&f) {
            ui.horizontal(|ui| {
                ui.set_min_height(24.0);
                let lw = (w * 0.45).min(230.0);
                ui.allocate_ui_with_layout(
                    vec2(lw, 20.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.set_width(lw);
                        ui.label(
                            crate::rich(k)
                                .font(font(size::CALLOUT, Weight::Regular))
                                .color(t.text_dim),
                        );
                    },
                );
                ui.add(
                    egui::Label::new(
                        crate::rich(v)
                            .font(font(size::CALLOUT, Weight::Medium))
                            .color(t.text),
                    )
                    .wrap(),
                );
            });
        }
        if !f.problems.is_empty() {
            ui.add_space(space::XS);
            kit::dim(ui, &t, &format!("Not read: {}", f.problems.join("; ")));
        }
    });
}

/// Free space samples of the chosen range, as chart points.
pub fn chart_points(h: &History, now: i64, days: i64) -> Vec<(i64, f64)> {
    space_history::window(h, now, days)
        .iter()
        .map(|s| (s.at, s.free as f64))
        .collect()
}

fn chart_card(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    let now = app
        .health
        .facts
        .as_ref()
        .map(|f| f.now)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            kit::card_head(ui, &t, Icon::DiskMap, t.sec_a, "Free space");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let names: Vec<&str> = RANGES.iter().map(|r| r.0).collect();
                if let Some(i) = kit::choice_pills(ui, &t, &names, app.health.range) {
                    app.act();
                    app.health.range = i;
                    app.health.regrow();
                }
            });
        });
        ui.add_space(space::S);
        let days = RANGES[app.health.range].1;
        let pts = chart_points(&app.health.history, now, days);
        let total = app
            .health
            .facts
            .as_ref()
            .and_then(|f| f.disk_total)
            .or_else(|| app.health.history.samples.last().and_then(|s| s.total));
        // One line for "low": the disk alert's threshold from Settings (10% when the alert
        // is off), the same value the menu bar warns at (U7-16).
        let pct = match app.settings.disk_low_percent {
            0 => (health::URGENT_FREE_FRACTION * 100.0).round() as u8,
            p => p,
        };
        let guide_label = format!("Low ({pct}% free)");
        let guide_v = total.map(|tot| tot as f64 * f64::from(pct) / 100.0);
        let guide = guide_v.map(|v| kit::ChartGuide {
            value: v,
            label: &guide_label,
            color: t.warning,
        });
        let w = ui.available_width();
        kit::time_chart(
            ui,
            &t,
            vec2(w, 190.0),
            &pts,
            (now - days * 86_400, now),
            t.sec_a,
            guide,
            &|v| {
                if v <= 0.0 {
                    "0".to_string()
                } else {
                    human_size(v as u64)
                }
            },
            &date,
        );
        ui.add_space(space::XS);
        kit::dim(
            ui,
            &t,
            &format!(
                "{} in the last {days} days. One reading per hour at most, kept 90 days.",
                count(pts.len(), "reading", "readings")
            ),
        );
    });
}

fn grew_card(app: &mut CleanApp, ui: &mut Ui) {
    let t = app.t;
    kit::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        kit::card_head(ui, &t, Icon::Folder, t.sec_a, "What grew");
        ui.add_space(space::S);
        let (measures, first) = health::measures(&app.health.maps);
        match app.health.growth.clone() {
            None => {
                let line = match (measures, first) {
                    (0, _) => "CleanCraft measures your home folder once a day while it runs. The first measure is not done yet.".to_string(),
                    (_, Some(at)) if health::ago(at, chrono::Utc::now().timestamp()) == "today" => {
                        "First measure taken today. Come back tomorrow to see what grew.".to_string()
                    }
                    (_, Some(at)) => format!(
                        "First measure taken {}. The next one comes with tomorrow's launch or scheduled scan.",
                        date(at)
                    ),
                    _ => String::new(),
                };
                kit::dim(ui, &t, &line);
                ui.add_space(space::XS);
                if kit::secondary(ui, &t, "Open Disk Map") {
                    app.act();
                    app.open_route(&cleancraft_core::route::AppRoute::Scan(
                        cleancraft_core::FeatureId::SpaceLens,
                    ));
                }
            }
            Some(g) => {
                let span = format!(
                    "{}: measures of {} and {} compared.",
                    app.env.display(&g.root),
                    date(g.from),
                    date(g.to)
                );
                kit::dim(ui, &t, &span);
                ui.add_space(space::XS);
                if g.grew.is_empty() {
                    kit::dim(
                        ui,
                        &t,
                        &format!(
                            "No folder grew by {} or more.",
                            human_size(health::GREW_BYTES)
                        ),
                    );
                }
                for (i, x) in g.grew.iter().take(6).enumerate() {
                    ui.horizontal(|ui| {
                        let shown = app.env.display(&x.path);
                        let fid = font(size::CALLOUT, Weight::Medium);
                        let w = (ui.available_width() - 150.0).max(120.0);
                        // Each folder opens in Disk Map (look only).
                        let r = ui.push_id(("grew", i), |ui| {
                            kit::link(ui, &t, &kit::truncate_middle(ui.ctx(), &shown, &fid, w))
                        });
                        let r = r.inner.on_hover_text(crate::tr(format!("Open {shown} in Disk Map")));
                        r.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                crate::tr(format!("Open {shown} in Disk Map")),
                            )
                        });
                        if r.clicked() {
                            app.act();
                            app.open_route(&cleancraft_core::route::AppRoute::Feature(
                                cleancraft_core::FeatureId::SpaceLens,
                            ));
                            super::disk_map::show_folder(app, x.path.clone());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                crate::rich(format!("+{}", human_size(x.after - x.before)))
                                    .font(font(size::CALLOUT, Weight::SemiBold))
                                    .color(t.warning),
                            );
                        });
                    });
                }
            }
        }
    });
}

/// "Oct 18" in local time.
pub fn date(x: i64) -> String {
    chrono::DateTime::from_timestamp(x, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%b %-d").to_string())
        .unwrap_or_default()
}
