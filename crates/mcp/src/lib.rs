//! MCP server: CleanCraft for AI agents, over stdio.
//!
//! Agents can scan, select and preview freely. Every tool that changes the Mac needs the
//! setting "Let AI assistants clean after you confirm" (off by default, set in the app; no
//! tool can turn it on), and plans need the digest of the previewed selection, which proves
//! the agent acts on exactly the list it showed. Removal is a move to the Trash or quarantine,
//! logged, undoable.

pub mod perf;

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use cleancraft_core::executor;
use cleancraft_core::features::{self, uninstaller};
use cleancraft_core::fsutil::human_size;
use cleancraft_core::permissions;
use cleancraft_core::plans;
use cleancraft_core::{Env, FeatureId, Settings};

pub mod apps;
pub mod control;
pub mod monitor;

pub type ToolResult = Result<String, String>;

pub fn ok(v: impl Serialize) -> ToolResult {
    serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
}

const INSTRUCTIONS: &str = "CleanCraft cleans and maintains a Mac safely. Start with list_actions (everything you can do) and agent_permissions (whether the user lets agents change the Mac). Cleanups: scan(feature) returns a plan with plan_id, items (sizes, reasons) and a digest; select_items picks exactly the items the user wants (also ones not ticked by default) and returns the preview with a new digest. Always show the user the preview and get a clear yes before execute_plan(plan_id, digest). Items that need administrator rights make macOS show its password or Touch ID dialog on the Mac; the user approves it there and CleanCraft never takes a password. Nothing is deleted: items move to the Trash or CleanCraft's quarantine and undo puts them back. Every changing tool is refused unless the user turned on 'Let AI assistants clean after you confirm' in CleanCraft Settings; you cannot turn it on.";

/// The name of the setting that lets agents change the Mac, as Settings shows it.
pub const SWITCH_NAME: &str = "Let AI assistants clean after you confirm";

/// The answer of every changing tool while that setting is off.
pub const REFUSED: &str = "Refused: the CleanCraft setting 'Let AI assistants clean after you confirm' (Settings, AI assistants) is off, so agents may not change the Mac. Nothing changed. Ask the user to do this in the CleanCraft app, or to turn that setting on there. No tool can turn it on.";

/// The gate of every tool that changes the Mac: the settings when the user lets agents act.
pub fn gate(env: &Env) -> Result<Settings, String> {
    let (s, _) = Settings::load_checked(env);
    if s.agents_may_clean {
        Ok(s)
    } else {
        Err(REFUSED.into())
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FeatureArg {
    /// Feature id from list_features, e.g. "system_junk".
    pub feature: String,
    /// Threat Check only: "quick" (default, seconds), "deep" (also reads the programs in the
    /// places malware lives with Apple's malware rules, minutes) or "full" (the whole startup
    /// disk, can take long).
    #[serde(default)]
    pub depth: features::malware::ThreatDepth,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlanArg {
    pub plan_id: String,
    /// First item to return (paging), default 0.
    pub offset: Option<usize>,
    /// How many items, default 200.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExecuteArgs {
    pub plan_id: String,
    /// The digest scan returned for this plan.
    pub digest: String,
    /// Optional: only these item ids, all from the plan's confirmed selection (the list the
    /// digest stands for); anything else is refused. Default: the whole confirmed selection.
    pub only_items: Option<Vec<usize>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UndoArgs {
    /// An action id from history, or a plan id to undo a whole cleanup.
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LimitArg {
    pub limit: Option<usize>,
}

#[derive(Clone)]
pub struct CleanCraftMcp {
    pub env: Env,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl CleanCraftMcp {
    pub fn new(env: Env) -> Self {
        Self {
            env,
            tool_router: Self::tool_router()
                + Self::monitor_router()
                + Self::apps_router()
                + Self::perf_router()
                + Self::control_router(),
        }
    }

    fn settings(&self) -> Settings {
        Settings::load(&self.env)
    }
}

fn plan_view(
    env: &Env,
    plan: &cleancraft_core::Plan,
    offset: usize,
    limit: usize,
) -> serde_json::Value {
    json!({
        "plan_id": plan.id,
        "feature": plan.feature,
        "digest": plan.digest(),
        "total": human_size(plan.total_size()),
        "total_bytes": plan.total_size(),
        "selected": human_size(plan.selected_size()),
        "selected_bytes": plan.selected_size(),
        "selected_count": plan.selected_count(),
        "item_count": plan.items.len(),
        "categories": plan.categories.iter().map(|c| json!({
            "id": c.id, "title": c.title, "risk": c.risk,
            "size": human_size(plan.category_size(&c.id)), "count": plan.category_count(&c.id)})).collect::<Vec<_>>(),
        "notes": plan.notes,
        "items": plan.items.iter().skip(offset).take(limit).map(|i| json!({
            "id": i.id, "path": env.display(&i.path), "size": human_size(i.size), "bytes": i.size,
            "category": i.category, "reason": i.reason, "risk": i.risk,
            "selected": i.selected, "locked": i.locked})).collect::<Vec<_>>(),
    })
}

/// `execute_plan`: the gate, the digest of the saved plan's selection, `only_items` (which can
/// only leave items out), then the one way every front end carries out a plan.
pub fn execute_plan_json(env: &Env, a: &ExecuteArgs) -> Result<serde_json::Value, String> {
    let settings = gate(env)?;
    let mut plan = plans::load(env, &a.plan_id).map_err(|e| e.to_string())?;
    if plan.digest() != a.digest {
        return Err("Refused: the digest does not match this plan's current selection. Call get_plan or select_items, show the user that list, and use the digest it returns. Nothing moved.".into());
    }
    // The digest above is the confirmed list; only_items may only leave items out of it.
    if let Some(only) = &a.only_items {
        features::narrow_selection(&mut plan, only)?;
    }
    let digest = plan.digest();
    let admin = features::admin_count(&plan);
    let journal_before = executor::history(env).len();
    let rep = features::run_plan(env, &settings, &plan, &digest, &mut |_, _| {})
        .map_err(|e| format!("{e:#}"))?;
    let mut v = control::report_json(env, &rep);
    // What ran besides the moves (stopping launch agents and daemons), from the journal.
    let ran: Vec<serde_json::Value> = executor::history(env)
        .into_iter()
        .skip(journal_before)
        .filter_map(|r| match r.kind {
            executor::ActionKind::Command {
                command,
                ok,
                output,
            } => Some(json!({"command": command, "ok": ok, "output": output})),
            _ => None,
        })
        .collect();
    if !ran.is_empty() {
        v["also_ran"] = json!(ran);
    }
    if admin > 0 {
        v["admin_note"] = json!(control::ADMIN_PROMPT);
    }
    if uninstaller::is_batch(&plan) {
        let apps: Vec<uninstaller::BatchApp> = uninstaller::batch_entries(env, &plan.id)
            .into_iter()
            .map(|e| uninstaller::BatchApp {
                group: uninstaller::group_key(&e.path),
                path: e.path,
                name: e.name,
                lock: None,
                running: false,
            })
            .collect();
        v["apps"] = json!(
            uninstaller::batch_results(&plan, &apps, &rep)
                .iter()
                .map(|r| json!({"name": r.name, "uninstalled": r.removed, "moved": r.moved.len(), "size": human_size(r.moved_size), "skipped": r.skipped.len()}))
                .collect::<Vec<_>>()
        );
    }
    Ok(v)
}

/// `update_status`: a fresh check when it works, else the last saved one with the error.
pub fn update_status_json(env: &Env, current: &str) -> serde_json::Value {
    use cleancraft_core::self_update::{self, Found, UpdateState};
    let (found, error) = match self_update::check_against(env, current) {
        Ok(f) => (Some(f), None),
        Err(e) => (UpdateState::load(env).found, Some(e)),
    };
    let notes = match &found {
        Some(Found::Available { release }) => release.notes.clone(),
        _ => String::new(),
    };
    json!({
        "current": current,
        "latest": found.as_ref().map(|f| f.latest().to_string()),
        "available": matches!(found, Some(Found::Available { .. })),
        "ahead": matches!(found, Some(Found::Ahead { .. })),
        "notes": notes,
        "error": error,
    })
}

#[tool_router]
impl CleanCraftMcp {
    #[tool(description = "List CleanCraft features with ids. Plan-based ones can be scanned.")]
    async fn list_features(&self) -> ToolResult {
        let scannable: Vec<FeatureId> = features::registry().iter().map(|f| f.id()).collect();
        ok(FeatureId::ALL
            .iter()
            .map(|f| json!({"id": f.as_str(), "title": f.title(), "scan": scannable.contains(f)}))
            .collect::<Vec<_>>())
    }

    #[tool(
        description = "Scan one feature (look only, nothing changes). Returns plan_id, digest, totals, categories and the first 200 items."
    )]
    async fn scan(&self, Parameters(a): Parameters<FeatureArg>) -> ToolResult {
        let f = FeatureId::parse(&a.feature)
            .ok_or_else(|| format!("unknown feature '{}'", a.feature))?;
        let (settings, warning) = Settings::load_checked(&self.env);
        let plan = features::scan_metered(
            &self.env,
            &settings,
            f,
            a.depth,
            &cleancraft_core::progress::Progress::new(),
        )
        .map_err(|e| format!("{e:#}"))?;
        plans::save(&self.env, &plan).map_err(|e| e.to_string())?;
        let mut v = plan_view(&self.env, &plan, 0, 200);
        if let Some(w) = warning {
            v["settings_warning"] = serde_json::Value::String(w);
        }
        ok(v)
    }

    #[tool(description = "Read a saved plan, with paging over its items.")]
    async fn get_plan(&self, Parameters(a): Parameters<PlanArg>) -> ToolResult {
        let plan = plans::load(&self.env, &a.plan_id).map_err(|e| e.to_string())?;
        ok(plan_view(
            &self.env,
            &plan,
            a.offset.unwrap_or(0),
            a.limit.unwrap_or(200),
        ))
    }

    #[tool(
        description = "Carry out a plan the user confirmed: moves the selected items to the Trash or quarantine (never deletes). Needs the digest of the preview the user said yes to (from scan, get_plan or select_items) and the CleanCraft setting that lets agents clean. Items that need administrator rights make macOS show its password or Touch ID dialog on the Mac; if the user cancels it those items are skipped. Returns moved, skipped with reasons, the size and undo_with."
    )]
    async fn execute_plan(&self, Parameters(a): Parameters<ExecuteArgs>) -> ToolResult {
        execute_plan_json(&self.env, &a).and_then(ok)
    }

    #[tool(
        description = "Undo one action (action id) or a whole cleanup (plan id, the undo_with of a result). Administrator moves show the macOS password dialog on the Mac again."
    )]
    async fn undo(&self, Parameters(a): Parameters<UndoArgs>) -> ToolResult {
        if executor::history(&self.env).iter().any(|r| r.id == a.id) {
            executor::undo(&self.env, &a.id)
                .map_err(|e| e.to_string())
                .and_then(|r| ok(json!({"undone": r.id})))
        } else {
            let (n, errs) = executor::undo_plan(&self.env, &a.id);
            ok(
                json!({"undone": n, "errors": errs.iter().map(|(id, e)| json!({"id": id, "error": e.to_string()})).collect::<Vec<_>>()}),
            )
        }
    }

    #[tool(description = "Recent CleanCraft actions, newest first.")]
    async fn history(&self, Parameters(a): Parameters<LimitArg>) -> ToolResult {
        let mut h = executor::history(&self.env);
        h.reverse();
        h.truncate(a.limit.unwrap_or(50));
        ok(h)
    }

    #[tool(
        description = "CleanCraft's own version and whether a newer release is on GitHub (looks only; installing is done in the app). Returns current, latest, available and the release notes."
    )]
    async fn update_status(&self) -> ToolResult {
        let current = cleancraft_core::self_update::CURRENT;
        ok(update_status_json(&self.env, current))
    }

    #[tool(
        description = "Whether CleanCraft has Full Disk Access (needed for Mail, Safari and Trash data)."
    )]
    async fn full_disk_access(&self) -> ToolResult {
        ok(json!({"access": format!("{:?}", permissions::full_disk_access(&self.env))}))
    }
}

#[tool_handler]
impl ServerHandler for CleanCraftMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("cleancraft", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod update_tests {
    use super::*;
    use cleancraft_core::testkit::FakeMac;
    use cleancraft_core::testkit_update::*;

    #[test]
    fn update_status_reads_only_and_says_what_is_available() {
        let m = FakeMac::new();
        install_app(&m, "0.1.2");
        answer_check(&m, &release_json("0.1.3", true, "Faster."));
        let v = update_status_json(&m.env, "0.1.2");
        assert_eq!(v["current"], "0.1.2");
        assert_eq!(v["latest"], "0.1.3");
        assert_eq!(v["available"], true);
        assert_eq!(v["notes"], "Faster.");
        let calls = m.env.fake_sys().map(|f| f.calls()).unwrap_or_default();
        assert_eq!(calls, vec![check_line()], "one look, nothing downloaded");
        assert_eq!(installed_version(&m).as_deref(), Some("0.1.2"));
        // Offline: the error, with what the last check found.
        fail_check(&m);
        let v = update_status_json(&m.env, "0.1.2");
        assert_eq!(v["available"], false);
        assert!(
            v["error"]
                .as_str()
                .is_some_and(|e| e.contains("no internet"))
        );
    }
}

#[cfg(test)]
mod control_tests;
