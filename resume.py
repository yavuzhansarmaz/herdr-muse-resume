#!/usr/bin/env python3
"""Herdr startup hook: resume Muse sessions in restored panes.

Herdr natively auto-resumes agents it knows how to launch (codex, claude,
...). Muse is detected but has no native resume support, so restored Muse
panes come back as plain shells. This hook closes that gap:

  1. After Herdr restores the session, list all panes.
  2. Skip panes that already run something (agent != null).
  3. Group the remaining plain-shell panes by cwd. For each cwd, fetch the
     recent valid Muse sessions from Muse's session-index.db (newest first)
     and give every pane its own distinct session, so N panes sharing one
     cwd resume N different sessions instead of fighting over the newest.
  4. Run `muse resume <session-id>` in each pane via `herdr pane run`,
     staggered so heavy TUIs don't all start in the same instant.

Stable matching: pane -> session assignments are persisted to
resume-state.json next to the plugin config. On the next restart, a pane
whose mapped session is still valid for its cwd gets that same session
back; unknown or stale panes fall back to the newest free session. Pane
ids are Herdr's stable per-pane numbers, so each space keeps its session.

Safety:
  - Read-only access to Muse's session index (never modifies sessions).
  - Each Muse session is resumed at most once per run (dedup by session id).
  - Panes whose cwd has no Muse history are left untouched.
  - Honors optional config at $HERDR_PLUGIN_CONFIG_DIR/config.toml:
        delay_seconds = 5        # wait for restored shells to reach a prompt
        stagger_seconds = 2      # pause between resumes (0 disables)
        ignore_cwds = ["/tmp"]   # exact cwd prefixes to skip
        only_cwds = []           # if non-empty, resume only under these prefixes
        dry_run = false          # log actions without running them

Manual use (also handy for testing):
    python3 resume.py [--dry-run] [--pane <pane-id>] [--delay N]
"""

import json
import os
import sqlite3
import subprocess
import sys
import time

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    tomllib = None

STATE_FILENAME = "resume-state.json"
# Candidates fetched per cwd; comfortably above any realistic pane count.
SESSIONS_PER_CWD = 50


def log(msg):
    print(f"[muse-resume] {msg}", flush=True)


def load_config():
    cfg = {
        "delay_seconds": 5,
        "stagger_seconds": 2,
        "ignore_cwds": [],
        "only_cwds": [],
        "dry_run": False,
    }
    config_dir = os.environ.get("HERDR_PLUGIN_CONFIG_DIR", "")
    path = os.path.join(config_dir, "config.toml") if config_dir else ""
    if path and os.path.isfile(path) and tomllib is not None:
        try:
            with open(path, "rb") as fh:
                user_cfg = tomllib.load(fh)
            for key in cfg:
                if key in user_cfg:
                    cfg[key] = user_cfg[key]
            log(f"loaded config from {path}")
        except Exception as exc:
            log(f"WARNING: ignoring unreadable config {path}: {exc}")
    return cfg


def herdr(*args, expect_json=True):
    exe = os.environ.get("HERDR_BIN_PATH", "herdr")
    proc = subprocess.run(
        [exe, *args], capture_output=True, text=True, timeout=30
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"herdr {' '.join(args)} failed: {proc.stderr.strip()}"
        )
    if not expect_json:
        return proc.stdout
    text = proc.stdout.strip()
    if not text:
        return {}
    return json.loads(text)


def list_panes():
    snapshot = herdr("api", "snapshot")
    return snapshot["result"]["snapshot"].get("panes", [])


def norm(path):
    try:
        return os.path.realpath(path)
    except Exception:
        return path


def find_session_db():
    data_home = os.environ.get("XDG_DATA_HOME") or "~/.local/share"
    candidates = [
        os.path.expanduser(os.path.join(data_home, "muse/session-index.db")),
        os.path.expanduser("~/.local/share/muse/session-index.db"),
        os.path.expanduser(
            "~/Library/Application Support/muse/session-index.db"
        ),
    ]
    return next((p for p in candidates if os.path.isfile(p)), None)


def recent_muse_sessions(cwd, limit=SESSIONS_PER_CWD):
    """Recent valid Muse sessions for cwd, newest first. Read-only.

    Mirrors `muse resume --last` workspace scoping (exact workspace match),
    but returns a list so panes sharing one cwd can each take a distinct
    session. An empty/missing index simply yields [].
    """
    db_path = find_session_db()
    if db_path is None:
        return []
    candidates = {cwd, norm(cwd)}
    try:
        db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        try:
            placeholders = ",".join("?" for _ in candidates)
            rows = db.execute(
                f"""SELECT session_id, session_name, prompt_count
                    FROM sessions
                    WHERE status = 'valid'
                      AND workspace_root IN ({placeholders})
                    ORDER BY updated_at_us DESC LIMIT ?""",
                [*sorted(candidates), max(1, limit)],
            ).fetchall()
        finally:
            db.close()
    except Exception as exc:
        log(f"WARNING: could not query muse session index: {exc}")
        return []
    return [
        {"session_id": r[0], "session_name": r[1], "prompt_count": r[2]}
        for r in rows
    ]


def under(path, prefixes):
    path = norm(path)
    return any(
        path == norm(prefix) or path.startswith(norm(prefix).rstrip("/") + "/")
        for prefix in prefixes
    )


def state_path():
    """Where pane -> session assignments persist across restarts."""
    config_dir = os.environ.get("HERDR_PLUGIN_CONFIG_DIR", "")
    if config_dir:
        return os.path.join(config_dir, STATE_FILENAME)
    data_home = os.environ.get("XDG_DATA_HOME") or "~/.local/share"
    return os.path.expanduser(
        os.path.join(data_home, "herdr-muse-resume", STATE_FILENAME)
    )


def load_mapping(path):
    try:
        with open(path, encoding="utf-8") as fh:
            data = json.load(fh)
        mapping = data.get("panes", {}) if isinstance(data, dict) else {}
        return {k: v for k, v in mapping.items() if isinstance(v, str)}
    except (OSError, ValueError):
        return {}


def save_mapping(path, mapping):
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump({"panes": mapping}, fh, indent=2, sort_keys=True)
        fh.write("\n")
    os.replace(tmp, path)


def assign_sessions(panes, fetch_sessions, mapping):
    """Assign each eligible pane a distinct session.

    panes: [{"pane_id":..., "cwd":...}], already filtered to resume targets.
    fetch_sessions: cwd -> [session dicts newest first].
    mapping: pane_id -> session_id remembered from a previous run.

    Returns (assignments, skips): assignments maps pane_id -> session dict,
    skips maps pane_id -> human-readable reason.
    """
    assignments = {}
    skips = {}
    taken = set()
    groups = {}
    for pane in panes:
        groups.setdefault(norm(pane["cwd"]), []).append(pane)
    for cwd_norm in sorted(groups):
        group = sorted(groups[cwd_norm], key=lambda p: p["pane_id"])
        sessions = [s for s in fetch_sessions(group[0]["cwd"])
                    if s["session_id"] not in taken]
        if not sessions:
            for pane in group:
                skips[pane["pane_id"]] = "no muse history here"
            continue
        by_id = {s["session_id"]: s for s in sessions}
        # Pass 1: keep stable pane -> session matches from previous runs.
        pending = []
        for pane in group:
            wanted = mapping.get(pane["pane_id"])
            if wanted and wanted in by_id and wanted not in taken:
                assignments[pane["pane_id"]] = by_id[wanted]
                taken.add(wanted)
            else:
                pending.append(pane)
        # Pass 2: newest free session for the rest, in pane-id order.
        free = [s for s in sessions if s["session_id"] not in taken]
        for pane, session in zip(pending, free):
            assignments[pane["pane_id"]] = session
            taken.add(session["session_id"])
        for pane in pending[len(free):]:
            skips[pane["pane_id"]] = (
                f"only {len(sessions)} muse session(s) for this cwd, "
                f"all already assigned"
            )
    return assignments, skips


def each_with_pause(items, stagger, sleep):
    """Yield (index, item), pausing before every item after the first.

    Keeps N heavy agents from starting in the same instant after a
    restore. A non-positive stagger disables pausing.
    """
    for index, item in enumerate(items):
        if index and stagger > 0:
            sleep(stagger)
        yield index, item


def main(argv):
    cfg = load_config()
    only_pane = None
    args = list(argv)
    while args:
        arg = args.pop(0)
        if arg == "--dry-run":
            cfg["dry_run"] = True
        elif arg == "--pane" and args:
            only_pane = args.pop(0)
        elif arg == "--delay" and args:
            cfg["delay_seconds"] = int(args.pop(0))
        else:
            log(f"WARNING: unknown argument {arg}")

    delay = max(0, cfg["delay_seconds"])
    if delay and not only_pane:
        log(f"waiting {delay}s for restored shells to reach a prompt...")
        time.sleep(delay)

    try:
        panes = list_panes()
    except Exception as exc:
        log(f"ERROR: cannot list panes: {exc}")
        return 1
    log(f"found {len(panes)} pane(s)")

    mapping = load_mapping(state_path())
    eligible = []
    for pane in panes:
        pane_id = pane.get("pane_id", "?")
        cwd = pane.get("cwd") or ""
        if only_pane and pane_id != only_pane:
            continue
        if pane.get("agent"):
            log(f"skip {pane_id} ({cwd}): already runs {pane.get('agent')}")
            continue
        if not cwd:
            log(f"skip {pane_id}: no cwd reported")
            continue
        if cfg["ignore_cwds"] and under(cwd, cfg["ignore_cwds"]):
            log(f"skip {pane_id} ({cwd}): matches ignore_cwds")
            continue
        if cfg["only_cwds"] and not under(cwd, cfg["only_cwds"]):
            log(f"skip {pane_id} ({cwd}): not under only_cwds")
            continue
        eligible.append({"pane_id": pane_id, "cwd": cwd})

    if not eligible:
        log("done, resumed 0 pane(s)")
        return 0

    def fetch(cwd):
        need = sum(1 for p in eligible if norm(p["cwd"]) == norm(cwd))
        return recent_muse_sessions(cwd, limit=max(SESSIONS_PER_CWD, need))

    assignments, skips = assign_sessions(eligible, fetch, mapping)
    for pane_id in sorted(skips):
        pane = next(p for p in eligible if p["pane_id"] == pane_id)
        log(f"skip {pane_id} ({pane['cwd']}): {skips[pane_id]}")

    def describe(pane_id):
        session = assignments[pane_id]
        pane = next(p for p in eligible if p["pane_id"] == pane_id)
        return session, pane, f"muse resume {session['session_id']}"

    resumed = set()
    if cfg["dry_run"]:
        for pane_id in sorted(assignments):
            session, pane, cmd = describe(pane_id)
            log(f"dry-run: would send to {pane_id} ({pane['cwd']}): {cmd}")
            resumed.add(session["session_id"])
    else:
        stagger = max(0, cfg["stagger_seconds"])
        plan = each_with_pause(sorted(assignments), stagger, time.sleep)
        for _, pane_id in plan:
            session, pane, cmd = describe(pane_id)
            try:
                herdr("pane", "run", pane_id, cmd, expect_json=False)
            except Exception as exc:
                log(f"ERROR: cannot resume in {pane_id}: {exc}")
                continue
            resumed.add(session["session_id"])
            mapping[pane_id] = session["session_id"]
            log(f"resumed {pane_id} ({pane['cwd']}): {cmd} "
                f"[{session['session_name']}, {session['prompt_count']} prompts]")

    if not cfg["dry_run"]:
        # Drop mappings for panes that no longer exist; keep the rest so
        # panes skipped this run (e.g. already running) stay stable.
        live = {p.get("pane_id") for p in panes if p.get("pane_id")}
        if only_pane:
            live |= set(mapping)
        save_mapping(state_path(), {k: v for k, v in mapping.items()
                                    if k in live})

    log(f"done, resumed {len(resumed)} pane(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
