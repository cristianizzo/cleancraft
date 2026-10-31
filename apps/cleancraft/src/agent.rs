//! `cleancraft agent`: the menu bar agent process (started at login by the LaunchAgent that
//! Settings > "Show CleanCraft in the menu bar" writes).
//!
//! Two threads: the worker (this file, no platform code) samples CPU, memory, disk and Trash
//! every few seconds, runs the schedule and alert checks of [`cleancraft_core::schedule`] and
//! carries out menu choices; the main thread runs the status item ([`crate::tray`], the only
//! macOS code). They talk over two channels: menu models one way, menu actions the other.
//! A click on the icon opens the live panel, a separate `cleancraft panel` process
//! ([`crate::panel`]) that reads on its own while open; its buttons come back here as the
//! same menu actions, so this worker stays the only writer of the agent's state.
//!
//! The agent never cleans or moves anything: it scans (look only), notifies and opens the app.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;
use cleancraft_core::known_malware;
use cleancraft_core::monitor::{self, Sampler, Snapshot};
use cleancraft_core::schedule::{self, Agent, CheckReport, MenuAction, MenuModel};
use cleancraft_core::self_update::{self, Auto};
use cleancraft_core::sweep::Sources;
use cleancraft_core::watch::{WatchEvent, WatchHandle, Watcher};
use cleancraft_core::{Env, Settings, login_agent, notify};

/// A new reading this often.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(5);
/// Schedule and alert checks this often.
pub const CHECK_EVERY: Duration = Duration::from_secs(60);

/// The worker's state, driven one round at a time (tests call [`Worker::round`] directly).
pub struct Worker {
    env: Env,
    agent: Agent,
    sampler: Sampler,
    snap: Option<Snapshot>,
    last_check: Option<Instant>,
    /// watch6's watcher while one of its Settings switches is on, with the switches it was
    /// started for.
    watcher: Option<(WatchHandle, (bool, bool))>,
    /// Events from the watcher thread, shown at the next round.
    events_tx: Sender<WatchEvent>,
    events: Receiver<WatchEvent>,
    /// The update check is running on its own thread.
    updating: Arc<AtomicBool>,
    /// The abuse.ch download is running on its own thread.
    fetching: Arc<AtomicBool>,
    /// The language setting last applied (menu and notifications follow a change).
    language: Option<String>,
}

impl Worker {
    pub fn new(env: Env, sources: Sources) -> Worker {
        let agent = Agent::new(&env, sources);
        let (events_tx, events) = mpsc::channel();
        Worker {
            env,
            agent,
            sampler: Sampler::new(),
            snap: None,
            last_check: None,
            watcher: None,
            events_tx,
            events,
            updating: Arc::default(),
            fetching: Arc::default(),
            language: None,
        }
    }

    /// Where watcher events go (tests send their own).
    #[cfg(test)]
    pub fn event_sender(&self) -> Sender<WatchEvent> {
        self.events_tx.clone()
    }

    /// Is a watcher running, and for which switches (trash offer, new items)?
    pub fn watching(&self) -> Option<(bool, bool)> {
        self.watcher.as_ref().map(|(_, k)| *k)
    }

    /// Start, restart or stop the watcher to match the Settings switches.
    fn follow_watch_settings(&mut self, settings: &Settings) {
        let want = (settings.watch_trash_offer, settings.watch_new_items);
        if self.watching() == Some(want) {
            return;
        }
        // Dropping the handle stops its thread.
        self.watcher = None;
        let Some(ws) = schedule::watch_settings(settings) else {
            return;
        };
        let tx = self.events_tx.clone();
        match Watcher::start(self.env.clone(), ws, move |ev| {
            let _ = tx.send(ev);
        }) {
            Ok(h) => self.watcher = Some((h, want)),
            Err(e) => eprintln!("CleanCraft agent: the watcher could not start: {e}"),
        }
    }

    /// One round: a new reading, the checks when [`CHECK_EVERY`] passed, and the menu to show.
    /// Settings are read again each round, so changes in the app apply within seconds.
    pub fn round(&mut self, mono: Instant, local: NaiveDateTime) -> (MenuModel, CheckReport) {
        let settings = Settings::load(&self.env);
        if self.language.as_ref() != Some(&settings.language) {
            cleancraft_core::i18n::apply(&self.env, &settings);
            self.language = Some(settings.language.clone());
        }
        self.snap = Some(self.sampler.sample(&self.env, monitor::now_secs()));
        let mut rep = CheckReport::default();
        self.follow_watch_settings(&settings);
        let evs: Vec<WatchEvent> = self.events.try_iter().collect();
        for ev in evs {
            let r = self
                .agent
                .notice(&self.env, &settings, schedule::watch_notification(&ev));
            rep.shown.extend(r.shown);
            rep.failed.extend(r.failed);
        }
        if self
            .last_check
            .is_none_or(|t| mono.duration_since(t) >= CHECK_EVERY)
        {
            self.last_check = Some(mono);
            let r = self
                .agent
                .check(&self.env, &settings, self.snap.as_ref(), local);
            rep.scanned = r.scanned;
            rep.shown.extend(r.shown);
            rep.failed.extend(r.failed);
            log_report(&rep);
            self.check_updates(&settings);
            self.fetch_known_malware();
        }
        (self.agent.menu(self.snap.as_ref(), &settings), rep)
    }

    /// CleanCraft's own update check, at most once every 24 hours: a newer version is
    /// notified (once per version), or installed when "Install automatically" is on and the
    /// app is closed (so no cleanup can be running). On a real Mac it runs on its own thread
    /// so the menu stays live; on a fake Mac (tests) right here.
    fn check_updates(&mut self, settings: &Settings) {
        let now = chrono::Utc::now().timestamp();
        if !settings.update_check_auto
            || !self_update::UpdateState::load(&self.env).due(now)
            || self.updating.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let env = self.env.clone();
        let settings = settings.clone();
        let flag = self.updating.clone();
        let work = move || {
            let install = self_update::installed_app(&env)
                .filter(|a| !self_update::app_running(&env, a))
                .map(|app| self_update::Target {
                    app,
                    current: self_update::CURRENT.into(),
                    busy: None,
                });
            let ctx = self_update::AutoCtx {
                now,
                notify: true,
                install,
                current: self_update::CURRENT,
            };
            let r = self_update::auto_check(&env, &settings, &ctx, &self_update::check);
            match r {
                Auto::Failed(e) => eprintln!("CleanCraft agent: {e}"),
                Auto::Installed(i) => eprintln!("CleanCraft agent: {}", i.message()),
                Auto::InstallFailed { why, .. } => {
                    eprintln!("CleanCraft agent: the update did not install: {why}")
                }
                _ => {}
            }
            flag.store(false, Ordering::SeqCst);
        };
        if self.env.is_fake() {
            work();
        } else {
            std::thread::spawn(work);
        }
    }

    /// abuse.ch's known Mac malware samples, once a day when a key is saved; on its own thread
    /// on a real Mac, right here on a fake one.
    fn fetch_known_malware(&mut self) {
        let now = chrono::Utc::now().timestamp();
        if !known_malware::due(&self.env, now) || self.fetching.swap(true, Ordering::SeqCst) {
            return;
        }
        let env = self.env.clone();
        let flag = self.fetching.clone();
        let work = move || {
            match known_malware::update(&env, now) {
                Ok(n) => eprintln!("CleanCraft agent: {n} new known malware samples from abuse.ch"),
                Err(e) => eprintln!("CleanCraft agent: abuse.ch list: {e}"),
            }
            flag.store(false, Ordering::SeqCst);
        };
        if self.env.is_fake() {
            work();
        } else {
            std::thread::spawn(work);
        }
    }

    /// A menu choice: true when the agent should quit. Errors are logged (there is no window
    /// to show them in).
    pub fn act(&mut self, a: MenuAction) -> (bool, MenuModel) {
        let mut settings = Settings::load(&self.env);
        let quit = match self.agent.act(&self.env, &mut settings, a) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("CleanCraft agent: {e}");
                false
            }
        };
        (quit, self.agent.menu(self.snap.as_ref(), &settings))
    }
}

fn log_report(rep: &CheckReport) {
    if let Some(b) = rep.scanned {
        eprintln!("CleanCraft agent: scheduled scan found {b} bytes ready to clean");
    }
    for f in &rep.failed {
        eprintln!("CleanCraft agent: {f}");
    }
}

/// Run the agent until Quit. Refuses when another agent runs.
pub fn run(env: &Env) -> anyhow::Result<()> {
    if !login_agent::is_on(env) {
        anyhow::bail!(
            "The menu bar is off. Turn on Settings > \"Show CleanCraft in the menu bar\" in CleanCraft."
        );
    }
    notify::claim_agent(env, std::process::id()).map_err(anyhow::Error::msg)?;
    // Notifications posted as CleanCraft, opening the app when clicked (osascript fallback).
    crate::native_notify::install(env);
    let (to_ui, models) = mpsc::channel::<MenuModel>();
    let (actions, from_ui) = mpsc::channel::<MenuAction>();
    let worker_env = env.clone();
    std::thread::spawn(move || {
        worker_loop(
            Worker::new(worker_env, Sources::registered()),
            &to_ui,
            &from_ui,
        )
    });
    crate::tray::run(models, actions)
}

/// Rounds every [`SAMPLE_EVERY`]; menu choices are handled as they come. Ends (and with it
/// the status item, whose channel closes) on Quit or when the menu bar is turned off: off
/// means off, also for an agent that launchctl did not stop.
fn worker_loop(mut w: Worker, to_ui: &Sender<MenuModel>, from_ui: &Receiver<MenuAction>) {
    loop {
        if !login_agent::is_on(&w.env) {
            return;
        }
        let (model, _) = w.round(Instant::now(), schedule::local_now());
        if to_ui.send(model).is_err() {
            return;
        }
        let until = Instant::now() + SAMPLE_EVERY;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match from_ui.recv_timeout(left) {
                Ok(a) => {
                    let (quit, model) = w.act(a);
                    if quit || to_ui.send(model).is_err() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use cleancraft_core::schedule::{QUICK_SWEEP, ScanEvery};
    use cleancraft_core::testkit_monitor::{answer_all, sweep_mac, sweep_sources};

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap_or_default()
    }

    #[test]
    fn a_round_shows_cpu_ram_and_the_four_lines_and_cleans_nothing() {
        let m = sweep_mac();
        let fake = m.env.fake_sys().expect("fake");
        answer_all(fake);
        let mut w = Worker::new(m.env.clone(), sweep_sources());
        let (model, rep) = w.round(Instant::now(), at("2026-10-20 09:00"));
        assert!(model.title.starts_with("CPU 17% RAM "), "{}", model.title);
        assert!(!model.disk_low);
        assert_eq!(model.info[0], "CPU: 17%");
        assert!(
            model.info[2].starts_with("Free space: "),
            "{:?}",
            model.info
        );
        assert!(model.info[3].starts_with("Trash: "), "{:?}", model.info);
        assert_eq!(rep.scanned, None, "scans are off by default");
        assert!(cleancraft_core::executor::history(&m.env).is_empty());
    }

    #[test]
    fn menu_choices_open_the_app_and_pause() {
        let m = sweep_mac();
        let fake = m.env.fake_sys().expect("fake");
        answer_all(fake);
        fake.answer("open", "");
        let mut w = Worker::new(m.env.clone(), sweep_sources());
        w.round(Instant::now(), at("2026-10-20 09:00"));
        let (quit, _) = w.act(MenuAction::QuickSweep);
        assert!(!quit);
        assert!(
            fake.calls()
                .contains(&"/usr/bin/open -b com.cristianizzo.cleancraft".to_string())
        );
        let mut inbox = notify::OpenInbox::default();
        assert_eq!(
            inbox
                .poll(&m.env, chrono::Utc::now().timestamp())
                .as_deref(),
            Some("quick_sweep"),
            "{QUICK_SWEEP} opens Quick Sweep"
        );
        // The panel's quick actions open the app on their page, and nothing more.
        let (quit, _) = w.act(MenuAction::Route("scan:trash_bins"));
        assert!(!quit);
        assert_eq!(
            inbox
                .poll(&m.env, chrono::Utc::now().timestamp())
                .as_deref(),
            Some("scan:trash_bins")
        );
        assert!(cleancraft_core::executor::history(&m.env).is_empty());
        let (_, model) = w.act(MenuAction::TogglePause);
        assert!(model.paused);
        assert!(Settings::load(&m.env).notifications_paused);
        assert_eq!(model.pause_label(), schedule::RESUME);
        let (quit, _) = w.act(MenuAction::Quit);
        assert!(quit);
    }

    #[test]
    fn the_schedule_runs_from_rounds_once_per_check() {
        let m = sweep_mac();
        let fake = m.env.fake_sys().expect("fake");
        answer_all(fake);
        fake.answer("osascript", "");
        let s = Settings {
            scan_every: ScanEvery::Daily,
            scan_hour: 10,
            ..Settings::default()
        };
        s.save(&m.env).expect("save");
        let mut w = Worker::new(m.env.clone(), sweep_sources());
        let t0 = Instant::now();
        let (_, rep) = w.round(t0, at("2026-10-20 09:00"));
        assert_eq!(rep.scanned, None, "armed at 09:00, nothing due yet");
        // Next round within a minute: no check at all.
        let (_, rep) = w.round(t0 + Duration::from_secs(5), at("2026-10-20 10:30"));
        assert_eq!(rep.scanned, None);
        let (model, rep) = w.round(t0 + CHECK_EVERY, at("2026-10-20 10:31"));
        assert!(rep.scanned.is_some_and(|b| b > 0), "{rep:?}");
        assert_eq!(model.reviews.len(), 1);
        assert!(cleancraft_core::executor::history(&m.env).is_empty());
    }

    #[test]
    fn watcher_switches_start_it_and_its_events_become_notices() {
        let m = sweep_mac();
        let fake = m.env.fake_sys().expect("fake");
        answer_all(fake);
        fake.answer("osascript", "");
        let mut w = Worker::new(m.env.clone(), sweep_sources());
        w.round(Instant::now(), at("2026-10-20 09:00"));
        assert_eq!(w.watching(), None, "off by default");
        let s = Settings {
            watch_trash_offer: true,
            ..Settings::default()
        };
        s.save(&m.env).expect("save");
        w.round(Instant::now(), at("2026-10-20 09:00"));
        assert_eq!(w.watching(), Some((true, false)));
        w.event_sender()
            .send(WatchEvent {
                id: "x".into(),
                kind: cleancraft_core::watch::WatchKind::TrashedApp,
                title: "-e Editor went to the Trash".into(),
                body: "Its leftovers take 12 MB.".into(),
                open: cleancraft_core::route::AppRoute::UninstallApp(
                    m.env.user_trash().join("Editor.app"),
                ),
                time: 0,
                paths: Vec::new(),
            })
            .expect("send");
        let (model, rep) = w.round(Instant::now(), at("2026-10-20 09:00"));
        assert_eq!(rep.shown.len(), 1);
        let line = fake
            .calls()
            .into_iter()
            .find(|c| c.starts_with("/usr/bin/osascript"))
            .unwrap_or_default();
        assert!(
            line.ends_with(&format!(
                "end run -- -e Editor went to the Trash Its leftovers take 12 MB. {}",
                notify::MENU_HINT
            )),
            "{line}"
        );
        assert_eq!(model.reviews.len(), 1);
        assert!(model.reviews[0].route.starts_with("uninstall:"));
        // Both off: the watcher stops.
        Settings::default().save(&m.env).expect("save");
        w.round(Instant::now(), at("2026-10-20 09:00"));
        assert_eq!(w.watching(), None);
        assert!(cleancraft_core::executor::history(&m.env).is_empty());
    }

    #[test]
    fn the_agent_checks_for_updates_once_a_day_and_notifies_without_installing() {
        use cleancraft_core::testkit_update::*;
        let m = sweep_mac();
        let fake = m.env.fake_sys().expect("fake");
        answer_all(fake);
        fake.answer("osascript", "");
        answer_check(&m, &release_json("99.0.0", true, "Big release."));
        let mut w = Worker::new(m.env.clone(), sweep_sources());
        let t0 = Instant::now();
        w.round(t0, at("2026-10-20 09:00"));
        let told = |f: &cleancraft_core::sys::FakeSys| {
            f.calls()
                .iter()
                .filter(|c| c.contains("A CleanCraft update is available"))
                .count()
        };
        assert_eq!(told(fake), 1);
        let checks = |f: &cleancraft_core::sys::FakeSys| {
            f.calls().iter().filter(|c| **c == check_line()).count()
        };
        assert_eq!(checks(fake), 1);
        // The next check round, a minute later: not due for a day.
        w.round(t0 + CHECK_EVERY, at("2026-10-20 09:01"));
        assert_eq!(checks(fake), 1);
        assert_eq!(told(fake), 1);
        assert!(!fake.calls().iter().any(|c| c.contains("hdiutil")));
        assert!(cleancraft_core::executor::history(&m.env).is_empty());
    }
}
