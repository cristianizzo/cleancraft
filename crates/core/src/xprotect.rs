//! Apple's XProtect signatures, for Threat Check's deep and full disk scans.
//!
//! The rules are read from the Mac at scan time, never copied into CleanCraft: macOS keeps
//! them up to date by itself (`XProtect.yara`, file signatures, and `XPScripts.yr`, script
//! behaviour rules). They are compiled with yara-x, each file in its own namespace (the two
//! files share a private rule name). Only files that can run are read: Mach-O (thin and fat),
//! scripts starting with `#!`, compiled AppleScript and Windows programs, since every Apple rule
//! is written for those. Links are never followed, files over [`MAX_BYTES`] are skipped, and so
//! are files whose content is not on this Mac (iCloud and other cloud placeholders), so a scan
//! never downloads anything.
//!
//! With an abuse.ch key ([`crate::known_malware`]) the same files, plus disk images and
//! installer packages, are also compared by SHA-256 with known Mac malware samples.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime};

use crate::env::Env;

/// Where macOS keeps the rules: current macOS, then older versions.
pub const DIRS: [&str; 2] = [
    "/Library/Apple/System/Library/CoreServices/XProtect.bundle/Contents/Resources",
    "/System/Library/CoreServices/XProtect.bundle/Contents/Resources",
];
/// The file signatures.
pub const FILE_RULES: &str = "XProtect.yara";
/// The script behaviour rules: more prone to false alarms, so their matches are never
/// selected for you.
pub const SCRIPT_RULES: &str = "XPScripts.yr";
/// Files bigger than this are not read (200 MB).
pub const MAX_BYTES: u64 = 200_000_000;
/// Threads reading files at once.
const READERS: usize = 6;
/// Longest time spent matching one file.
const FILE_TIMEOUT: Duration = Duration::from_secs(20);

/// The compiled rules and what could not be read.
pub struct Rules {
    rules: yara_x::Rules,
    /// Rules that can match (private helper rules aside).
    pub count: usize,
    /// Rule files that could not be read or compiled, in words.
    pub problems: Vec<String>,
    /// Known malware samples from abuse.ch: SHA-256 to family name (empty without a key).
    pub known: HashMap<[u8; 32], String>,
}

/// One rule that matched a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Apple's name for it ("MACOS.ADLOAD.I"), or the rule name when it has none.
    pub name: String,
    /// From the script behaviour rules.
    pub script: bool,
    /// An exact copy of a sample listed by abuse.ch (`name` is its family, maybe empty).
    pub listed: bool,
}

impl Hit {
    /// In words: "Adload (adware), Apple's rule MACOS.ADLOAD.I".
    pub fn describe(&self) -> String {
        if self.listed {
            return if self.name.is_empty() {
                "a known malware sample, listed by abuse.ch MalwareBazaar".into()
            } else {
                format!(
                    "a known malware sample ({}), listed by abuse.ch MalwareBazaar",
                    self.name
                )
            };
        }
        let parts: Vec<&str> = self
            .name
            .split(['.', '_'])
            .filter(|p| !p.is_empty() && !p.eq_ignore_ascii_case("xprotect"))
            .collect();
        let family = match parts.first().map(|p| p.to_ascii_uppercase()) {
            Some(p) if p == "MACOS" || p == "OSX" || p == "MULTI" => parts.get(1),
            _ => parts.first(),
        };
        let what = match family {
            Some(f) if f.eq_ignore_ascii_case("osascript") => {
                "a script that acts like known malware".to_string()
            }
            Some(f) => format!("{} ({})", title(f), kind(f)),
            None => "known malware".to_string(),
        };
        format!("{what}, Apple's rule {}", self.name)
    }
}

/// "ADLOAD" -> "Adload".
fn title(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut c = lower.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

/// What a well known family does, from public reports; "malware" for the others.
fn kind(family: &str) -> &'static str {
    match family.to_ascii_uppercase().as_str() {
        "ADLOAD" | "PIRRIT" | "BUNDLORE" | "GENIEO" | "VSEARCH" | "TROVI" | "MUGHTHESEC"
        | "SHLAYER" => "adware",
        "SOMA" | "BANSHEE" | "KEYSTEAL" => "password and wallet stealer",
        "PROTON" | "XAGENT" | "NETWIRE" | "LEVERAGE" | "HIDDENLOTUS" | "DOK" => "backdoor",
        "FINDZIP" => "ransomware",
        "MACDEFENDER" => "fake antivirus",
        _ => "malware",
    }
}

/// The folder holding the rules on this Mac, if any.
pub fn rules_dir(env: &Env) -> Option<PathBuf> {
    DIRS.iter()
        .map(|d| env.sys_path(d))
        .find(|d| d.join(FILE_RULES).is_file())
}

/// Apple's rules count as old after this many days (Apple ships new ones every few weeks).
pub const STALE_DAYS: u64 = 45;

/// Which rules this Mac has: Apple's version number and the date of the rules file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub version: Option<String>,
    pub built: Option<SystemTime>,
}

impl Version {
    /// Whole days since the rules were made.
    pub fn age_days(&self, now: SystemTime) -> Option<u64> {
        Some(now.duration_since(self.built?).ok()?.as_secs() / 86_400)
    }

    pub fn is_old(&self, now: SystemTime) -> bool {
        self.age_days(now).is_some_and(|d| d >= STALE_DAYS)
    }

    /// "Apple's malware rules: version 5368, 14 days old."
    pub fn text(&self, now: SystemTime) -> String {
        let v = self.version.as_deref().unwrap_or("?");
        match self.age_days(now) {
            Some(0) => format!("Apple's malware rules: version {v}, made today."),
            Some(1) => format!("Apple's malware rules: version {v}, 1 day old."),
            Some(d) => format!("Apple's malware rules: version {v}, {d} days old."),
            None => format!("Apple's malware rules: version {v}."),
        }
    }
}

/// The rules on this Mac, None when there are none.
pub fn version(env: &Env) -> Option<Version> {
    let dir = rules_dir(env)?;
    let version = dir
        .parent()
        .and_then(|c| plist::Value::from_file(c.join("Info.plist")).ok())
        .and_then(|v| {
            v.as_dictionary()?
                .get("CFBundleShortVersionString")?
                .as_string()
                .map(str::to_string)
        });
    let built = std::fs::metadata(dir.join(FILE_RULES))
        .and_then(|m| m.modified())
        .ok();
    Some(Version { version, built })
}

/// macOS's own XProtect tool (macOS 15 and later).
pub const TOOL: &str = "/usr/bin/xprotect";
/// System Settings > General > Software Update.
pub const SOFTWARE_UPDATE_URL: &str =
    "x-apple.systempreferences:com.apple.Software-Update-Settings.extension";

/// Ask macOS for Apple's newest rules: `xprotect update` with the administrator password, or
/// Software Update on older macOS. `Ok` says in words what happened.
pub fn update(env: &Env) -> Result<String, String> {
    if !env.sys_path(TOOL).is_file() {
        env.sys
            .open_url(SOFTWARE_UPDATE_URL)
            .map_err(|e| format!("Software Update could not be opened: {e}"))?;
        return Ok("Software Update is open. Under Automatic Updates, turn on \"Install Security Responses and system files\".".into());
    }
    let out = env
        .sys
        .run_admin(&format!("{TOOL} update"))
        .map_err(|e| format!("The update could not start: {e}"))?;
    if crate::sys::admin_cancelled(&out) {
        return Err("Cancelled: nothing was changed.".into());
    }
    if out.status != 0 {
        let e = out.stderr.trim();
        return Err(format!(
            "macOS could not update the rules: {}",
            if e.is_empty() { "no reason given" } else { e }
        ));
    }
    Ok("macOS checked for new malware rules.".into())
}

type Cached = (PathBuf, Vec<Option<SystemTime>>, Arc<Rules>);
static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

/// The rules of this Mac, compiled once and again only when macOS updated them. `Err` says in
/// words why there are none.
pub fn load(env: &Env) -> Result<Arc<Rules>, String> {
    let dir = rules_dir(env).ok_or_else(|| {
        "Apple's malware rules (XProtect) were not found on this Mac, so file contents were not checked".to_string()
    })?;
    let list = crate::known_malware::list_path(env);
    let stamps: Vec<Option<SystemTime>> = [dir.join(FILE_RULES), dir.join(SCRIPT_RULES), list]
        .iter()
        .map(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok())
        .collect();
    if let Ok(c) = CACHE.lock()
        && let Some((d, s, r)) = c.as_ref()
        && *d == dir
        && *s == stamps
    {
        return Ok(r.clone());
    }
    let mut rules = compile(&dir)?;
    rules.known = crate::known_malware::hashes(env);
    let rules = Arc::new(rules);
    if let Ok(mut c) = CACHE.lock() {
        *c = Some((dir, stamps, rules.clone()));
    }
    Ok(rules)
}

fn compile(dir: &Path) -> Result<Rules, String> {
    let mut c = yara_x::Compiler::new();
    c.relaxed_re_syntax(true);
    let mut problems = Vec::new();
    let mut added = 0;
    for f in [FILE_RULES, SCRIPT_RULES] {
        let Ok(src) = std::fs::read_to_string(dir.join(f)) else {
            if f == FILE_RULES {
                problems.push(format!("{f} could not be read"));
            }
            continue;
        };
        c.new_namespace(f);
        match c.add_source(src.as_str()) {
            Ok(_) => added += 1,
            Err(e) => problems.push(format!(
                "{f} could not be compiled: {}",
                e.to_string().lines().next().unwrap_or("")
            )),
        }
    }
    if added == 0 {
        return Err(format!(
            "Apple's malware rules (XProtect) could not be used ({}), so file contents were not checked",
            problems.join("; ")
        ));
    }
    let rules = c.build();
    let count = rules.iter().count();
    Ok(Rules {
        rules,
        count,
        problems,
        known: HashMap::new(),
    })
}

/// Can a file starting with `head` run (so Apple's rules apply to it)?
pub fn can_run(head: &[u8]) -> bool {
    const MACHO: [[u8; 4]; 6] = [
        [0xfe, 0xed, 0xfa, 0xce],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
        [0xbe, 0xba, 0xfe, 0xca],
    ];
    MACHO.iter().any(|m| head.starts_with(m))
        || head.starts_with(b"#!")
        || head.starts_with(b"Fasd")
        || head.starts_with(b"MZ")
}

/// Is the file's content somewhere else (an iCloud or cloud drive placeholder)? Reading it
/// would download it.
#[cfg(target_os = "macos")]
fn not_here(md: &std::fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    const SF_DATALESS: u32 = 0x4000_0000;
    md.st_flags() & SF_DATALESS != 0
}

#[cfg(not(target_os = "macos"))]
fn not_here(_md: &std::fs::Metadata) -> bool {
    false
}

/// The rules matching one file, if it can run and is not too big. Errors count as no match.
pub fn check_file(rules: &Rules, path: &Path) -> Vec<Hit> {
    let mut scanner = yara_x::Scanner::new(&rules.rules);
    scanner.set_timeout(FILE_TIMEOUT);
    check_with(&mut scanner, &rules.known, path)
}

fn sha256(path: &Path) -> Option<[u8; 32]> {
    let h = crate::self_update::sha256_file(path).ok()?;
    hex::decode(h).ok()?.try_into().ok()
}

fn check_with(
    scanner: &mut yara_x::Scanner<'_>,
    known: &HashMap<[u8; 32], String>,
    path: &Path,
) -> Vec<Hit> {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return Vec::new();
    };
    if !md.is_file() || md.len() > MAX_BYTES || not_here(&md) {
        return Vec::new();
    }
    let mut head = [0u8; 4];
    let read = std::fs::File::open(path).and_then(|mut f| {
        use std::io::Read;
        f.read(&mut head)
    });
    let head = &head[..read.unwrap_or(0)];
    let runs = can_run(head);
    // Disk images and installer packages carry Mac malware too; only hashes apply to them.
    let package = head.starts_with(b"xar!")
        || path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("dmg"));
    let hashed = package && !known.is_empty();
    if !runs && !hashed {
        return Vec::new();
    }
    let mut hits = Vec::new();
    if !known.is_empty()
        && let Some(family) = sha256(path).and_then(|d| known.get(&d))
    {
        hits.push(Hit {
            name: family.clone(),
            script: false,
            listed: true,
        });
    }
    if !runs {
        return hits;
    }
    let Ok(res) = scanner.scan_file(path) else {
        return hits;
    };
    hits.extend(res.matching_rules().map(|r| {
        let name = r
            .metadata()
            .find_map(|(k, v)| match (k, v) {
                ("description", yara_x::MetaValue::String(s)) => Some(s.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| r.identifier().to_string());
        Hit {
            name,
            script: r.namespace() == SCRIPT_RULES,
            listed: false,
        }
    }));
    hits
}

/// What a walk found and how much it looked at.
#[derive(Debug, Default)]
pub struct Found {
    /// Each matching file with its rules, in the order found.
    pub hits: Vec<(PathBuf, Vec<Hit>)>,
    /// Files looked at.
    pub files: u64,
}

/// Check every file under `roots` (folders or single files) with `rules`, never entering a
/// path for which `skip` is true. Several threads read at once; `cancelled` is asked often.
/// `step` is told each root as it starts, and the file being read now and then (for the
/// progress bar and line).
pub fn walk(
    rules: &Rules,
    roots: &[PathBuf],
    skip: &(dyn Fn(&Path) -> bool + Sync),
    cancelled: &(dyn Fn() -> bool + Sync),
    step: &mut dyn FnMut(usize, &Path),
) -> Found {
    let (tx, rx) = mpsc::sync_channel::<PathBuf>(512);
    let rx = Mutex::new(rx);
    let hits = Mutex::new(Vec::new());
    let mut files = 0;
    std::thread::scope(|s| {
        for _ in 0..READERS {
            s.spawn(|| {
                let mut scanner = yara_x::Scanner::new(&rules.rules);
                scanner.set_timeout(FILE_TIMEOUT);
                loop {
                    let next = rx.lock().ok().and_then(|r| r.recv().ok());
                    let Some(p) = next else { break };
                    if cancelled() {
                        continue;
                    }
                    let found = check_with(&mut scanner, &rules.known, &p);
                    if !found.is_empty()
                        && let Ok(mut h) = hits.lock()
                    {
                        h.push((p, found));
                    }
                }
            });
        }
        for (k, root) in roots.iter().enumerate() {
            // A root listed twice, or inside another root, is walked once with that one.
            let inside = roots
                .iter()
                .enumerate()
                .any(|(j, r)| (r == root && j < k) || (r != root && root.starts_with(r)));
            if cancelled() {
                break;
            }
            if inside {
                continue;
            }
            step(k, root);
            let mut stack = vec![root.clone()];
            while let Some(p) = stack.pop() {
                if cancelled() {
                    break;
                }
                if skip(&p) {
                    continue;
                }
                let Ok(md) = std::fs::symlink_metadata(&p) else {
                    continue;
                };
                if md.is_dir() {
                    if let Ok(rd) = std::fs::read_dir(&p) {
                        stack.extend(rd.flatten().map(|e| e.path()));
                    }
                } else if md.is_file() && md.len() <= MAX_BYTES && !not_here(&md) {
                    files += 1;
                    if files % 500 == 0 {
                        step(k, &p);
                    }
                    if tx.send(p).is_err() {
                        break;
                    }
                }
            }
        }
        drop(tx);
    });
    let mut hits = hits.into_inner().unwrap_or_default();
    hits.sort_by(|a, b| a.0.cmp(&b.0));
    Found { hits, files }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_in_words() {
        let h = |n: &str, script| Hit {
            name: n.into(),
            script,
            listed: false,
        };
        assert_eq!(
            h("MACOS.ADLOAD.I", false).describe(),
            "Adload (adware), Apple's rule MACOS.ADLOAD.I"
        );
        assert_eq!(
            h("OSX.Genieo.G", false).describe(),
            "Genieo (adware), Apple's rule OSX.Genieo.G"
        );
        assert_eq!(
            h("MACOS.SOMA.L", false).describe(),
            "Soma (password and wallet stealer), Apple's rule MACOS.SOMA.L"
        );
        assert_eq!(
            h("MACOS.OSASCRIPT.COTABR", true).describe(),
            "a script that acts like known malware, Apple's rule MACOS.OSASCRIPT.COTABR"
        );
        assert_eq!(
            h("XProtect_MACOS_DUBROBBER_A", false).describe(),
            "Dubrobber (malware), Apple's rule XProtect_MACOS_DUBROBBER_A"
        );
        let l = |n: &str| Hit {
            name: n.into(),
            script: false,
            listed: true,
        };
        assert_eq!(
            l("AMOS").describe(),
            "a known malware sample (AMOS), listed by abuse.ch MalwareBazaar"
        );
        assert_eq!(
            l("").describe(),
            "a known malware sample, listed by abuse.ch MalwareBazaar"
        );
    }

    #[test]
    fn only_files_that_can_run() {
        assert!(can_run(&[0xcf, 0xfa, 0xed, 0xfe]));
        assert!(can_run(&[0xca, 0xfe, 0xba, 0xbe]));
        assert!(can_run(b"#!/bin/sh"));
        assert!(can_run(b"FasdUAS"));
        assert!(can_run(b"MZ\x90\x00"));
        assert!(!can_run(b"%PDF"));
        assert!(!can_run(&[0xff, 0xd8, 0xff, 0xe0]));
        assert!(!can_run(b""));
    }
}
