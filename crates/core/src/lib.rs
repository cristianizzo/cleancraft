//! CleanCraft engine: everything the app, the CLI and the MCP server do, without UI.
//!
//! Safety model, in order: [`env`] decides where things are (a temporary fake Mac in tests),
//! [`policy`] is the fence (allow-lists, protected locations), [`plan`] is the preview the user
//! confirms, [`executor`] moves confirmed items to the Trash or quarantine and logs each move
//! with an undo.

pub mod app_icons;
pub mod app_quit;
pub mod apps;
pub mod env;
pub mod executor;
pub mod extensions;
pub mod features;
pub mod fsutil;
pub mod health;
pub mod i18n;
pub mod known_malware;
pub mod login_agent;
pub mod malware_rules;
pub mod mcp_setup;
pub mod menubar;
pub mod monitor;
pub mod notify;
pub mod permissions;
pub mod plan;
pub mod plans;
pub mod policy;
pub mod proc_names;
pub mod progress;
pub mod route;
pub mod schedule;
pub mod self_update;
pub mod settings;
pub mod space;
pub mod space_history;
pub mod space_snapshots;
pub mod startup;
pub mod sweep;
pub mod sys;
pub mod tasks;
pub mod testkit;
pub mod testkit_apps;
pub mod testkit_cleanup;
pub mod testkit_files;
pub mod testkit_health;
pub mod testkit_icons;
pub mod testkit_monitor;
pub mod testkit_perf;
pub mod testkit_quit;
pub mod testkit_trash;
pub mod testkit_update;
pub mod updater;
pub mod watch;
pub mod xprotect;

pub use env::{Env, Fault};
pub use executor::{ActionKind, ActionRecord, Method, Report, SkipReason};
pub use plan::{FeatureId, Item, Plan, Risk};
pub use settings::Settings;
