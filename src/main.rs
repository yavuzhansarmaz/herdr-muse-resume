//! Herdr startup hook: resume Muse sessions in restored panes.
//!
//! Herdr natively auto-resumes agents it knows how to launch (codex, claude,
//! ...). Muse is detected but has no native resume support, so restored Muse
//! panes come back as plain shells. This hook closes that gap:
//!
//!   1. After Herdr restores the session, list all panes.
//!   2. Skip panes that already run something (agent != null).
//!   3. Group the remaining plain-shell panes by cwd. For each cwd, fetch the
//!      recent valid Muse sessions from Muse's session-index.db (newest first)
//!      and give every pane its own distinct session, so N panes sharing one
//!      cwd resume N different sessions instead of fighting over the newest.
//!   4. Run `muse resume <session-id>` in each pane via `herdr pane run`,
//!      waiting first for each shell to settle at a prompt (bounded by
//!      delay_seconds) and staggered so heavy TUIs don't all start in the
//!      same instant.
//!
//! Stable matching: pane -> session assignments are persisted to
//! resume-state.json next to the plugin config. On the next restart, a pane
//! whose mapped session is still valid for its cwd gets that same session
//! back; unknown or stale panes fall back to the newest free session. Pane
//! ids are Herdr's stable per-pane numbers, so each space keeps its session.
//!
//! Safety:
//!   - Read-only access to Muse's session index (never modifies sessions).
//!   - Each Muse session is resumed at most once per run (dedup by session id).
//!   - Panes whose cwd has no Muse history are left untouched.
//!   - Honors optional config at $HERDR_PLUGIN_CONFIG_DIR/config.toml:
//!     ```toml
//!     delay_seconds = 10       # max wait per pane for its shell to settle
//!                              # at a prompt (adaptive: proceeds as soon as
//!                              # pane output is stable; 0 disables waiting)
//!     stagger_seconds = 2      # pause between resumes (0 disables)
//!     ignore_cwds = ["/tmp"]   # exact cwd prefixes to skip
//!     only_cwds = []           # if non-empty, resume only under these prefixes
//!     dry_run = false          # log actions without running them
//!     ```
//!
//! Manual use (also handy for testing):
//!     herdr-muse-resume [--dry-run] [--pane <pane-id>] [--delay N]
//!
//! Rust port of the original stdlib-only resume.py (v0.2.x): identical CLI,
//! config keys, state file format, and log lines; no interpreter needed.

use rusqlite::{params_from_iter, Connection, OpenFlags};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use wait_timeout::ChildExt;

const STATE_FILENAME: &str = "resume-state.json";
// Candidates fetched per cwd; comfortably above any realistic pane count.
const SESSIONS_PER_CWD: usize = 50;
const HERDR_TIMEOUT_SECS: u64 = 30;

fn log(msg: &str) {
    println!("[muse-resume] {msg}");
    let _ = std::io::stdout().flush();
}

#[derive(Debug, Clone)]
struct Config {
    delay_seconds: i64,
    stagger_seconds: i64,
    ignore_cwds: Vec<String>,
    only_cwds: Vec<String>,
    dry_run: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            delay_seconds: 10,
            stagger_seconds: 2,
            ignore_cwds: Vec::new(),
            only_cwds: Vec::new(),
            dry_run: false,
        }
    }
}

fn load_config() -> Config {
    let mut cfg = Config::default();
    let dir = env::var("HERDR_PLUGIN_CONFIG_DIR").unwrap_or_default();
    if dir.is_empty() {
        return cfg;
    }
    let path = Path::new(&dir).join("config.toml");
    if !path.is_file() {
        return cfg;
    }
    let display = path.to_string_lossy();
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            log(&format!(
                "WARNING: ignoring unreadable config {display}: {e}"
            ));
            return cfg;
        }
    };
    let value: toml::Value = match text.parse() {
        Ok(v) => v,
        Err(e) => {
            log(&format!(
                "WARNING: ignoring unreadable config {display}: {e}"
            ));
            return cfg;
        }
    };
    if let Some(tbl) = value.as_table() {
        if let Some(v) = tbl.get("delay_seconds") {
            match v.as_integer() {
                Some(n) => cfg.delay_seconds = n,
                None => log("WARNING: ignoring non-integer delay_seconds in config"),
            }
        }
        if let Some(v) = tbl.get("stagger_seconds") {
            match v.as_integer() {
                Some(n) => cfg.stagger_seconds = n,
                None => log("WARNING: ignoring non-integer stagger_seconds in config"),
            }
        }
        for key in ["ignore_cwds", "only_cwds"] {
            if let Some(v) = tbl.get(key) {
                match v
                    .as_array()
                    .and_then(|a| a.iter().map(|e| e.as_str()).collect::<Option<Vec<_>>>())
                {
                    Some(list) => {
                        let target = if key == "ignore_cwds" {
                            &mut cfg.ignore_cwds
                        } else {
                            &mut cfg.only_cwds
                        };
                        *target = list.into_iter().map(str::to_string).collect();
                    }
                    None => log(&format!(
                        "WARNING: ignoring non-string-list {key} in config"
                    )),
                }
            }
        }
        if let Some(v) = tbl.get("dry_run") {
            match v.as_bool() {
                Some(b) => cfg.dry_run = b,
                None => log("WARNING: ignoring non-boolean dry_run in config"),
            }
        }
    }
    log(&format!("loaded config from {display}"));
    cfg
}

/// Run `herdr <args>`, capturing stdout. 30s timeout like the original.
fn run_herdr(args: &[String]) -> Result<String, String> {
    let exe = env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let mut child = Command::new(&exe)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("herdr {} failed to start: {e}", args.join(" ")))?;
    match child.wait_timeout(Duration::from_secs(HERDR_TIMEOUT_SECS)) {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "herdr {} timed out after {HERDR_TIMEOUT_SECS}s",
                args.join(" ")
            ));
        }
        Err(e) => return Err(format!("herdr {} failed: {e}", args.join(" "))),
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("herdr {} failed: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "herdr {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn herdr_json(args: &[String]) -> Result<serde_json::Value, String> {
    let text = run_herdr(args)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::Value::Object(Default::default()));
    }
    serde_json::from_str(trimmed)
        .map_err(|e| format!("herdr {} returned invalid JSON: {e}", args.join(" ")))
}

#[derive(Debug, Clone)]
struct Pane {
    pane_id: String,
    cwd: String,
    agent: Option<String>,
}

fn list_panes() -> Result<Vec<Pane>, String> {
    let v = herdr_json(&["api".to_string(), "snapshot".to_string()])?;
    let snap = v
        .get("result")
        .and_then(|r| r.get("snapshot"))
        .ok_or_else(|| "herdr api snapshot: missing result.snapshot".to_string())?;
    let arr: &[serde_json::Value] = match snap.get("panes") {
        None | Some(serde_json::Value::Null) => &[],
        Some(serde_json::Value::Array(a)) => a,
        Some(_) => return Err("herdr api snapshot: 'panes' is not an array".to_string()),
    };
    Ok(arr
        .iter()
        .map(|p| Pane {
            pane_id: p
                .get("pane_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string(),
            cwd: p
                .get("cwd")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            agent: p.get("agent").and_then(|v| v.as_str()).map(str::to_string),
        })
        .collect())
}

fn norm(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical.to_string_lossy().to_string();
    }
    // Missing path: lexical fallback (realpath would still normalize).
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return format!("{home}/{rest}");
        }
    } else if path == "~" {
        if let Ok(home) = env::var("HOME") {
            return home;
        }
    }
    path.to_string()
}

fn data_home() -> String {
    env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "~/.local/share".to_string())
}

fn find_session_db() -> Option<String> {
    let candidates = [
        format!("{}/muse/session-index.db", data_home()),
        "~/.local/share/muse/session-index.db".to_string(),
        "~/Library/Application Support/muse/session-index.db".to_string(),
    ];
    candidates
        .into_iter()
        .map(|c| expand_tilde(&c))
        .find(|p| Path::new(p).is_file())
}

#[derive(Debug, Clone, PartialEq)]
struct MuseSession {
    session_id: String,
    session_name: Option<String>,
    prompt_count: i64,
}

/// Recent valid Muse sessions for cwd, newest first. Read-only.
///
/// Mirrors `muse resume --last` workspace scoping (exact workspace match),
/// but returns a list so panes sharing one cwd can each take a distinct
/// session. An empty/missing index simply yields [].
fn recent_muse_sessions(cwd: &str, limit: usize) -> Vec<MuseSession> {
    let db_path = match find_session_db() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let mut candidates = vec![cwd.to_string(), norm(cwd)];
    candidates.sort();
    candidates.dedup();
    let conn = match Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("WARNING: could not query muse session index: {e}"));
            return Vec::new();
        }
    };
    let placeholders = candidates.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT session_id, session_name, prompt_count FROM sessions \
         WHERE status = 'valid' AND workspace_root IN ({placeholders}) \
         ORDER BY updated_at_us DESC LIMIT ?"
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("WARNING: could not query muse session index: {e}"));
            return Vec::new();
        }
    };
    let mut params: Vec<rusqlite::types::Value> = candidates
        .into_iter()
        .map(rusqlite::types::Value::from)
        .collect();
    params.push(rusqlite::types::Value::from(std::cmp::max(1, limit) as i64));
    let rows = match stmt.query_map(params_from_iter(params), |row| {
        Ok(MuseSession {
            session_id: row.get(0)?,
            session_name: row.get(1)?,
            prompt_count: row.get(2)?,
        })
    }) {
        Ok(r) => r,
        Err(e) => {
            log(&format!("WARNING: could not query muse session index: {e}"));
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for row in rows {
        match row {
            Ok(s) => out.push(s),
            Err(e) => {
                log(&format!("WARNING: could not query muse session index: {e}"));
                return Vec::new();
            }
        }
    }
    out
}

fn under(path: &str, prefixes: &[String]) -> bool {
    let resolved = norm(path);
    prefixes.iter().any(|prefix| {
        let base = norm(prefix);
        resolved == base || resolved.starts_with(&(base.trim_end_matches('/').to_string() + "/"))
    })
}

/// Where pane -> session assignments persist across restarts.
fn state_path() -> String {
    let dir = env::var("HERDR_PLUGIN_CONFIG_DIR").unwrap_or_default();
    if !dir.is_empty() {
        return Path::new(&dir)
            .join(STATE_FILENAME)
            .to_string_lossy()
            .to_string();
    }
    expand_tilde(&format!(
        "{}/herdr-muse-resume/{STATE_FILENAME}",
        data_home()
    ))
}

fn load_mapping(path: &str) -> HashMap<String, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return HashMap::new(),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    match value.get("panes").and_then(|p| p.as_object()) {
        Some(map) => map
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        None => HashMap::new(),
    }
}

fn save_mapping(path: &str, mapping: &HashMap<String, String>) -> Result<(), String> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.to_string_lossy()))?;
        }
    }
    let ordered: BTreeMap<&String, &String> = mapping.iter().collect();
    let text = serde_json::to_string_pretty(&serde_json::json!({"panes": ordered}))
        .map_err(|e| format!("cannot encode state: {e}"))?;
    let tmp = format!("{path}.tmp");
    fs::write(&tmp, text + "\n").map_err(|e| format!("cannot write {tmp}: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("cannot replace {path}: {e}"))?;
    Ok(())
}

#[derive(Debug, Clone)]
struct EligiblePane {
    pane_id: String,
    cwd: String,
}

/// Assign each eligible pane a distinct session.
///
/// `panes` are already filtered to resume targets. `fetch` maps a cwd to
/// its sessions, newest first. `mapping` is pane_id -> session_id remembered
/// from a previous run.
///
/// Returns (assignments, skips): assignments maps pane_id -> session,
/// skips maps pane_id -> human-readable reason.
fn assign_sessions(
    panes: &[EligiblePane],
    fetch: &dyn Fn(&str) -> Vec<MuseSession>,
    mapping: &HashMap<String, String>,
) -> (BTreeMap<String, MuseSession>, BTreeMap<String, String>) {
    let mut assignments = BTreeMap::new();
    let mut skips = BTreeMap::new();
    let mut taken: HashSet<String> = HashSet::new();
    let mut groups: BTreeMap<String, Vec<&EligiblePane>> = BTreeMap::new();
    for pane in panes {
        groups.entry(norm(&pane.cwd)).or_default().push(pane);
    }
    for group_unsorted in groups.values() {
        let mut group = group_unsorted.clone();
        group.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        let sessions: Vec<MuseSession> = fetch(&group[0].cwd)
            .into_iter()
            .filter(|s| !taken.contains(&s.session_id))
            .collect();
        if sessions.is_empty() {
            for pane in &group {
                skips.insert(pane.pane_id.clone(), "no muse history here".to_string());
            }
            continue;
        }
        let by_id: HashMap<&str, &MuseSession> = sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s))
            .collect();
        // Pass 1: keep stable pane -> session matches from previous runs.
        let mut pending: Vec<&&EligiblePane> = Vec::new();
        for pane in &group {
            match mapping.get(&pane.pane_id) {
                Some(wanted) if by_id.contains_key(wanted.as_str()) && !taken.contains(wanted) => {
                    assignments.insert(pane.pane_id.clone(), (*by_id[wanted.as_str()]).clone());
                    taken.insert(wanted.clone());
                }
                _ => pending.push(pane),
            }
        }
        // Pass 2: newest free session for the rest, in pane-id order.
        let free: Vec<&MuseSession> = sessions
            .iter()
            .filter(|s| !taken.contains(&s.session_id))
            .collect();
        for (pane, session) in pending.iter().zip(free.iter()) {
            assignments.insert(pane.pane_id.clone(), (*session).clone());
            taken.insert(session.session_id.clone());
        }
        if pending.len() > free.len() {
            let msg = format!(
                "only {} muse session(s) for this cwd, all already assigned",
                sessions.len()
            );
            for pane in &pending[free.len()..] {
                skips.insert(pane.pane_id.clone(), msg.clone());
            }
        }
    }
    (assignments, skips)
}

/// Pair each item with its index, pausing before every item after the first.
///
/// Keeps N heavy agents from starting in the same instant after a
/// restore. A non-positive stagger disables pausing.
fn each_with_pause<T>(items: Vec<T>, stagger: i64, sleep: &mut dyn FnMut(i64)) -> Vec<(usize, T)> {
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        if index > 0 && stagger > 0 {
            sleep(stagger);
        }
        out.push((index, item));
    }
    out
}

fn session_display_name(session: &MuseSession) -> &str {
    session.session_name.as_deref().unwrap_or("None")
}

// How often to re-read a pane while waiting for its shell to settle.
const READY_POLL_MS: u64 = 200;
// How many recent rows to compare; the prompt lives at the bottom.
const READY_PANE_LINES: &str = "20";

/// Recent plain-text output of a pane (bottom rows only).
fn pane_read_text(pane_id: &str) -> Result<String, String> {
    run_herdr(&[
        "pane".to_string(),
        "read".to_string(),
        "--lines".to_string(),
        READY_PANE_LINES.to_string(),
        "--format".to_string(),
        "text".to_string(),
        pane_id.to_string(),
    ])
}

/// True when a pane's output looks like a shell settled at a prompt:
/// non-empty and unchanged since the previous sample. Prompt-agnostic on
/// purpose: shells differ, but a ready shell stops printing.
fn is_settled(previous: Option<&str>, current: &str) -> bool {
    !current.trim().is_empty() && previous == Some(current)
}

/// Poll `sample` every `interval` until the output settles (see
/// `is_settled`) or `max_waits` sleeps elapse. Failed samples (None) never
/// settle but don't reset the previous sample either. Returns true when
/// settled, false on cap exhaustion.
fn wait_until_settled(
    sample: &mut dyn FnMut() -> Option<String>,
    sleep: &mut dyn FnMut(Duration),
    max_waits: u32,
    interval: Duration,
) -> bool {
    let mut previous: Option<String> = None;
    let mut waits = 0u32;
    loop {
        if let Some(current) = sample() {
            if is_settled(previous.as_deref(), &current) {
                return true;
            }
            previous = Some(current);
        }
        if waits >= max_waits {
            return false;
        }
        sleep(interval);
        waits += 1;
    }
}

fn run(argv: Vec<String>) -> i32 {
    let mut cfg = load_config();
    let mut only_pane: Option<String> = None;
    let mut rest: VecDeque<String> = argv.into();
    while let Some(arg) = rest.pop_front() {
        if arg == "--dry-run" {
            cfg.dry_run = true;
        } else if arg == "--pane" {
            match rest.pop_front() {
                Some(id) => only_pane = Some(id),
                None => log(&format!("WARNING: unknown argument {arg}")),
            }
        } else if arg == "--delay" {
            match rest.pop_front() {
                Some(v) => match v.parse::<i64>() {
                    Ok(n) => cfg.delay_seconds = n,
                    Err(_) => log(&format!("WARNING: ignoring invalid --delay {v}")),
                },
                None => log(&format!("WARNING: unknown argument {arg}")),
            }
        } else {
            log(&format!("WARNING: unknown argument {arg}"));
        }
    }

    // Max seconds to wait per pane for its shell to settle at a prompt.
    // Unlike the old fixed pre-sleep, each pane proceeds as soon as its own
    // output stabilizes, so fast shells resume immediately.
    let ready_cap_secs = cfg.delay_seconds.max(0);

    let panes = match list_panes() {
        Ok(p) => p,
        Err(e) => {
            log(&format!("ERROR: cannot list panes: {e}"));
            return 1;
        }
    };
    log(&format!("found {} pane(s)", panes.len()));

    let mapping_path = state_path();
    let mut mapping = load_mapping(&mapping_path);
    let mut eligible: Vec<EligiblePane> = Vec::new();
    for pane in &panes {
        let id = &pane.pane_id;
        let cwd = &pane.cwd;
        if let Some(ref only) = only_pane {
            if id != only {
                continue;
            }
        }
        match pane.agent.as_deref() {
            Some(agent) if !agent.is_empty() => {
                log(&format!("skip {id} ({cwd}): already runs {agent}"));
                continue;
            }
            _ => {}
        }
        if cwd.is_empty() {
            log(&format!("skip {id}: no cwd reported"));
            continue;
        }
        if !cfg.ignore_cwds.is_empty() && under(cwd, &cfg.ignore_cwds) {
            log(&format!("skip {id} ({cwd}): matches ignore_cwds"));
            continue;
        }
        if !cfg.only_cwds.is_empty() && !under(cwd, &cfg.only_cwds) {
            log(&format!("skip {id} ({cwd}): not under only_cwds"));
            continue;
        }
        eligible.push(EligiblePane {
            pane_id: id.clone(),
            cwd: cwd.clone(),
        });
    }

    if eligible.is_empty() {
        log("done, resumed 0 pane(s)");
        return 0;
    }

    let fetch = |cwd: &str| -> Vec<MuseSession> {
        let need = eligible
            .iter()
            .filter(|p| norm(&p.cwd) == norm(cwd))
            .count();
        recent_muse_sessions(cwd, SESSIONS_PER_CWD.max(need))
    };

    let (assignments, skips) = assign_sessions(&eligible, &fetch, &mapping);
    for (pane_id, reason) in &skips {
        let pane = eligible.iter().find(|p| &p.pane_id == pane_id).unwrap();
        log(&format!("skip {pane_id} ({}): {reason}", pane.cwd));
    }

    let mut resumed: HashSet<String> = HashSet::new();
    if cfg.dry_run {
        for (pane_id, session) in &assignments {
            let pane = eligible.iter().find(|p| &p.pane_id == pane_id).unwrap();
            log(&format!(
                "dry-run: would send to {pane_id} ({}): muse resume {}",
                pane.cwd, session.session_id
            ));
            resumed.insert(session.session_id.clone());
        }
    } else {
        let stagger = cfg.stagger_seconds.max(0);
        let ids: Vec<String> = assignments.keys().cloned().collect();
        let mut sleeper = |s: i64| thread::sleep(Duration::from_secs(s as u64));
        let plan = each_with_pause(ids, stagger, &mut sleeper);
        for (_, pane_id) in plan {
            let session = &assignments[&pane_id];
            let pane = eligible.iter().find(|p| p.pane_id == pane_id).unwrap();
            if only_pane.is_none() && ready_cap_secs > 0 {
                let mut sampler = || pane_read_text(&pane_id).ok();
                let mut sleeper = |d: Duration| thread::sleep(d);
                let max_waits = (ready_cap_secs as u64 * 1000 / READY_POLL_MS) as u32;
                let settled = wait_until_settled(
                    &mut sampler,
                    &mut sleeper,
                    max_waits,
                    Duration::from_millis(READY_POLL_MS),
                );
                if !settled {
                    log(&format!(
                        "WARNING: {pane_id} shell not settled after {ready_cap_secs}s, resuming anyway"
                    ));
                }
            }
            let cmd = format!("muse resume {}", session.session_id);
            let args = vec![
                "pane".to_string(),
                "run".to_string(),
                pane_id.clone(),
                cmd.clone(),
            ];
            if let Err(e) = run_herdr(&args) {
                log(&format!("ERROR: cannot resume in {pane_id}: {e}"));
                continue;
            }
            resumed.insert(session.session_id.clone());
            mapping.insert(pane_id.clone(), session.session_id.clone());
            log(&format!(
                "resumed {pane_id} ({}): {cmd} [{}, {} prompts]",
                pane.cwd,
                session_display_name(session),
                session.prompt_count
            ));
        }
    }

    if !cfg.dry_run {
        // Drop mappings for panes that no longer exist; keep the rest so
        // panes skipped this run (e.g. already running) stay stable.
        let mut live: HashSet<String> = panes
            .iter()
            .filter(|p| !p.pane_id.is_empty())
            .map(|p| p.pane_id.clone())
            .collect();
        if only_pane.is_some() {
            live.extend(mapping.keys().cloned());
        }
        mapping.retain(|k, _| live.contains(k));
        if let Err(e) = save_mapping(&mapping_path, &mapping) {
            log(&format!("ERROR: cannot save state {mapping_path}: {e}"));
            return 1;
        }
    }

    log(&format!("done, resumed {} pane(s)", resumed.len()));
    0
}

fn main() {
    std::process::exit(run(env::args().skip(1).collect()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn panes(ids: &[&str], cwd: &str) -> Vec<EligiblePane> {
        ids.iter()
            .map(|id| EligiblePane {
                pane_id: id.to_string(),
                cwd: cwd.to_string(),
            })
            .collect()
    }

    fn sessions(names: &[&str]) -> Vec<MuseSession> {
        names
            .iter()
            .map(|n| MuseSession {
                session_id: format!("id-{n}"),
                session_name: Some(n.to_string()),
                prompt_count: 1,
            })
            .collect()
    }

    fn name_of(map: &BTreeMap<String, MuseSession>, pane: &str) -> String {
        map[pane].session_name.clone().unwrap()
    }

    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        let id = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = env::temp_dir().join(format!(
            "herdr-muse-resume-test-{}-{}-{}",
            prefix,
            std::process::id(),
            id
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn same_cwd_panes_get_distinct_sessions() {
        // Regression: two panes sharing one cwd must resume two sessions,
        // not skip the second as "already resumed this run".
        let ps = panes(&["w1:p1", "w2:p1"], "/repo");
        let fetch = |_: &str| sessions(&["a", "b", "c"]);
        let (got, skips) = assign_sessions(&ps, &fetch, &HashMap::new());
        assert!(skips.is_empty());
        assert_eq!(name_of(&got, "w1:p1"), "a");
        assert_eq!(name_of(&got, "w2:p1"), "b");
    }

    #[test]
    fn three_panes_same_cwd() {
        let ps = panes(&["w1:p1", "w2:p1", "w3:p1"], "/repo");
        let fetch = |_: &str| sessions(&["a", "b", "c"]);
        let (got, skips) = assign_sessions(&ps, &fetch, &HashMap::new());
        assert!(skips.is_empty());
        assert_eq!(
            vec![
                name_of(&got, "w1:p1"),
                name_of(&got, "w2:p1"),
                name_of(&got, "w3:p1"),
            ],
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn stable_mapping_keeps_own_session() {
        let ps = panes(&["w1:p1", "w2:p1"], "/repo");
        let fetch = |_: &str| sessions(&["a", "b", "c"]);
        let mapping: HashMap<String, String> = [("w2:p1".to_string(), "id-b".to_string())]
            .into_iter()
            .collect();
        let (got, skips) = assign_sessions(&ps, &fetch, &mapping);
        assert!(skips.is_empty());
        assert_eq!(name_of(&got, "w2:p1"), "b");
        assert_eq!(name_of(&got, "w1:p1"), "a");
    }

    #[test]
    fn stale_mapping_falls_back_to_newest_free() {
        let ps = panes(&["w1:p1"], "/repo");
        let fetch = |_: &str| sessions(&["a", "b"]);
        let mapping: HashMap<String, String> = [("w1:p1".to_string(), "id-gone".to_string())]
            .into_iter()
            .collect();
        let (got, skips) = assign_sessions(&ps, &fetch, &mapping);
        assert!(skips.is_empty());
        assert_eq!(name_of(&got, "w1:p1"), "a");
    }

    #[test]
    fn more_panes_than_sessions() {
        let ps = panes(&["w1:p1", "w2:p1", "w3:p1"], "/repo");
        let fetch = |_: &str| sessions(&["a", "b"]);
        let (got, skips) = assign_sessions(&ps, &fetch, &HashMap::new());
        let keys: Vec<&str> = got.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, vec!["w1:p1", "w2:p1"]);
        assert!(skips.contains_key("w3:p1"));
    }

    #[test]
    fn no_history_everywhere() {
        let ps = panes(&["w1:p1", "w2:p1"], "/repo");
        let fetch = |_: &str| Vec::new();
        let (got, skips) = assign_sessions(&ps, &fetch, &HashMap::new());
        assert!(got.is_empty());
        assert_eq!(skips.len(), 2);
    }

    #[test]
    fn different_cwds_are_independent() {
        let ps = vec![
            EligiblePane {
                pane_id: "w1:p1".to_string(),
                cwd: "/a".to_string(),
            },
            EligiblePane {
                pane_id: "w2:p1".to_string(),
                cwd: "/b".to_string(),
            },
        ];
        let fetch = |cwd: &str| {
            if cwd == "/a" {
                sessions(&["x"])
            } else {
                sessions(&["y"])
            }
        };
        let (got, skips) = assign_sessions(&ps, &fetch, &HashMap::new());
        assert!(skips.is_empty());
        assert_eq!(name_of(&got, "w1:p1"), "x");
        assert_eq!(name_of(&got, "w2:p1"), "y");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_cwds_share_one_pool() {
        // /tmp/.../link -> /tmp/.../real: panes in both are one cwd group.
        let tmp = temp_dir("symlink");
        let real = tmp.join("real");
        fs::create_dir(&real).unwrap();
        let link = tmp.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let ps = vec![
            EligiblePane {
                pane_id: "w1:p1".to_string(),
                cwd: real.to_string_lossy().to_string(),
            },
            EligiblePane {
                pane_id: "w2:p1".to_string(),
                cwd: link.to_string_lossy().to_string(),
            },
        ];
        let fetch = |_: &str| sessions(&["a", "b"]);
        let (got, skips) = assign_sessions(&ps, &fetch, &HashMap::new());
        fs::remove_dir_all(&tmp).unwrap();
        assert!(skips.is_empty());
        let mut names = vec![name_of(&got, "w1:p1"), name_of(&got, "w2:p1")];
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn pauses_between_items_only() {
        let mut calls: Vec<i64> = Vec::new();
        let got = each_with_pause(vec!["a", "b", "c"], 2, &mut |s| calls.push(s));
        assert_eq!(
            got.into_iter().map(|(_, item)| item).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert_eq!(calls, vec![2, 2]);
    }

    #[test]
    fn no_pause_for_single_item() {
        let mut calls: Vec<i64> = Vec::new();
        let got = each_with_pause(vec!["a"], 5, &mut |s| calls.push(s));
        assert_eq!(
            got.into_iter().map(|(_, item)| item).collect::<Vec<_>>(),
            vec!["a"]
        );
        assert!(calls.is_empty());
    }

    #[test]
    fn non_positive_stagger_disables_pausing() {
        for stagger in [0, -1] {
            let mut calls: Vec<i64> = Vec::new();
            each_with_pause(vec!["a", "b"], stagger, &mut |s| calls.push(s));
            assert!(calls.is_empty(), "stagger={stagger}");
        }
    }

    #[test]
    fn mapping_roundtrip_and_missing_file() {
        let tmp = temp_dir("mapping");
        let path = tmp.join("sub").join("resume-state.json");
        let display = path.to_string_lossy().to_string();
        assert!(load_mapping(&display).is_empty());
        let mapping: HashMap<String, String> = [("w1:p1".to_string(), "id-a".to_string())]
            .into_iter()
            .collect();
        save_mapping(&display, &mapping).unwrap();
        assert_eq!(load_mapping(&display), mapping);
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn corrupt_mapping_file_yields_empty_mapping() {
        let tmp = temp_dir("corrupt");
        let path = tmp.join("resume-state.json");
        fs::write(&path, "{not json").unwrap();
        assert!(load_mapping(&path.to_string_lossy()).is_empty());
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn settled_needs_two_identical_nonempty_samples() {
        assert!(!is_settled(None, "$ "));
        assert!(is_settled(Some("$ "), "$ "));
        assert!(!is_settled(Some("$ "), "$ % "));
    }

    #[test]
    fn settled_rejects_empty_output() {
        // A pane that prints nothing (shell not started yet) is never ready,
        // even when consecutive samples agree.
        assert!(!is_settled(None, ""));
        assert!(!is_settled(Some(""), ""));
        assert!(!is_settled(Some("  \n "), "  \n "));
    }

    /// Scripted sampler/sleeper for wait_until_settled tests.
    fn scripted(
        outputs: Vec<Option<&str>>,
    ) -> (
        impl FnMut() -> Option<String>,
        impl Fn() -> usize,
        impl FnMut(Duration),
    ) {
        use std::cell::RefCell;
        use std::rc::Rc;
        let outputs: Vec<Option<String>> =
            outputs.into_iter().map(|o| o.map(str::to_string)).collect();
        let outputs = Rc::new(RefCell::new(outputs.into_iter()));
        let sleeps = Rc::new(RefCell::new(0usize));
        let sampler = {
            let outputs = Rc::clone(&outputs);
            move || outputs.borrow_mut().next().unwrap_or(None)
        };
        let sleep_count = {
            let sleeps = Rc::clone(&sleeps);
            move || *sleeps.borrow()
        };
        let sleeper = {
            let sleeps = Rc::clone(&sleeps);
            move |_: Duration| *sleeps.borrow_mut() += 1
        };
        (sampler, sleep_count, sleeper)
    }

    #[test]
    fn wait_settles_on_second_identical_sample() {
        let (mut sampler, sleep_count, mut sleeper) = scripted(vec![Some("$ "), Some("$ ")]);
        assert!(wait_until_settled(
            &mut sampler,
            &mut sleeper,
            50,
            Duration::from_millis(200)
        ));
        assert_eq!(sleep_count(), 1);
    }

    #[test]
    fn wait_settles_after_changing_output() {
        let (mut sampler, sleep_count, mut sleeper) =
            scripted(vec![Some(""), Some("loading..."), Some("$ "), Some("$ ")]);
        assert!(wait_until_settled(
            &mut sampler,
            &mut sleeper,
            50,
            Duration::from_millis(200)
        ));
        assert_eq!(sleep_count(), 3);
    }

    #[test]
    fn wait_tolerates_failed_samples() {
        // Read errors (None) neither settle nor reset the previous sample.
        let (mut sampler, _, mut sleeper) = scripted(vec![Some("$ "), None, Some("$ ")]);
        assert!(wait_until_settled(
            &mut sampler,
            &mut sleeper,
            50,
            Duration::from_millis(200)
        ));
    }

    #[test]
    fn wait_gives_up_at_cap() {
        // Ever-changing output exhausts the cap and reports not settled,
        // sleeping exactly max_waits times (no trailing sleep).
        let mut n = 0;
        let mut sampler = move || {
            n += 1;
            Some(format!("tick {n}"))
        };
        let mut sleeps = 0;
        let mut sleeper = |_: Duration| sleeps += 1;
        assert!(!wait_until_settled(
            &mut sampler,
            &mut sleeper,
            4,
            Duration::from_millis(200)
        ));
        assert_eq!(sleeps, 4);
    }
}
