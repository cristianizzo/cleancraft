//! The command line. Same engine, same safety: `scan` saves a plan and prints it; `clean`
//! carries out a saved plan only with `--yes` after printing the exact list.

use clap::{Parser, Subcommand};
use cleancraft_core::executor;
use cleancraft_core::features;
use cleancraft_core::fsutil::human_size;
use cleancraft_core::permissions;
use cleancraft_core::plans;
use cleancraft_core::{Env, FeatureId, Settings};

#[derive(Parser)]
#[command(
    name = "cleancraft",
    version,
    about = "Clean and look after your Mac. Nothing is deleted: items move to the Trash and can be put back."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// List features and their ids.
    Features,
    /// Scan a feature (looks only) and save the plan.
    Scan {
        /// Feature id, e.g. system_junk.
        feature: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
        /// Threat Check only: quick, deep (reads programs with Apple's malware rules) or full
        /// (the whole startup disk).
        #[arg(long, default_value = "quick", value_parser = ["quick", "deep", "full"])]
        depth: String,
    },
    /// Move the selected items of a saved plan to the Trash (or quarantine).
    Clean {
        plan_id: String,
        /// Confirm the list printed.
        #[arg(long)]
        yes: bool,
    },
    /// Put back one action (id with #) or a whole cleanup (plan id).
    Undo { id: String },
    /// What CleanCraft did.
    History {
        #[arg(long)]
        json: bool,
    },
    /// Full Disk Access status.
    Access,
    /// Live reading: CPU, memory, disk, Trash, battery, network.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Quick Sweep: scan every cleanup module (looks only) and save their plans.
    Sweep,
    /// List Tune-up Tasks with their exact commands.
    Tasks,
    /// Run one Tune-up Task (prints the exact commands; runs only with --yes).
    RunTask {
        id: String,
        /// Confirm the commands printed.
        #[arg(long)]
        yes: bool,
    },
    /// List login items, launch agents and daemons, and heavy apps.
    Startup,
    /// Run the MCP server on stdio (what AI agents launch).
    Serve,
    /// Run the menu bar agent (live CPU and memory, scheduled scans, notifications). Started
    /// at login when Settings > "Show CleanCraft in the menu bar" is on. It never cleans.
    Agent,
    /// The menu bar panel the agent opens under its icon (internal: started by the agent,
    /// which reads its commands on stdout and closes it by closing stdin).
    #[command(hide = true)]
    Panel {
        /// Top-left corner in points, "x,y".
        #[arg(long, allow_hyphen_values = true)]
        at: String,
    },
    /// Undo of "Connect AI Apps": `mcp-setup remove <config file> [--drop-empty]`.
    #[command(name = "mcp-setup")]
    McpSetup {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Open the app (bringing it to the front when it runs) on a page: quick_sweep,
    /// scan:quick_sweep, system_junk, history, settings... or a cleancraft:// link.
    Open {
        /// Page or cleancraft:// link.
        route: Option<String>,
        /// The page, as an option.
        #[arg(long)]
        page: Option<String>,
    },
    /// CleanCraft's own update: --check (default) looks on GitHub Releases, --install
    /// installs the newer version (the old one goes to the Trash, with Undo in History).
    Update(crate::cli_update::UpdateArgs),
    #[command(flatten)]
    Apps(crate::cli_apps::AppsCmd),
}

pub fn run(args: Vec<String>) -> i32 {
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { 2 } else { 0 };
        }
    };
    let env = match Env::real() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    match run_cmd(&env, cli.cmd) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e:#}");
            1
        }
    }
}

/// The route `cleancraft open` asks for: a cleancraft:// link is read, a page name is kept,
/// nothing means just open the app.
pub fn open_route(arg: Option<&str>) -> anyhow::Result<String> {
    let Some(a) = arg.map(str::trim) else {
        return Ok(String::new());
    };
    if a.contains("://") || a.to_lowercase().starts_with("cleancraft:") {
        return cleancraft_core::notify::route_from_url(a)
            .ok_or_else(|| anyhow::anyhow!("not a CleanCraft link: {a}"));
    }
    if !cleancraft_core::notify::valid_route(a) {
        anyhow::bail!("not a page name: {a}");
    }
    Ok(a.to_string())
}

pub fn run_cmd(env: &Env, cmd: Cmd) -> anyhow::Result<()> {
    let (settings, warning) = Settings::load_checked(env);
    if let Some(w) = warning {
        eprintln!("Warning: {w}");
    }
    match cmd {
        Cmd::Features => {
            let scannable: Vec<FeatureId> = features::registry().iter().map(|f| f.id()).collect();
            for f in FeatureId::ALL {
                println!(
                    "{:<18} {}{}",
                    f.as_str(),
                    f.title(),
                    if scannable.contains(&f) {
                        ""
                    } else {
                        "  (no scan)"
                    }
                );
            }
        }
        Cmd::Scan {
            feature,
            json,
            depth,
        } => {
            let f = FeatureId::parse(&feature).ok_or_else(|| {
                anyhow::anyhow!("unknown feature '{feature}' (see `cleancraft features`)")
            })?;
            let depth = serde_json::from_value(serde_json::Value::String(depth))?;
            let plan = scan_with_bar(env, &settings, f, depth, !json)?;
            plans::save(env, &plan)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                for it in &plan.items {
                    let mark = if it.locked.is_some() {
                        "-"
                    } else if it.selected {
                        "x"
                    } else {
                        " "
                    };
                    println!(
                        "[{mark}] {:>10}  {}  ({})",
                        human_size(it.size),
                        env.display(&it.path),
                        it.reason
                    );
                }
                for n in &plan.notes {
                    println!("note: {n}");
                }
                println!("\n{}", plan.summary());
                println!(
                    "Plan {}. To move the selected items to the Trash: cleancraft clean {} --yes",
                    plan.id, plan.id
                );
            }
        }
        Cmd::Clean { plan_id, yes } => {
            let plan = plans::load(env, &plan_id)?;
            for it in plan.selected() {
                println!("{:>10}  {}", human_size(it.size), env.display(&it.path));
            }
            println!("{}", plan.summary());
            if !yes {
                anyhow::bail!("nothing moved: add --yes to confirm this list");
            }
            let rep = features::run_plan(env, &settings, &plan, &plan.digest(), &mut |_, _| {})?;
            println!(
                "{} ({}).",
                cleancraft_core::features::moved_text([&rep]),
                cleancraft_core::fsutil::items(rep.moved.len())
            );
            for (p, why) in &rep.skipped {
                println!("skipped {}: {why}", env.display(p));
            }
            println!("Undo with: cleancraft undo {}", rep.plan_id);
        }
        Cmd::Undo { id } => {
            if id.contains('#') {
                executor::undo(env, &id).map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("Put back.");
            } else {
                let (n, errs) = executor::undo_plan(env, &id);
                println!("{} put back.", cleancraft_core::fsutil::items(n));
                for (id, e) in errs {
                    println!("{id}: {e}");
                }
            }
        }
        Cmd::History { json } => {
            let h = executor::history(env);
            if json {
                println!("{}", serde_json::to_string_pretty(&h)?);
            } else {
                for r in h {
                    println!(
                        "{}  {}  {}{}",
                        r.id,
                        human_size(r.size),
                        serde_json::to_string(&r.kind)?,
                        if r.undone.is_some() { "  (undone)" } else { "" }
                    );
                }
            }
        }
        Cmd::Access => {
            println!("{:?}", permissions::full_disk_access(env));
        }
        Cmd::Status { json } => {
            println!(
                "{}",
                crate::cli_monitor::status(env, json, std::time::Duration::from_secs(1))?
            );
        }
        Cmd::Sweep => println!("{}", crate::cli_monitor::sweep(env, &settings)?),
        Cmd::Apps(c) => crate::cli_apps::run(env, c, &mut std::io::stdout())?,
        Cmd::Tasks => crate::cli_perf::tasks(env, &mut std::io::stdout())?,
        Cmd::RunTask { id, yes } => {
            crate::cli_perf::run_task(env, &id, yes, &mut std::io::stdout())?
        }
        Cmd::Startup => crate::cli_perf::startup(env, &mut std::io::stdout())?,
        Cmd::Agent => crate::agent::run(env)?,
        Cmd::Panel { at } => crate::panel::run(env, &at)?,
        Cmd::Update(a) => crate::cli_update::run(
            env,
            &a,
            cleancraft_core::self_update::CURRENT,
            &mut std::io::stdout(),
        )?,
        Cmd::McpSetup { args } => {
            let code = cleancraft_core::mcp_setup::cli(env, &args);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Cmd::Open { route, page } => {
            let route = open_route(route.or(page).as_deref())?;
            cleancraft_core::notify::request_open(env, &route).map_err(anyhow::Error::msg)?;
        }
        Cmd::Serve => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                use rmcp::ServiceExt;
                let service = cleancraft_mcp::CleanCraftMcp::new(env.clone())
                    .serve(rmcp::transport::stdio())
                    .await?;
                service.waiting().await?;
                anyhow::Ok(())
            })?;
        }
    }
    Ok(())
}

/// Scan `f`; with `bar` and a terminal on stderr, a text progress bar is drawn there while
/// it runs and cleared at the end (the list on stdout stays clean for pipes).
fn scan_with_bar(
    env: &Env,
    settings: &Settings,
    f: FeatureId,
    depth: features::malware::ThreatDepth,
    bar: bool,
) -> anyhow::Result<cleancraft_core::Plan> {
    use std::io::{IsTerminal, Write};
    let meter = cleancraft_core::progress::Progress::new();
    if !(bar && std::io::stderr().is_terminal()) {
        return features::scan_metered(env, settings, f, depth, &meter);
    }
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut tick = 0usize;
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let line = cleancraft_core::progress::text_bar(&meter, 30, tick);
                let line: String = line.chars().take(100).collect();
                eprint!("\r\x1b[2K{line}");
                let _ = std::io::stderr().flush();
                tick += 1;
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        });
        let r = features::scan_metered(env, settings, f, depth, &meter);
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        r
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use cleancraft_core::testkit::FakeMac;

    #[test]
    fn open_routes_from_names_and_links() {
        assert_eq!(open_route(None).ok().as_deref(), Some(""));
        assert_eq!(
            open_route(Some("quick-sweep")).ok().as_deref(),
            Some("quick-sweep")
        );
        assert_eq!(
            open_route(Some("scan:quick_sweep")).ok().as_deref(),
            Some("scan:quick_sweep")
        );
        assert_eq!(
            open_route(Some("cleancraft://open/history"))
                .ok()
                .as_deref(),
            Some("history")
        );
        assert!(open_route(Some("https://example.com/x")).is_err());
        assert!(open_route(Some("a\nb")).is_err());
    }

    #[test]
    fn cleancraft_open_page_writes_the_request_and_opens_the_app() {
        let m = FakeMac::new();
        let fake = m.env.fake_sys().expect("fake");
        fake.answer("open", "");
        let cli =
            Cli::try_parse_from(["cleancraft", "open", "--page", "quick-sweep"]).expect("args");
        run_cmd(&m.env, cli.cmd).expect("open");
        assert_eq!(
            fake.calls(),
            vec!["/usr/bin/open -b com.cristianizzo.cleancraft".to_string()]
        );
        let mut inbox = cleancraft_core::notify::OpenInbox::default();
        assert_eq!(
            inbox.poll(&m.env, chrono_now()).as_deref(),
            Some("quick-sweep")
        );
        // The agent subcommand refuses while the menu bar is off.
        let cli = Cli::try_parse_from(["cleancraft", "agent"]).expect("args");
        let e = run_cmd(&m.env, cli.cmd)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(e.starts_with("The menu bar is off."), "{e}");
    }

    #[test]
    fn mcp_setup_passes_its_arguments_through() {
        let cli = Cli::try_parse_from([
            "cleancraft",
            "mcp-setup",
            "remove",
            "/Users/me/x.json",
            "--drop-empty",
        ])
        .expect("args");
        match cli.cmd {
            Cmd::McpSetup { args } => {
                assert_eq!(args, ["remove", "/Users/me/x.json", "--drop-empty"])
            }
            _ => panic!("not mcp-setup"),
        }
    }

    fn chrono_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}
