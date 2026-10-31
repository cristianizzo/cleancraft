//! Full control over MCP, on the fake Mac only: selection with a new digest, the gate on every
//! changing tool, the administrator path, uninstall with quitting first, the fence.

use std::path::{Path, PathBuf};

use rmcp::handler::server::wrapper::Parameters;
use serde_json::Value;

use cleancraft_core::app_quit::quit_args;
use cleancraft_core::executor::{self, ActionKind, Method};
use cleancraft_core::plan::{FeatureId, Risk};
use cleancraft_core::testkit::FakeMac;
use cleancraft_core::testkit_perf as tp;
use cleancraft_core::testkit_quit::FakeMacQuit;
use cleancraft_core::{Settings, plans};

use crate::control::*;
use crate::{CleanCraftMcp, ExecuteArgs, FeatureArg, execute_plan_json};

const KEYSTONE_PROG: &str = "/Users/tester/Library/Google/GoogleSoftwareUpdate/GoogleSoftwareUpdate.bundle/Contents/Resources/GoogleSoftwareUpdateAgent.app/Contents/MacOS/GoogleSoftwareUpdateAgent";
const UPDATER_PROG: &str = "/Library/Application Support/Google/GoogleUpdater/Current/GoogleUpdater.app/Contents/MacOS/GoogleUpdater";
const NORD_PROG: &str = "/Library/PrivilegedHelperTools/com.nordvpn.macos.helper";

fn allow(env: &cleancraft_core::Env) {
    let mut s = Settings::load(env);
    s.agents_may_clean = true;
    s.save(env).unwrap();
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

fn js(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

/// The user's Mac: two Keystone agents and the Google Updater wake agent whose programs are
/// gone, and NordVPN's helper daemon in /Library/LaunchDaemons (root-owned).
struct Startup {
    mac: FakeMac,
    keystone_agent: PathBuf,
    keystone_xpc: PathBuf,
    updater_wake: PathBuf,
    nord: PathBuf,
}

fn startup() -> Startup {
    let mac = tp::startup_mac();
    let keystone_agent = tp::add_launch_item(
        &mac,
        "~/Library/LaunchAgents",
        "com.google.keystone.agent",
        KEYSTONE_PROG,
        None,
    );
    let keystone_xpc = tp::add_launch_item(
        &mac,
        "~/Library/LaunchAgents",
        "com.google.keystone.xpcservice",
        KEYSTONE_PROG,
        None,
    );
    let updater_wake = tp::add_launch_item(
        &mac,
        "/Library/LaunchAgents",
        "com.google.GoogleUpdater.wake",
        UPDATER_PROG,
        None,
    );
    let nord = tp::add_launch_item(
        &mac,
        "/Library/LaunchDaemons",
        "com.nordvpn.macos.helper",
        NORD_PROG,
        Some(cleancraft_core::sys::CmdOut {
            status: 0,
            stdout: String::new(),
            stderr: tp::codesign_developer(NORD_PROG, "NordVPN", "W5W395V82Y"),
        }),
    );
    Startup {
        mac,
        keystone_agent,
        keystone_xpc,
        updater_wake,
        nord,
    }
}

/// `scan login_items` as an agent calls it: the plan id and the scan's digest.
fn scan_startup(mcp: &CleanCraftMcp) -> (String, String, Value) {
    let v = js(&rt()
        .block_on(mcp.scan(Parameters(FeatureArg {
            feature: "login_items".into(),
            depth: Default::default(),
        })))
        .unwrap());
    (
        v["plan_id"].as_str().unwrap().to_string(),
        v["digest"].as_str().unwrap().to_string(),
        v,
    )
}

fn shown(mac: &FakeMac, p: &Path) -> String {
    mac.env.display(p)
}

fn exec(env: &cleancraft_core::Env, plan_id: &str, digest: &str) -> Result<Value, String> {
    execute_plan_json(
        env,
        &ExecuteArgs {
            plan_id: plan_id.into(),
            digest: digest.into(),
            only_items: None,
        },
    )
}

#[test]
fn select_items_picks_unticked_startup_items_and_the_new_digest_is_the_one_that_runs() {
    let s = startup();
    let env = &s.mac.env;
    let mcp = CleanCraftMcp::new(env.clone());
    let (plan_id, scan_digest, v) = scan_startup(&mcp);
    // Nothing is ticked by default; the user's four items are there, the daemon needs admin.
    assert_eq!(v["selected_count"], 0);
    let items = v["items"].as_array().unwrap();
    let row = |p: &Path| {
        items
            .iter()
            .find(|i| i["path"] == shown(&s.mac, p))
            .unwrap_or_else(|| panic!("{} not listed", p.display()))
            .clone()
    };
    assert!(
        row(&s.keystone_agent)["reason"]
            .as_str()
            .unwrap()
            .contains("is missing")
    );
    assert_eq!(row(&s.nord)["locked"], Value::Null);

    // only_items can never reach unticked items: the old way stays refused.
    allow(env);
    let e = execute_plan_json(
        env,
        &ExecuteArgs {
            plan_id: plan_id.clone(),
            digest: scan_digest.clone(),
            only_items: Some(vec![row(&s.keystone_agent)["id"].as_u64().unwrap() as usize]),
        },
    )
    .unwrap_err();
    assert!(e.contains("only_items can only leave items out"), "{e}");

    // The agent selects exactly the four items by path (both spellings work).
    let p = select_items(
        env,
        &SelectArgs {
            plan_id: plan_id.clone(),
            paths: Some(vec![
                "~/Library/LaunchAgents/com.google.keystone.agent.plist".into(),
                shown(&s.mac, &s.keystone_xpc),
                shown(&s.mac, &s.updater_wake),
                shown(&s.mac, &s.nord),
            ]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(p["selected_count"], 4);
    assert_eq!(p["needs_admin_count"], 2);
    assert!(p["admin_note"].as_str().unwrap().contains("Touch ID"));
    assert!(p["next"].as_str().unwrap().contains("clear yes"));
    let new_digest = p["digest"].as_str().unwrap().to_string();
    assert_ne!(new_digest, scan_digest);
    let nord_row = p["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"] == shown(&s.mac, &s.nord))
        .unwrap()
        .clone();
    assert_eq!(nord_row["needs_admin"], true);
    assert!(nord_row["size"].is_string() && nord_row["risk"].is_string());
    // Saved: get_plan shows the same selection and digest.
    assert_eq!(plans::load(env, &plan_id).unwrap().digest(), new_digest);

    // The scan's digest is refused now, nothing moved.
    let e = exec(env, &plan_id, &scan_digest).unwrap_err();
    assert!(e.starts_with("Refused: the digest"), "{e}");
    assert!(s.keystone_agent.exists() && s.nord.exists());

    // The new digest runs: Keystone to the Trash, the system items through the
    // administrator path, with ONE password dialog that stops the daemon and then moves.
    let r = exec(env, &plan_id, &new_digest).unwrap();
    assert_eq!(r["moved"].as_array().unwrap().len(), 4, "{r:#}");
    assert!(r["skipped"].as_array().unwrap().is_empty(), "{r:#}");
    assert!(
        r["admin_note"]
            .as_str()
            .unwrap()
            .contains("approve it there")
    );
    for p in [&s.keystone_agent, &s.keystone_xpc, &s.updater_wake, &s.nord] {
        assert!(!p.exists(), "{} still there", p.display());
    }
    let trash = env.user_trash();
    assert!(trash.join("com.google.keystone.agent.plist").exists());
    assert!(trash.join("com.google.keystone.xpcservice.plist").exists());
    let fake = env.fake_sys().unwrap();
    let admin = fake.admin_calls();
    assert_eq!(admin.len(), 1, "one password dialog: {admin:#?}");
    let lines: Vec<&str> = admin[0].lines().collect();
    assert_eq!(
        lines[0], "/bin/launchctl bootout 'system/com.nordvpn.macos.helper' || true",
        "the daemon is stopped first, in the same command"
    );
    assert!(lines[1..].iter().all(|l| l.starts_with("/bin/mv -n -- ")));
    assert!(admin[0].contains("com.nordvpn.macos.helper.plist"));
    assert!(admin[0].contains("com.google.GoogleUpdater.wake.plist"));
    let script = cleancraft_core::sys::parse_admin_script(&admin[0]).expect("our own shape");
    assert_eq!(script.stops, vec!["system/com.nordvpn.macos.helper"]);
    assert_eq!(script.moves.len(), 2);
    let calls = fake.calls();
    for l in [
        "com.google.keystone.agent",
        "com.google.keystone.xpcservice",
        "com.google.GoogleUpdater.wake",
    ] {
        assert!(
            calls.contains(&format!("/bin/launchctl bootout gui/501/{l}")),
            "{l} not stopped: {calls:#?}"
        );
    }
    assert!(r["also_ran"].as_array().unwrap().iter().any(|c| {
        c["command"]
            .as_str()
            .unwrap()
            .contains("system/com.nordvpn")
    }));
    // Every move is in History with Undo; undo over MCP puts all four back.
    let undo_with = r["undo_with"].as_str().unwrap().to_string();
    let u = js(&rt()
        .block_on(mcp.undo(Parameters(crate::UndoArgs { id: undo_with })))
        .unwrap());
    assert_eq!(u["undone"], 4, "{u:#}");
    for p in [&s.keystone_agent, &s.keystone_xpc, &s.updater_wake, &s.nord] {
        assert!(p.exists(), "{} not back", p.display());
    }
}

#[test]
fn a_cancelled_password_dialog_skips_the_admin_items_and_the_rest_runs() {
    let s = startup();
    let env = &s.mac.env;
    allow(env);
    let mcp = CleanCraftMcp::new(env.clone());
    let (plan_id, _, _) = scan_startup(&mcp);
    let p = select_items(
        env,
        &SelectArgs {
            plan_id: plan_id.clone(),
            paths: Some(vec![
                shown(&s.mac, &s.keystone_agent),
                shown(&s.mac, &s.nord),
            ]),
            clear: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    env.fake_sys().unwrap().with(|f| f.admin_cancelled = true);
    let r = exec(env, &plan_id, p["digest"].as_str().unwrap()).unwrap();
    assert_eq!(r["moved"].as_array().unwrap().len(), 1);
    let skipped = r["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["path"], shown(&s.mac, &s.nord));
    assert!(
        skipped[0]["reason"]
            .as_str()
            .unwrap()
            .contains("administrator password was not given")
    );
    assert!(s.nord.exists() && !s.keystone_agent.exists());
    // The daemon whose file stayed is never stopped.
    assert_eq!(env.fake_sys().unwrap().admin_calls().len(), 1);
}

#[test]
fn a_daemon_whose_file_does_not_move_is_never_stopped() {
    let s = startup();
    let env = &s.mac.env;
    allow(env);
    let mcp = CleanCraftMcp::new(env.clone());
    let (plan_id, _, _) = scan_startup(&mcp);
    let p = select_items(
        env,
        &SelectArgs {
            plan_id: plan_id.clone(),
            paths: Some(vec![shown(&s.mac, &s.nord), shown(&s.mac, &s.updater_wake)]),
            ..Default::default()
        },
    )
    .unwrap();
    // macOS refuses even the administrator for the daemon's file.
    env.fake_sys().unwrap().fail_admin_move(&s.nord);
    let r = exec(env, &plan_id, p["digest"].as_str().unwrap()).unwrap();
    assert_eq!(r["moved"].as_array().unwrap().len(), 1);
    assert_eq!(r["skipped"][0]["path"], shown(&s.mac, &s.nord));
    assert!(s.nord.exists() && !s.updater_wake.exists());
    // The stop was asked for in the same command (the file was still there when it ran), but
    // nothing says the daemon stopped, so the journal holds no line about it.
    assert!(
        !executor::history(env).iter().any(|rec| matches!(
            &rec.kind,
            ActionKind::Command { command, .. } if command.contains("system/com.nordvpn")
        )),
        "a daemon whose file stayed was reported as stopped"
    );
    executor::undo_plan(env, r["undo_with"].as_str().unwrap());
    assert!(s.updater_wake.exists());
}

#[test]
fn select_items_refuses_locked_and_unknown_items_and_the_fence_still_holds() {
    let s = startup();
    let env = &s.mac.env;
    allow(env);
    let mcp = CleanCraftMcp::new(env.clone());
    let (plan_id, _, _) = scan_startup(&mcp);
    let e = select_items(
        env,
        &SelectArgs {
            plan_id: plan_id.clone(),
            item_ids: Some(vec![999]),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(e.contains("no item 999"), "{e}");
    assert!(
        select_items(
            env,
            &SelectArgs {
                plan_id: plan_id.clone(),
                ..Default::default()
            }
        )
        .is_err()
    );
    // A saved plan edited to hold a document and a locked row.
    let doc = s.mac.home_file("Documents/secret.plist", 2_000);
    let mut plan = plans::load(env, &plan_id).unwrap();
    let id = plan.push(
        doc.clone(),
        2_000,
        "user_agents",
        "edited in",
        Risk::Low,
        false,
    );
    let locked = plan.push(
        s.mac.home_file("Library/LaunchAgents/x.plist", 10),
        10,
        "user_agents",
        "locked",
        Risk::Low,
        false,
    );
    plan.lock(locked, "Kept for a reason");
    plans::save(env, &plan).unwrap();
    let e = select_items(
        env,
        &SelectArgs {
            plan_id: plan_id.clone(),
            item_ids: Some(vec![locked]),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(e.contains("Kept for a reason"), "{e}");
    let p = select_items(
        env,
        &SelectArgs {
            plan_id: plan_id.clone(),
            item_ids: Some(vec![id]),
            ..Default::default()
        },
    )
    .unwrap();
    let r = exec(env, &plan_id, p["digest"].as_str().unwrap()).unwrap();
    assert!(r["moved"].as_array().unwrap().is_empty());
    assert!(
        r["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("not allowed")
    );
    assert!(doc.exists(), "the fence let a document move");
    // select_all takes every unlocked item and leaves the locked one out.
    let p = select_items(
        env,
        &SelectArgs {
            plan_id: plan_id.clone(),
            select_all: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    let plan = plans::load(env, &plan_id).unwrap();
    assert_eq!(p["selected_count"], plan.items.len() - 1);
    assert!(!plan.items[locked].selected);
    // clear empties the selection.
    let p = select_items(
        env,
        &SelectArgs {
            plan_id,
            clear: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(p["selected_count"], 0);
}

#[test]
fn gate_off_refuses_every_changing_tool_and_nothing_runs() {
    let s = startup();
    let env = &s.mac.env;
    let mcp = CleanCraftMcp::new(env.clone());
    let (plan_id, digest, _) = scan_startup(&mcp);
    let before_calls = env.fake_sys().unwrap().calls().len();
    let changing: Vec<(&str, Box<dyn Fn() -> Result<String, String>>)> = vec![
        (
            "execute_plan",
            Box::new(|| exec(env, &plan_id, &digest).map(|v| v.to_string())),
        ),
        (
            "sweep_run",
            Box::new(|| {
                sweep_run_json(
                    env,
                    &[ConfirmedPlan {
                        plan_id: plan_id.clone(),
                        digest: digest.clone(),
                    }],
                )
            }),
        ),
        (
            "quit_app",
            Box::new(|| quit_app_json(env, "Example", true, true)),
        ),
        ("quit_process", Box::new(|| quit_process_json(env, 640))),
        ("empty_trash", Box::new(|| empty_trash_json(env, "0-0"))),
        (
            "erase_mail_deleted",
            Box::new(|| erase_mail_deleted_json(env, "Work")),
        ),
        (
            "reset_app_permissions",
            Box::new(|| reset_app_permissions_json(env, &plan_id, &[0])),
        ),
        (
            "quarantine_to_trash",
            Box::new(|| quarantine_to_trash_json(env, &["/x".into()])),
        ),
        (
            "update_settings",
            Box::new(|| {
                update_settings_json(
                    env,
                    &SettingsArgs {
                        scan_every: Some("daily".into()),
                        ..Default::default()
                    },
                )
            }),
        ),
        (
            "ignore_list",
            Box::new(|| {
                ignore_list_json(
                    env,
                    &IgnoreArgs {
                        add: Some(vec!["~/Library".into()]),
                        remove: None,
                    },
                )
            }),
        ),
        (
            "menu_bar_agent",
            Box::new(|| menu_bar_agent_json(env, true)),
        ),
        (
            "install_update",
            Box::new(|| install_update_json(env, "0.0.1")),
        ),
        (
            "run_task",
            Box::new(|| crate::perf::run_task(env, "flush_dns")),
        ),
        (
            "toggle_startup_item",
            Box::new(|| {
                crate::perf::toggle_startup_item(env, "user_agents:com.example.helper", false)
            }),
        ),
    ];
    for a in ACTIONS.iter().filter(|a| a.changes) {
        assert!(
            changing.iter().any(|(n, _)| *n == a.tool),
            "{} is not covered by this test",
            a.tool
        );
    }
    for (name, f) in &changing {
        let e = f().expect_err(name);
        assert!(e.starts_with("Refused"), "{name}: {e}");
        assert!(
            e.contains("Let AI assistants clean after you confirm"),
            "{name}: {e}"
        );
    }
    let f = env.fake_sys().unwrap();
    assert_eq!(
        f.calls().len(),
        before_calls,
        "a refused tool ran a command"
    );
    assert!(f.admin_calls().is_empty());
    assert!(executor::history(env).is_empty());
    assert!(!Settings::load(env).agents_may_clean);
    assert_eq!(Settings::load(env).scan_every, Default::default());
    // agent_permissions says it is off and how to turn it on; nothing can turn it on.
    let v = agent_permissions_json(env);
    assert_eq!(v["agents_may_clean"], false);
    assert!(v["how_to_turn_on"].as_str().unwrap().contains("Settings"));
    assert!(SettingsArgs::default().method.is_none());
    let schema = serde_json::to_string(&schemars::schema_for!(SettingsArgs)).unwrap();
    assert!(!schema.contains("agents_may_clean"));
}

#[test]
fn every_tool_is_in_list_actions_and_every_listed_tool_exists() {
    let m = FakeMac::new();
    let mcp = CleanCraftMcp::new(m.env.clone());
    let names: Vec<String> = mcp
        .tool_router
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    for n in &names {
        assert!(
            ACTIONS.iter().any(|a| a.tool == n),
            "{n} missing from list_actions"
        );
    }
    for a in ACTIONS {
        assert!(
            names.iter().any(|n| n == a.tool),
            "{} is not a tool",
            a.tool
        );
        assert!(!a.what.contains('\u{2014}'));
        // The banned conjunction, spelled out so the source never holds the word itself.
        let banned: String = ['n', 'o', 'r'].iter().collect();
        assert!(
            !a.what
                .to_lowercase()
                .split_whitespace()
                .any(|w| w == banned)
        );
        if a.changes {
            assert!(!a.preview.is_empty(), "{} has no preview step", a.tool);
        }
    }
    let v = list_actions_json();
    assert!(v["confirm_rule"].as_str().unwrap().contains("clear yes"));
}

const ED: &str = "com.example.Editor";

fn quit_key(id: &str) -> String {
    std::iter::once("/usr/bin/osascript".to_string())
        .chain(quit_args(id))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn uninstall_over_mcp_quits_the_running_app_first_then_moves_it_with_undo() {
    let m = FakeMac::new();
    let env = &m.env;
    let editor = m.app("Editor", ED, "1.0");
    let tool = m.app("Tool", "com.example.Tool", "2.0");
    let cache = m.home_file(&format!("Library/Caches/{ED}/c.db"), 5_000);
    m.user_id(501);
    m.run_program(&editor.join("Contents/MacOS/Editor"), 600, 501);
    m.launchd("gui/501", &[(600, "application.com.example.Editor.1.1")]);
    m.launchd("system", &[]);
    m.window_server(&[(600, "Editor", ED, "Foreground")]);
    let f = env.fake_sys().unwrap();
    f.answer("/usr/bin/mdfind", "");
    f.answer(&quit_key(ED), "");
    m.stops(&quit_key(ED), &[600]);
    let settings = Settings::default();

    let v = uninstall_apps_preview_json(env, &settings, &["Editor".into(), "Tool".into()]).unwrap();
    assert!(v["running_note"].is_string());
    let editor_row = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"] == m.env.display(&editor))
        .unwrap()
        .clone();
    assert!(editor_row["locked"].is_string(), "a running app is locked");
    let r = app_running_json(env, "Editor").unwrap();
    assert_eq!(r["running"], true);
    assert_eq!(r["blocks_uninstall"], true);

    // Quitting changes the Mac: refused until the user lets agents act.
    assert!(
        quit_app_json(env, "Editor", false, false)
            .unwrap_err()
            .starts_with("Refused")
    );
    allow(env);
    let q = js(&quit_app_json(env, "Editor", false, false).unwrap());
    assert_eq!(q["done"], true, "{q:#}");
    assert!(f.calls().contains(&quit_key(ED)));

    let v = uninstall_apps_preview_json(env, &settings, &["Editor".into(), "Tool".into()]).unwrap();
    assert!(v["running_note"].is_null());
    let plan_id = v["plan_id"].as_str().unwrap().to_string();
    let preview = &v["preview"];
    assert!(
        preview["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["path"] == m.env.display(&editor))
    );
    let r = exec(env, &plan_id, preview["digest"].as_str().unwrap()).unwrap();
    assert!(
        !editor.exists() && !tool.exists() && !cache.exists(),
        "{r:#}"
    );
    let apps = r["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 2);
    assert!(apps.iter().all(|a| a["uninstalled"] == true), "{r:#}");
    // Undo one app of the batch from History, then the rest.
    executor::undo_plan(env, r["undo_with"].as_str().unwrap());
    assert!(editor.exists() && tool.exists() && cache.exists());
}

#[test]
fn settings_ignore_list_menu_bar_and_quarantine_over_mcp() {
    let m = FakeMac::new();
    let env = &m.env;
    allow(env);
    let f = env.fake_sys().unwrap();
    f.answer("launchctl", "");
    f.answer("id -u", "501\n");
    // Settings: schedule, method; a bad value changes nothing.
    let r = js(&update_settings_json(
        env,
        &SettingsArgs {
            scan_every: Some("weekly".into()),
            scan_weekday: Some(5),
            scan_hour: Some(8),
            method: Some("quarantine".into()),
            ..Default::default()
        },
    )
    .unwrap());
    assert_eq!(r["before"]["scan_every"], "off");
    let s = Settings::load(env);
    assert_eq!(
        (s.scan_every, s.scan_weekday, s.scan_hour, s.method),
        (
            cleancraft_core::schedule::ScanEvery::Weekly,
            5,
            8,
            Method::Quarantine
        )
    );
    assert!(
        s.agents_may_clean,
        "update_settings never touches the switch"
    );
    assert!(
        update_settings_json(
            env,
            &SettingsArgs {
                scan_hour: Some(30),
                ..Default::default()
            }
        )
        .is_err()
    );
    // Ignore list: add then remove.
    let dir = m.home_file("Library/Caches/keep/x", 10);
    let dir = dir.parent().unwrap().to_path_buf();
    js(&ignore_list_json(
        env,
        &IgnoreArgs {
            add: Some(vec!["~/Library/Caches/keep".into()]),
            remove: None,
        },
    )
    .unwrap());
    assert_eq!(Settings::load(env).ignore, vec![dir.clone()]);
    assert!(
        ignore_list_json(
            env,
            &IgnoreArgs {
                add: Some(vec!["~/nothing/here".into()]),
                remove: None
            }
        )
        .is_err()
    );
    ignore_list_json(
        env,
        &IgnoreArgs {
            add: None,
            remove: Some(vec![m.env.display(&dir)]),
        },
    )
    .unwrap();
    assert!(Settings::load(env).ignore.is_empty());
    // Menu bar agent on, then undo over MCP.
    let r = js(&menu_bar_agent_json(env, true).unwrap());
    assert!(cleancraft_core::login_agent::is_on(env));
    let id = r["undo_with"].as_str().unwrap().to_string();
    f.answer("sh", "");
    let mcp = CleanCraftMcp::new(env.clone());
    let u = js(&rt()
        .block_on(mcp.undo(Parameters(crate::UndoArgs { id })))
        .unwrap());
    assert!(u["undone"].is_string(), "{u:#}");
    // Quarantine: clean into it, list it, move one item to the Trash, undo.
    let junk = m.home_file("Library/Logs/old.log", 40_000);
    let plan =
        cleancraft_core::features::scan(env, &Settings::load(env), FeatureId::SystemJunk).unwrap();
    plans::save(env, &plan).unwrap();
    let p = select_items(
        env,
        &SelectArgs {
            plan_id: plan.id.clone(),
            paths: Some(vec![m.env.display(&junk)]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(p["moves_to"], "the CleanCraft quarantine");
    exec(env, &plan.id, p["digest"].as_str().unwrap()).unwrap();
    let q = quarantine_status_json(env);
    let item = q["items"][0]["path"].as_str().unwrap().to_string();
    let r = js(&quarantine_to_trash_json(env, &[item]).unwrap());
    assert_eq!(r["moved"].as_array().unwrap().len(), 1);
    assert!(quarantine_to_trash_json(env, &["/etc/passwd".into()]).is_err());
}

#[test]
fn trash_status_token_guards_empty_trash() {
    let m = FakeMac::new();
    let env = &m.env;
    allow(env);
    m.home_file(".Trash/old.zip", 30_000);
    let v = trash_status_json(env);
    let token = v["empty_token"].as_str().unwrap().to_string();
    // The Trash changed after the user saw it: refused, Finder never asked.
    m.home_file(".Trash/new.zip", 10_000);
    let e = empty_trash_json(env, &token).unwrap_err();
    assert!(e.contains("changed"), "{e}");
    assert!(env.fake_sys().unwrap().calls().is_empty());
    let token = trash_status_json(env)["empty_token"]
        .as_str()
        .unwrap()
        .to_string();
    env.fake_sys().unwrap().answer("osascript", "");
    // The fake Finder does not empty anything; the request goes out and is logged.
    let _ = empty_trash_json(env, &token);
    assert!(
        env.fake_sys()
            .unwrap()
            .calls()
            .iter()
            .any(|c| c.contains("osascript"))
    );
    assert!(executor::history(env).iter().any(|r| matches!(
        &r.kind,
        ActionKind::Command { command, .. } if command.contains("osascript")
    )));
}

#[test]
fn sweep_preview_with_modules_and_sweep_run() {
    let m = FakeMac::sample();
    let env = &m.env;
    let sources = cleancraft_core::sweep::Sources::registered();
    let s = crate::monitor::sweep_settings(
        &Settings::default(),
        &sources,
        Some(&["system_junk".to_string()]),
    )
    .unwrap();
    let v = crate::monitor::sweep_preview_value(env, &s, &sources);
    let tiles = v["tiles"].as_array().unwrap();
    assert_eq!(tiles.len(), 1, "{v:#}");
    assert!(
        crate::monitor::sweep_settings(&Settings::default(), &sources, Some(&["x".into()]))
            .is_err()
    );
    let plan_id = tiles[0]["plan_id"].as_str().unwrap().to_string();
    let digest = tiles[0]["digest"].as_str().unwrap().to_string();
    allow(env);
    assert!(
        sweep_run_json(
            env,
            &[ConfirmedPlan {
                plan_id: plan_id.clone(),
                digest: "bad".into()
            }]
        )
        .unwrap_err()
        .contains("does not match")
    );
    let before = plans::load(env, &plan_id).unwrap();
    let r = js(&sweep_run_json(env, &[ConfirmedPlan { plan_id, digest }]).unwrap());
    let moved = r["modules"][0]["moved"].as_array().unwrap().len();
    assert_eq!(moved, before.selected_count(), "{r:#}");
}

#[test]
fn install_update_is_refused_while_the_app_is_open_then_installs() {
    use cleancraft_core::testkit_update::*;
    let m = FakeMac::new();
    let app = install_app(&m, "0.1.2");
    answer_check(&m, &release_json("0.1.3", true, "Faster."));
    answer_install(&m, &Script::good("0.1.3"));
    allow(&m.env);
    let open = cleancraft_core::sys::RunningApp {
        pid: 99_999,
        name: "CleanCraft".into(),
        bundle_id: None,
        path: Some(cleancraft_core::self_update::main_exe(&app)),
    };
    m.env.fake_sys().unwrap().with(|s| s.running.push(open));
    let e = install_update_json(&m.env, "0.1.2").unwrap_err();
    assert!(e.contains("app is open"), "{e}");
    assert_eq!(installed_version(&m).as_deref(), Some("0.1.2"));
    m.env.fake_sys().unwrap().with(|s| s.running.clear());
    let v = js(&install_update_json(&m.env, "0.1.2").unwrap());
    assert_eq!(v["installed"], true, "{v:#}");
    assert_eq!(installed_version(&m).as_deref(), Some("0.1.3"));
}
