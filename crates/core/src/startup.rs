//! Startup Items: what starts when you log in, and the apps using a lot of the Mac right now.
//!
//! - Login items come from System Events (`osascript`). Removing one is undoable: the undo
//!   command adds it back with the same path.
//! - Launch agents and daemons are the property lists in `~/Library/LaunchAgents`,
//!   `/Library/LaunchAgents` and `/Library/LaunchDaemons`. Each shows its program, whether the
//!   program exists, who signed it (`codesign -dv`) and whether launchd will start it
//!   (`launchctl print-disabled`). Apple-signed items are shown as part of macOS and cannot be
//!   changed. Turning an item off is `launchctl disable` plus `launchctl bootout`; the journal
//!   line carries the command that turns it back on, so History can undo it.
//! - Removing an agent's property list goes through the plan and executor like any cleanup
//!   ([`StartupFeature`]); the agent is stopped first.
//! - Heavy apps come from `ps`. Quitting asks the app politely (`osascript ... to quit`) or
//!   sends `kill -TERM`, only for this user's processes, never CleanCraft or macOS itself.
//!
//! Every command goes through `env.sys`, so tests answer them with `FakeSys`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::env::Env;
use crate::executor::{self, ActionKind, ActionRecord};
use crate::features::{Feature, ScanCtx};
use crate::fsutil::tree_size;
use crate::plan::{FeatureId, Plan, Risk};
use crate::sys::CmdOut;
use crate::tasks::{self, is_admin_cancel, journal_writable, sh_quote};

pub const LAUNCHCTL: &str = "/bin/launchctl";
pub const OSASCRIPT: &str = "/usr/bin/osascript";
pub const CODESIGN: &str = "/usr/bin/codesign";

// ---------------------------------------------------------------------------------------------
// Signatures

/// Who signed a program, from `codesign -dv --verbose=2`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Signer {
    /// Part of macOS ("Software Signing" authority).
    Apple,
    /// Signed with a developer certificate: the first authority and the team id.
    Developer {
        authority: String,
        team: Option<String>,
    },
    /// Signed ad hoc: no identity behind it.
    AdHoc,
    /// Not signed at all.
    Unsigned,
    /// The program file does not exist.
    Missing,
    /// codesign could not tell (the message).
    Unknown { message: String },
}

impl Signer {
    pub fn label(&self) -> String {
        match self {
            Signer::Apple => "Apple (part of macOS)".into(),
            Signer::Developer { authority, .. } => authority
                .strip_prefix("Developer ID Application: ")
                .or_else(|| authority.strip_prefix("Apple Development: "))
                .or_else(|| authority.strip_prefix("Apple Distribution: "))
                .unwrap_or(authority)
                .to_string(),
            // Plain words (REVIEW-U U21): "ad hoc" means nobody known signed it.
            Signer::AdHoc => "Not signed by a known developer".into(),
            Signer::Unsigned => "Not signed".into(),
            Signer::Missing => "Program missing".into(),
            Signer::Unknown { message } => format!("Signature unknown ({message})"),
        }
    }

    /// Unsigned or ad hoc: nobody vouches for this program.
    pub fn is_weak(&self) -> bool {
        matches!(self, Signer::AdHoc | Signer::Unsigned)
    }
}

/// Parse `codesign -dv --verbose=2 <path>` (it writes to stderr).
pub fn parse_codesign(out: &CmdOut) -> Signer {
    let text = format!("{}\n{}", out.stderr, out.stdout);
    if text.contains("code object is not signed at all") {
        return Signer::Unsigned;
    }
    if text.contains("No such file or directory") {
        return Signer::Missing;
    }
    let mut authorities = Vec::new();
    let mut team = None;
    let mut adhoc = false;
    for l in text.lines() {
        let l = l.trim();
        if let Some(a) = l.strip_prefix("Authority=") {
            authorities.push(a.to_string());
        } else if let Some(t) = l.strip_prefix("TeamIdentifier=") {
            if t != "not set" {
                team = Some(t.to_string());
            }
        } else if l == "Signature=adhoc" || (l.starts_with("CodeDirectory") && l.contains("adhoc"))
        {
            adhoc = true;
        }
    }
    if adhoc {
        return Signer::AdHoc;
    }
    match authorities.first() {
        Some(a) if a == "Software Signing" => Signer::Apple,
        Some(a) => Signer::Developer {
            authority: a.clone(),
            team,
        },
        None if out.ok() => Signer::Unknown {
            message: "no signing authority listed".into(),
        },
        None => Signer::Unknown {
            message: tasks::first_lines(&out.stderr, 1),
        },
    }
}

/// Signature of a program, by its macOS path.
pub fn signer_of(env: &Env, program: &str) -> Signer {
    if !env.sys_path(program).exists() {
        return Signer::Missing;
    }
    match env.sys.run(CODESIGN, &["-dv", "--verbose=2", program]) {
        Ok(out) => parse_codesign(&out),
        Err(e) => Signer::Unknown {
            message: e.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Launch agents and daemons

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    /// `~/Library/LaunchAgents`: yours, runs when you log in.
    UserAgent,
    /// `/Library/LaunchAgents`: installed for every user, runs in your session.
    GlobalAgent,
    /// `/Library/LaunchDaemons`: runs as the system, before anyone logs in.
    Daemon,
}

impl Domain {
    pub const ALL: [Domain; 3] = [Domain::UserAgent, Domain::GlobalAgent, Domain::Daemon];

    /// The folder as a macOS path ("~/" is the home folder).
    pub fn folder(self) -> &'static str {
        match self {
            Domain::UserAgent => "~/Library/LaunchAgents",
            Domain::GlobalAgent => "/Library/LaunchAgents",
            Domain::Daemon => "/Library/LaunchDaemons",
        }
    }

    pub fn path(self, env: &Env) -> PathBuf {
        match self.folder().strip_prefix("~/") {
            Some(rel) => env.home_path(rel),
            None => env.sys_path(self.folder()),
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Domain::UserAgent => "user_agents",
            Domain::GlobalAgent => "global_agents",
            Domain::Daemon => "daemons",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Domain::UserAgent => "Your launch agents",
            Domain::GlobalAgent => "Launch agents for all users",
            Domain::Daemon => "System launch daemons",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Domain::UserAgent => {
                "Background helpers apps installed for you. They start when you log in."
            }
            Domain::GlobalAgent => {
                "Background helpers installed for every user. They start when anyone logs in."
            }
            Domain::Daemon => {
                "Background services that run for the whole Mac, even before login. Changing them needs the administrator password."
            }
        }
    }

    /// Needs the administrator password to change.
    pub fn admin(self) -> bool {
        self == Domain::Daemon
    }
}

/// What a launchd property list says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchPlist {
    pub label: Option<String>,
    /// `Program`, else `ProgramArguments[0]`.
    pub program: Option<String>,
    /// The `Disabled` key.
    pub disabled: bool,
    pub run_at_load: bool,
    pub keep_alive: bool,
}

pub fn read_launch_plist(path: &Path) -> Result<LaunchPlist, String> {
    let v = plist::Value::from_file(path).map_err(|e| e.to_string())?;
    let d = v
        .as_dictionary()
        .ok_or_else(|| "not a launchd property list".to_string())?;
    let s = |k: &str| d.get(k).and_then(|x| x.as_string()).map(str::to_string);
    let b = |k: &str| d.get(k).and_then(|x| x.as_boolean()).unwrap_or(false);
    let program = s("Program").or_else(|| {
        d.get("ProgramArguments")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|x| x.as_string())
            .map(str::to_string)
    });
    Ok(LaunchPlist {
        label: s("Label"),
        program,
        disabled: b("Disabled"),
        run_at_load: b("RunAtLoad"),
        keep_alive: d.get("KeepAlive").is_some_and(|k| {
            k.as_boolean().unwrap_or(true) // a dictionary of conditions also keeps it alive
        }),
    })
}

/// `launchctl print-disabled <domain>`: label -> disabled. Newer macOS prints
/// `"label" => disabled|enabled`, older `"label" => true|false` (true = disabled).
pub fn parse_print_disabled(stdout: &str) -> BTreeMap<String, bool> {
    let mut m = BTreeMap::new();
    let mut in_block = false;
    for l in stdout.lines() {
        let t = l.trim();
        if t.starts_with("disabled services") {
            in_block = true;
            continue;
        }
        if in_block && t.starts_with('}') {
            break;
        }
        if !in_block {
            continue;
        }
        let Some((k, v)) = t.split_once("=>") else {
            continue;
        };
        let k = k.trim().trim_matches('"').to_string();
        let v = v.trim();
        let disabled = match v {
            "disabled" | "true" => true,
            "enabled" | "false" => false,
            _ => continue,
        };
        m.insert(k, disabled);
    }
    m
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LaunchItem {
    pub domain: Domain,
    pub label: String,
    /// The property list (real path in the env).
    pub plist: PathBuf,
    /// The property list as macOS names it.
    pub plist_shown: String,
    /// The program, as a macOS path.
    pub program: Option<String>,
    pub program_exists: bool,
    pub signer: Signer,
    /// launchd will start it (override from `print-disabled`, else the `Disabled` key).
    pub enabled: bool,
    /// Signed by Apple: part of macOS, never changed.
    pub apple: bool,
    pub run_at_load: bool,
    /// Could not be read; the text says why.
    pub problem: Option<String>,
}

impl LaunchItem {
    pub fn id(&self) -> String {
        format!("{}:{}", self.domain.id(), self.label)
    }

    pub fn display_name(&self) -> String {
        self.label.clone()
    }
}

/// Property lists of one folder, sorted by name. Hidden files are included (malware likes them).
pub fn plists_in(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "plist") && p.is_file())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Overrides of one launchd domain ("gui/501" or "system"). Empty when launchctl fails.
pub fn disabled_overrides(env: &Env, domain: &str) -> Result<BTreeMap<String, bool>, String> {
    let out = env
        .sys
        .run(LAUNCHCTL, &["print-disabled", domain])
        .map_err(|e| e.to_string())?;
    if !out.ok() {
        return Err(tasks::first_lines(&out.stderr, 1));
    }
    Ok(parse_print_disabled(&out.stdout))
}

/// One launch agent or daemon, read from its property list `p` in the folder of `d`.
/// `overrides` are launchd's turned-off list for that domain ([`disabled_overrides`]).
pub fn launch_item_at(
    env: &Env,
    d: Domain,
    p: &Path,
    overrides: &BTreeMap<String, bool>,
) -> LaunchItem {
    let fallback_label = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (lp, problem) = match read_launch_plist(p) {
        Ok(lp) => (lp, None),
        Err(e) => (
            LaunchPlist::default(),
            Some(format!("could not be read: {e}")),
        ),
    };
    let label = lp.label.clone().unwrap_or(fallback_label);
    let program_exists = lp
        .program
        .as_ref()
        .is_some_and(|pr| pr.starts_with('/') && env.sys_path(pr).exists());
    let signer = if label == crate::login_agent::LABEL {
        // CleanCraft's own menu bar agent: never flag it as suspicious, whatever this build
        // was signed with.
        Signer::Developer {
            authority: "CleanCraft".into(),
            team: None,
        }
    } else {
        match &lp.program {
            Some(pr) if program_exists => signer_of(env, pr),
            Some(_) => Signer::Missing,
            None => Signer::Unknown {
                message: "no program named".into(),
            },
        }
    };
    let enabled = match overrides.get(&label) {
        Some(disabled) => !disabled,
        None => !lp.disabled,
    };
    LaunchItem {
        domain: d,
        apple: signer == Signer::Apple,
        label,
        plist_shown: env.display(p),
        plist: p.to_path_buf(),
        program: lp.program,
        program_exists,
        signer,
        enabled,
        run_at_load: lp.run_at_load,
        problem,
    }
}

/// Every launch agent and daemon, with notes about what could not be read.
pub fn list_launch_items(env: &Env, uid: Option<u32>) -> (Vec<LaunchItem>, Vec<String>) {
    let mut notes = Vec::new();
    let gui = match uid {
        Some(u) => disabled_overrides(env, &format!("gui/{u}")).unwrap_or_else(|e| {
            notes.push(format!(
                "Could not read which agents are turned off ({e}); the property lists decide."
            ));
            BTreeMap::new()
        }),
        None => BTreeMap::new(),
    };
    let system = disabled_overrides(env, "system").unwrap_or_else(|e| {
        notes.push(format!(
            "Could not read which daemons are turned off ({e}); the property lists decide."
        ));
        BTreeMap::new()
    });
    let mut items = Vec::new();
    for d in Domain::ALL {
        let dir = d.path(env);
        if !dir.exists() {
            continue;
        }
        if fs::read_dir(&dir).is_err() {
            notes.push(format!("{} could not be read.", env.display(&dir)));
            continue;
        }
        let overrides = if d == Domain::Daemon { &system } else { &gui };
        for p in plists_in(&dir) {
            items.push(launch_item_at(env, d, &p, overrides));
        }
    }
    (items, notes)
}

// ---------------------------------------------------------------------------------------------
// Login items

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LoginItem {
    pub name: String,
    /// macOS path of the app, when System Events reports it.
    pub path: Option<String>,
}

/// AppleScript lines for `osascript -e ... -e ...`: one "name<TAB>path" line per login item.
pub const LOGIN_ITEMS_SCRIPT: &[&str] = &[
    "set out to \"\"",
    "tell application \"System Events\"",
    "repeat with i in (every login item)",
    "set p to \"\"",
    "try",
    "set p to (path of i) as text",
    "end try",
    "set out to out & (name of i) & tab & p & linefeed",
    "end repeat",
    "end tell",
    "return out",
];

/// The arguments for `osascript` running `lines`.
pub fn osascript_args(lines: &[&str]) -> Vec<String> {
    lines
        .iter()
        .flat_map(|l| ["-e".to_string(), l.to_string()])
        .collect()
}

/// The key FakeSys matches for a command (program file name and arguments joined by spaces).
pub fn command_key(program: &str, args: &[String]) -> String {
    let short = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    std::iter::once(short)
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_login_items(stdout: &str) -> Vec<LoginItem> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (name, path) = l.split_once('\t').unwrap_or((l, ""));
            let path = path.trim();
            LoginItem {
                name: name.trim().to_string(),
                path: (!path.is_empty() && path != "missing value").then(|| path.to_string()),
            }
        })
        .collect()
}

/// Login items. `Err` explains a refusal (Automation permission) or failure.
pub fn list_login_items(env: &Env) -> Result<Vec<LoginItem>, String> {
    let args = osascript_args(LOGIN_ITEMS_SCRIPT);
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = env
        .sys
        .run(OSASCRIPT, &a)
        .map_err(|e| format!("Could not ask System Events for login items: {e}"))?;
    if !out.ok() {
        if out.stderr.contains("-1743") || out.stderr.contains("Not authorized") {
            return Err("CleanCraft is not allowed to ask System Events. Allow it in System Settings > Privacy & Security > Automation.".into());
        }
        return Err(format!(
            "System Events did not list login items: {}",
            tasks::first_lines(&out.stderr, 1)
        ));
    }
    Ok(parse_login_items(&out.stdout))
}

/// Escape for an AppleScript string literal.
pub fn as_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A shell command that runs `cmd` as administrator through the macOS password dialog.
/// Used for undo commands, which History runs with `/bin/sh -c`.
pub fn admin_wrap(cmd: &str) -> String {
    let script = format!(
        "do shell script {} with administrator privileges",
        as_quote(cmd)
    );
    format!("{OSASCRIPT} -e {}", sh_quote(&script))
}

pub const LOGIN_REMOVED_PREFIX: &str = "Removed login item ";

// ---------------------------------------------------------------------------------------------
// Turning things off and on

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", content = "message")]
pub enum ChangeError {
    /// The administrator dialog was cancelled; nothing changed.
    Cancelled,
    /// CleanCraft will not do this (an Apple item, CleanCraft itself...).
    Refused(String),
    /// The command failed; nothing or only part changed (the text says).
    Failed(String),
}

impl std::fmt::Display for ChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeError::Cancelled => write!(f, "{}", tasks::CANCELLED_TEXT),
            ChangeError::Refused(s) | ChangeError::Failed(s) => write!(f, "{s}"),
        }
    }
}

fn new_record(kind: ActionKind, tag: &str) -> ActionRecord {
    let now = chrono::Utc::now();
    let plan_id = format!("startup-{tag}-{}", now.timestamp_nanos_opt().unwrap_or(0));
    ActionRecord {
        id: format!("{plan_id}#0"),
        plan_id,
        feature: Some(FeatureId::LoginItems),
        time: now.timestamp(),
        size: 0,
        kind,
        undone: None,
    }
}

fn log(env: &Env, rec: ActionRecord) -> Result<ActionRecord, ChangeError> {
    executor::append_journal(env, &rec).map_err(|e| {
        ChangeError::Failed(format!(
            "The change was made, but the action log could not be written: {e}"
        ))
    })?;
    Ok(rec)
}

fn run_ok(env: &Env, program: &str, args: &[&str]) -> Result<CmdOut, String> {
    let out = env
        .sys
        .run(program, args)
        .map_err(|e| format!("could not run {program}: {e}"))?;
    if out.ok() {
        Ok(out)
    } else {
        let why = tasks::first_lines(&out.stderr, 1);
        Err(if why.is_empty() {
            format!(
                "{} {} failed (exit code {})",
                program,
                args.join(" "),
                out.status
            )
        } else {
            why
        })
    }
}

/// The shell commands that turn a launch item on or off (without the administrator wrapper).
pub fn launch_commands(item: &LaunchItem, uid: u32, on: bool) -> String {
    let (domain, target) = match item.domain {
        Domain::Daemon => ("system".to_string(), format!("system/{}", item.label)),
        _ => (format!("gui/{uid}"), format!("gui/{uid}/{}", item.label)),
    };
    let plist = sh_quote(&item.plist_shown);
    let target = sh_quote(&target);
    if on {
        format!(
            "{LAUNCHCTL} enable {target} && {{ {LAUNCHCTL} bootstrap {domain} {plist} || true; }}"
        )
    } else {
        format!(
            "{LAUNCHCTL} disable {target} && {{ {LAUNCHCTL} bootout {domain} {plist} || true; }}"
        )
    }
}

/// Turn a launch agent or daemon on or off. Logged with the undo command.
pub fn set_launch_item_enabled(
    env: &Env,
    item: &LaunchItem,
    uid: u32,
    on: bool,
) -> Result<ActionRecord, ChangeError> {
    if item.apple {
        return Err(ChangeError::Refused(
            "This item is part of macOS and is not changed by CleanCraft.".into(),
        ));
    }
    journal_writable(env).map_err(|e| {
        ChangeError::Failed(format!(
            "Could not write the action log, nothing changed: {e}"
        ))
    })?;
    let verb = if on { "Turned on" } else { "Turned off" };
    let what = format!("{verb} {} ({})", item.label, item.plist_shown);
    let undo_cmd = launch_commands(item, uid, !on);
    if item.domain.admin() {
        let cmd = launch_commands(item, uid, on);
        let out = env
            .sys
            .run_admin(&cmd)
            .map_err(|e| ChangeError::Failed(e.to_string()))?;
        if is_admin_cancel(&out) {
            return Err(ChangeError::Cancelled);
        }
        if !out.ok() {
            return Err(ChangeError::Failed(format!(
                "launchctl refused: {}",
                tasks::first_lines(&out.stderr, 1)
            )));
        }
        return log(
            env,
            new_record(
                ActionKind::Toggled {
                    what,
                    undo_command: Some(admin_wrap(&undo_cmd)),
                },
                "daemon",
            ),
        );
    }
    let target = format!("gui/{uid}/{}", item.label);
    let domain = format!("gui/{uid}");
    if on {
        run_ok(env, LAUNCHCTL, &["enable", &target])
            .map_err(|e| ChangeError::Failed(format!("Could not turn it on: {e}")))?;
        // Already loaded is fine.
        let _ = env
            .sys
            .run(LAUNCHCTL, &["bootstrap", &domain, &item.plist_shown]);
    } else {
        run_ok(env, LAUNCHCTL, &["disable", &target])
            .map_err(|e| ChangeError::Failed(format!("Could not turn it off: {e}")))?;
        // Not running is fine: it stays off from the next login.
        let _ = env
            .sys
            .run(LAUNCHCTL, &["bootout", &domain, &item.plist_shown]);
    }
    log(
        env,
        new_record(
            ActionKind::Toggled {
                what,
                undo_command: Some(undo_cmd),
            },
            "agent",
        ),
    )
}

/// The AppleScript that adds a login item back.
pub fn add_login_item_script(path: &str) -> String {
    format!(
        "tell application \"System Events\" to make login item at end with properties {{path:{}, hidden:false}}",
        as_quote(path)
    )
}

/// Remove a login item through System Events. Undo adds it back.
pub fn remove_login_item(env: &Env, item: &LoginItem) -> Result<ActionRecord, ChangeError> {
    let Some(path) = item.path.clone() else {
        return Err(ChangeError::Refused(
            "macOS did not say where this item is, so CleanCraft could not put it back later. Remove it in System Settings > General > Login Items.".into(),
        ));
    };
    journal_writable(env).map_err(|e| {
        ChangeError::Failed(format!(
            "Could not write the action log, nothing changed: {e}"
        ))
    })?;
    let script = format!(
        "tell application \"System Events\" to delete login item {}",
        as_quote(&item.name)
    );
    run_ok(env, OSASCRIPT, &["-e", &script])
        .map_err(|e| ChangeError::Failed(format!("System Events did not remove it: {e}")))?;
    let undo = format!("{OSASCRIPT} -e {}", sh_quote(&add_login_item_script(&path)));
    log(
        env,
        new_record(
            ActionKind::Toggled {
                what: format!("{LOGIN_REMOVED_PREFIX}{} ({path})", item.name),
                undo_command: Some(undo),
            },
            "login",
        ),
    )
}

/// Login items CleanCraft removed and that are still gone: (journal action id, item).
pub fn removed_login_items(env: &Env, current: &[LoginItem]) -> Vec<(String, LoginItem)> {
    let mut v: Vec<(String, LoginItem)> = Vec::new();
    for r in executor::history(env) {
        if r.feature != Some(FeatureId::LoginItems) || !r.can_undo() {
            continue;
        }
        let ActionKind::Toggled { what, .. } = &r.kind else {
            continue;
        };
        let Some(rest) = what.strip_prefix(LOGIN_REMOVED_PREFIX) else {
            continue;
        };
        let Some((name, path)) = rest.rsplit_once(" (") else {
            continue;
        };
        let path = path.trim_end_matches(')').to_string();
        let back_already = current
            .iter()
            .any(|c| c.path.as_deref() == Some(path.as_str()));
        if back_already {
            continue;
        }
        v.retain(|(_, it)| it.path.as_deref() != Some(path.as_str()));
        v.push((
            r.id.clone(),
            LoginItem {
                name: name.to_string(),
                path: Some(path),
            },
        ));
    }
    v
}

// ---------------------------------------------------------------------------------------------
// Heavy apps

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Proc {
    pub pid: u32,
    pub uid: u32,
    pub cpu: f32,
    pub mem: f32,
    /// Executable path as `ps` prints it.
    pub comm: String,
    pub name: String,
    /// The `.app` bundle, as a macOS path.
    pub app: Option<String>,
    pub bundle_id: Option<String>,
    /// Part of macOS: never quit by CleanCraft.
    pub system: bool,
    /// CleanCraft itself.
    pub is_self: bool,
}

/// CPU percent from which an app counts as heavy.
pub const HEAVY_CPU: f32 = 30.0;
/// Memory percent from which an app counts as heavy.
pub const HEAVY_MEM: f32 = 10.0;

pub const PS_ARGS: &[&str] = &["-axo", "pid=,uid=,%cpu=,%mem=,comm="];

/// `ps -axo pid=,uid=,%cpu=,%mem=,comm=` lines: "  512   501  85.2  4.3 /Applications/...".
pub fn parse_ps_usage(stdout: &str) -> Vec<Proc> {
    stdout
        .lines()
        .filter_map(|l| {
            let mut rest = l.trim_start();
            let mut next = || -> Option<&str> {
                let end = rest.find(char::is_whitespace)?;
                let (a, b) = rest.split_at(end);
                rest = b.trim_start();
                Some(a)
            };
            let pid = next()?.parse().ok()?;
            let uid = next()?.parse().ok()?;
            let cpu = next()?.replace(',', ".").parse().ok()?;
            let mem = next()?.replace(',', ".").parse().ok()?;
            let comm = rest.trim().to_string();
            if comm.is_empty() {
                return None;
            }
            let app = crate::sys::app_bundle_of(Path::new(&comm))
                .map(|p| p.to_string_lossy().into_owned());
            let name = app
                .as_deref()
                .and_then(|a| Path::new(a).file_stem())
                .or_else(|| Path::new(&comm).file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| comm.clone());
            Some(Proc {
                pid,
                uid,
                cpu,
                mem,
                system: is_system_program(&comm),
                is_self: false,
                name,
                app,
                bundle_id: None,
                comm,
            })
        })
        .collect()
}

/// Programs that belong to macOS.
pub fn is_system_program(comm: &str) -> bool {
    !comm.starts_with('/')
        || [
            "/System/",
            "/usr/",
            "/bin/",
            "/sbin/",
            "/Library/Apple/",
            "/private/",
            "/Library/Developer/CommandLineTools/",
        ]
        .iter()
        .any(|p| comm.starts_with(p))
}

fn is_cleancraft(p: &Proc) -> bool {
    p.pid == std::process::id()
        || p.name.eq_ignore_ascii_case("cleancraft")
        || p.app
            .as_deref()
            .is_some_and(|a| a.to_lowercase().ends_with("/cleancraft.app"))
}

fn bundle_id_of(env: &Env, app: &str) -> Option<String> {
    let v = plist::Value::from_file(env.sys_path(app).join("Contents/Info.plist")).ok()?;
    v.as_dictionary()?
        .get("CFBundleIdentifier")?
        .as_string()
        .map(str::to_string)
}

/// This user's processes, heaviest first.
pub fn list_processes(env: &Env, uid: u32) -> Result<Vec<Proc>, String> {
    let out = env
        .sys
        .run("/bin/ps", PS_ARGS)
        .map_err(|e| format!("Could not list running apps: {e}"))?;
    if !out.ok() {
        return Err(format!(
            "Could not list running apps: {}",
            tasks::first_lines(&out.stderr, 1)
        ));
    }
    let mut v: Vec<Proc> = parse_ps_usage(&out.stdout)
        .into_iter()
        .filter(|p| p.uid == uid)
        .map(|mut p| {
            p.is_self = is_cleancraft(&p);
            if let Some(app) = &p.app {
                p.bundle_id = bundle_id_of(env, app);
            }
            p
        })
        .collect();
    v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    Ok(v)
}

/// The apps worth showing: heavy, quittable, heaviest first.
pub fn heavy_apps(procs: &[Proc]) -> Vec<Proc> {
    procs
        .iter()
        .filter(|p| !p.system && !p.is_self && (p.cpu >= HEAVY_CPU || p.mem >= HEAVY_MEM))
        .cloned()
        .collect()
}

/// The command CleanCraft would use to quit `p`.
pub fn quit_command(p: &Proc) -> (String, Vec<String>) {
    match &p.bundle_id {
        Some(id) => (
            OSASCRIPT.to_string(),
            vec![
                "-e".into(),
                format!("tell application id {} to quit", as_quote(id)),
            ],
        ),
        None => (
            "/bin/kill".to_string(),
            vec!["-TERM".into(), p.pid.to_string()],
        ),
    }
}

/// Quit one app of this user, after checking again that the same program still has that pid.
pub fn quit_process(env: &Env, p: &Proc, uid: u32) -> Result<ActionRecord, ChangeError> {
    if p.is_self || is_cleancraft(p) {
        return Err(ChangeError::Refused(
            "CleanCraft does not quit itself.".into(),
        ));
    }
    if p.system {
        return Err(ChangeError::Refused(
            "This is part of macOS; CleanCraft does not quit it.".into(),
        ));
    }
    if p.uid != uid {
        return Err(ChangeError::Refused(
            "This process belongs to another user.".into(),
        ));
    }
    journal_writable(env).map_err(|e| {
        ChangeError::Failed(format!(
            "Could not write the action log, nothing changed: {e}"
        ))
    })?;
    // The pid could belong to another program by now.
    let now = list_processes(env, uid).map_err(ChangeError::Failed)?;
    if !now.iter().any(|q| q.pid == p.pid && q.comm == p.comm) {
        return Err(ChangeError::Failed(format!(
            "{} is no longer running.",
            p.name
        )));
    }
    let (prog, args) = quit_command(p);
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = env
        .sys
        .run(&prog, &a)
        .map_err(|e| ChangeError::Failed(format!("Could not quit {}: {e}", p.name)))?;
    let shown = std::iter::once(prog.clone())
        .chain(args.iter().map(|x| tasks::sh_quote_if_needed(x)))
        .collect::<Vec<_>>()
        .join(" ");
    let rec = new_record(
        ActionKind::Command {
            command: shown,
            ok: out.ok(),
            output: tasks::first_lines(&format!("{}\n{}", out.stdout, out.stderr), 5),
        },
        "quit",
    );
    let rec = log(env, rec)?;
    if out.ok() {
        Ok(rec)
    } else {
        Err(ChangeError::Failed(format!(
            "{} did not quit: {}",
            p.name,
            tasks::first_lines(&out.stderr, 1)
        )))
    }
}

// ---------------------------------------------------------------------------------------------
// Removing an agent's property list (plan-based)

/// Startup Items as a plan feature: removing launch agent property lists. Nothing is preselected.
pub struct StartupFeature;

pub fn domain_of(env: &Env, plist: &Path) -> Option<Domain> {
    Domain::ALL
        .into_iter()
        .find(|d| plist.parent() == Some(d.path(env).as_path()))
}

/// The `Label` of a launch item's property list, else its file name without `.plist`.
fn plist_label(path: &Path) -> Option<String> {
    plist::Value::from_file(path)
        .ok()
        .and_then(|v| {
            v.as_dictionary()?
                .get("Label")?
                .as_string()
                .map(str::to_string)
        })
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
}

/// Stop the launch agents whose property list really moved (REL-6: an agent whose file was
/// skipped is left running and untouched). Runs after the move, by label:
/// `launchctl bootout gui/<uid>/<label>`. Each stop is logged; a stop that fails because the
/// agent was not running is fine. A daemon is stopped earlier, inside the one administrator
/// command that moved its property list ([`daemon_stops`]), so the person sees one password
/// dialog; here its line is written to the journal.
pub fn bootout_moved(
    env: &Env,
    report: &executor::Report,
    feature: FeatureId,
) -> anyhow::Result<()> {
    let moved: Vec<(PathBuf, PathBuf)> = report
        .moved
        .iter()
        .filter_map(|r| match &r.kind {
            ActionKind::Moved {
                original, stored, ..
            } => Some((original.clone(), stored.clone())),
            _ => None,
        })
        .filter(|(original, _)| {
            original.extension().is_some_and(|e| e == "plist")
                && matches!(
                    domain_of(env, original),
                    Some(Domain::UserAgent | Domain::GlobalAgent)
                )
        })
        .collect();
    // Daemons were stopped by the administrator command that moved them: their line only.
    let daemons: Vec<String> = report
        .moved
        .iter()
        .filter_map(|r| match &r.kind {
            ActionKind::Moved {
                original, stored, ..
            } => Some((original.clone(), stored.clone())),
            _ => None,
        })
        .filter(|(original, _)| is_daemon_plist(env, original))
        .filter_map(|(_, stored)| plist_label(&stored))
        .filter(|l| safe_label(l))
        .collect();
    if !daemons.is_empty() {
        journal_writable(env)?;
        for label in daemons {
            let target = format!("system/{label}");
            let mut rec = new_record(
                ActionKind::Command {
                    command: admin_wrap(&format!(
                        "{LAUNCHCTL} bootout {} || true",
                        sh_quote(&target)
                    )),
                    ok: true,
                    output: DAEMON_STOPPED.to_string(),
                },
                "bootout",
            );
            rec.feature = Some(feature);
            executor::append_journal(env, &rec)?;
        }
    }
    if moved.is_empty() {
        return Ok(());
    }
    let uid = tasks::current_uid(env).map_err(|e| anyhow::anyhow!(e))?;
    journal_writable(env)?;
    for (original, stored) in moved {
        let Some(label) = plist_label(&stored) else {
            continue;
        };
        let target = format!("gui/{uid}/{label}");
        let (ok, output) = match env.sys.run(LAUNCHCTL, &["bootout", &target]) {
            Ok(out) => (
                out.ok(),
                tasks::first_lines(&format!("{}\n{}", out.stdout, out.stderr), 3),
            ),
            Err(e) => (false, e.to_string()),
        };
        let mut rec = new_record(
            ActionKind::Command {
                command: format!("{LAUNCHCTL} bootout {}", sh_quote(&target)),
                ok,
                output: if ok {
                    format!("stopped after {} moved", env.display(&original))
                } else {
                    format!("not running ({output})")
                },
            },
            "bootout",
        );
        rec.feature = Some(feature);
        executor::append_journal(env, &rec)?;
    }
    Ok(())
}

/// A launchd label CleanCraft passes to an administrator command: letters, digits, `.`, `-`
/// and `_` only.
fn safe_label(l: &str) -> bool {
    !l.is_empty()
        && l.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Journal output of the daemons stopped inside the administrator move command.
pub const DAEMON_STOPPED: &str = "stopped with the same administrator command that moved its file (a daemon that was not loaded was left as it was)";

/// The launchd targets of the selected items that are daemon property lists: what
/// [`crate::features::Feature::admin_stops`] gives the executor, so each daemon is stopped by
/// the same administrator command that moves its file. Shared by Startup Items and Threat
/// Check. A file that is not a daemon property list, or whose label is not a plain launchd
/// name, is left out.
pub fn daemon_stops(env: &Env, items: &[&crate::plan::Item]) -> Vec<String> {
    items
        .iter()
        .filter(|i| is_daemon_plist(env, &i.path))
        .filter_map(|i| plist_label(&i.path))
        .filter(|l| safe_label(l))
        .map(|l| format!("system/{l}"))
        .collect()
}

/// A property list directly in `/Library/LaunchDaemons`.
fn is_daemon_plist(env: &Env, p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "plist") && domain_of(env, p) == Some(Domain::Daemon)
}

impl Feature for StartupFeature {
    fn id(&self) -> FeatureId {
        FeatureId::LoginItems
    }

    fn allow_roots(&self, env: &Env) -> Vec<PathBuf> {
        Domain::ALL.iter().map(|d| d.path(env)).collect()
    }

    fn scan(&self, ctx: &ScanCtx<'_>) -> anyhow::Result<Plan> {
        let env = ctx.env;
        let allow = self.allow_roots(env);
        let mut plan = Plan::new(FeatureId::LoginItems);
        let (items, notes) = list_launch_items(env, None);
        plan.notes = notes
            .into_iter()
            .filter(|n| !n.contains("turned off"))
            .collect();
        for d in Domain::ALL {
            plan.add_category(d.id(), d.title(), d.description(), Risk::Medium);
        }
        for it in items {
            if it.apple || !ctx.allowed(&allow, &it.plist) {
                continue;
            }
            (ctx.progress)(&it.plist_shown);
            // An empty list (old Google Keystone ones on a real Mac) names no program at all.
            let reason = match &it.program {
                Some(p) if it.program_exists => {
                    format!("{}: runs {p} ({})", it.label, it.signer.label())
                }
                Some(p) => format!("{}: its program {p} is missing", it.label),
                None => format!("{}: names no program, so it runs nothing", it.label),
            };
            let id = plan.push(
                it.plist.clone(),
                tree_size(&it.plist).0,
                it.domain.id(),
                reason,
                Risk::Medium,
                false,
            );
            // Folders of the system (`/Library/LaunchAgents`, `/Library/LaunchDaemons`): the
            // file moves to the quarantine with the administrator password, after the user
            // selected it by hand (nothing here is preselected).
            if !it.plist.parent().is_some_and(|dir| env.can_modify(dir)) {
                plan.mark_admin(id);
            }
        }
        Ok(plan)
    }

    fn allows_admin(&self) -> bool {
        true
    }

    fn admin_stops(&self, env: &Env, items: &[&crate::plan::Item]) -> Vec<String> {
        daemon_stops(env, items)
    }

    /// A saved plan can be edited, and an agent can now select any item of it: every selected
    /// file must still be a property list directly in one of the three folders, and never one
    /// whose program macOS signed.
    fn validate(
        &self,
        env: &Env,
        _settings: &crate::settings::Settings,
        _plan: &Plan,
        item: &crate::plan::Item,
    ) -> Result<(), String> {
        let Some(d) = domain_of(env, &item.path) else {
            return Err("it is not directly in a launch agents or daemons folder".into());
        };
        let is_plist = item.path.extension().is_some_and(|e| e == "plist")
            && fs::symlink_metadata(&item.path).is_ok_and(|m| m.is_file());
        if !is_plist {
            return Err("it is not a launch property list".into());
        }
        if launch_item_at(env, d, &item.path, &BTreeMap::new()).apple {
            return Err("it is part of macOS".into());
        }
        Ok(())
    }

    fn finish(&self, env: &Env, _plan: &Plan, report: &executor::Report) -> anyhow::Result<()> {
        bootout_moved(env, report, FeatureId::LoginItems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(status: i32, stdout: &str, stderr: &str) -> CmdOut {
        CmdOut {
            status,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn codesign_outputs() {
        let dev = "Executable=/Applications/Dropbox.app/Contents/MacOS/Dropbox\nIdentifier=com.getdropbox.dropbox\nAuthority=Developer ID Application: Dropbox, Inc. (G7HH3F8CAK)\nAuthority=Developer ID Certification Authority\nAuthority=Apple Root CA\nTeamIdentifier=G7HH3F8CAK\n";
        assert_eq!(
            parse_codesign(&out(0, "", dev)),
            Signer::Developer {
                authority: "Developer ID Application: Dropbox, Inc. (G7HH3F8CAK)".into(),
                team: Some("G7HH3F8CAK".into())
            }
        );
        let apple = "Executable=/usr/libexec/x\nAuthority=Software Signing\nAuthority=Apple Code Signing Certification Authority\nAuthority=Apple Root CA\nTeamIdentifier=not set\n";
        assert_eq!(parse_codesign(&out(0, "", apple)), Signer::Apple);
        let adhoc = "Executable=/tmp/x\nCodeDirectory v=20400 size=200 flags=0x2(adhoc) hashes=1+0 location=embedded\nSignature=adhoc\nTeamIdentifier=not set\n";
        assert_eq!(parse_codesign(&out(0, "", adhoc)), Signer::AdHoc);
        assert_eq!(
            parse_codesign(&out(1, "", "/tmp/x: code object is not signed at all\n")),
            Signer::Unsigned
        );
    }

    #[test]
    fn print_disabled_both_formats() {
        let new = "disabled services = {\n\t\"com.example.a\" => disabled\n\t\"com.example.b\" => enabled\n}\n\nlogin item associations = {\n\t\"com.example.c\" => disabled\n}\n";
        let m = parse_print_disabled(new);
        assert_eq!(m.get("com.example.a"), Some(&true));
        assert_eq!(m.get("com.example.b"), Some(&false));
        assert!(!m.contains_key("com.example.c"));
        let old =
            "disabled services = {\n\t\"com.example.a\" => true\n\t\"com.example.b\" => false\n}\n";
        let m = parse_print_disabled(old);
        assert_eq!(m.get("com.example.a"), Some(&true));
        assert_eq!(m.get("com.example.b"), Some(&false));
    }

    #[test]
    fn ps_lines() {
        let s = "    1     0   0.3  0.1 /sbin/launchd\n  812   501  85.2  4.3 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome\n  900   501   1,5  0,2 /usr/bin/ssh\n";
        let v = parse_ps_usage(s);
        assert_eq!(v.len(), 3);
        assert_eq!(v[1].name, "Google Chrome");
        assert_eq!(v[1].app.as_deref(), Some("/Applications/Google Chrome.app"));
        assert!((v[1].cpu - 85.2).abs() < 0.01);
        assert!(v[0].system && v[2].system && !v[1].system);
        assert!((v[2].cpu - 1.5).abs() < 0.01);
    }

    #[test]
    fn quoting_for_admin_undo() {
        let w = admin_wrap(
            "launchctl enable 'system/com.x' && { launchctl bootstrap system '/Library/LaunchDaemons/a b.plist' || true; }",
        );
        assert!(w.starts_with("/usr/bin/osascript -e '"));
        assert!(w.contains("with administrator privileges"));
        // Single quotes inside are closed and reopened for the shell.
        assert!(w.contains("'\\''system/com.x'\\''"));
        assert_eq!(as_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn login_item_lines() {
        let v = parse_login_items(
            "Dropbox\t/Applications/Dropbox.app\nOdd, Name\t\nX\tmissing value\n",
        );
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].path.as_deref(), Some("/Applications/Dropbox.app"));
        assert_eq!(v[1].name, "Odd, Name");
        assert_eq!(v[1].path, None);
        assert_eq!(v[2].path, None);
    }
}
