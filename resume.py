#!/usr/bin/env python3
"""Herdr startup hook: resume Muse sessions in restored panes.

Herdr natively auto-resumes agents it knows how to launch (codex, claude,
...). Muse is detected but has no native resume support, so restored Muse
panes come back as plain shells. This hook closes that gap:

  1. After Herdr restores the session, list all panes.
  2. Skip panes that already run something (agent != null).
  3. For each plain-shell pane, look up the most recent valid Muse session
     for that pane's cwd in Muse's session-index.db.
  4. Run `muse resume <session-id>` in the pane via `herdr pane run`.

Safety:
  - Read-only access to Muse's session index (never modifies sessions).
  - Each Muse session is resumed at most once per run (dedup by session id),
    so two panes sharing one cwd don't attach to the same session twice.
  - Panes whose cwd has no Muse history are left untouched.
  - Honors optional config at $HERDR_PLUGIN_CONFIG_DIR/config.toml:
        delay_seconds = 5        # wait for restored shells to reach a prompt
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


def log(msg):
    print(f"[muse-resume] {msg}", flush=True)


def load_config():
    cfg = {
        "delay_seconds": 5,
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


def latest_muse_session(cwd):
    """Most recent valid Muse session for cwd, or None.

    Mirrors `muse resume --last` workspace scoping: exact workspace match,
    newest first. Read-only; an empty/missing index simply yields None.
    """
    data_home = os.environ.get("XDG_DATA_HOME") or "~/.local/share"
    candidates_db = [
        os.path.expanduser(os.path.join(data_home, "muse/session-index.db")),
        os.path.expanduser("~/.local/share/muse/session-index.db"),
        os.path.expanduser(
            "~/Library/Application Support/muse/session-index.db"
        ),
    ]
    db_path = next(
        (p for p in candidates_db if os.path.isfile(p)), None
    )
    if db_path is None:
        return None
    candidates = {cwd, norm(cwd)}
    try:
        db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        try:
            placeholders = ",".join("?" for _ in candidates)
            row = db.execute(
                f"""SELECT session_id, session_name, prompt_count
                    FROM sessions
                    WHERE status = 'valid'
                      AND workspace_root IN ({placeholders})
                    ORDER BY updated_at_us DESC LIMIT 1""",
                sorted(candidates),
            ).fetchone()
        finally:
            db.close()
    except Exception as exc:
        log(f"WARNING: could not query muse session index: {exc}")
        return None
    if not row:
        return None
    return {"session_id": row[0], "session_name": row[1],
            "prompt_count": row[2]}


def under(path, prefixes):
    path = norm(path)
    return any(
        path == norm(prefix) or path.startswith(norm(prefix).rstrip("/") + "/")
        for prefix in prefixes
    )


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

    resumed = set()
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
        session = latest_muse_session(cwd)
        if not session:
            log(f"skip {pane_id} ({cwd}): no muse history here")
            continue
        if session["session_id"] in resumed:
            log(f"skip {pane_id} ({cwd}): session "
                f"{session['session_name']} already resumed this run")
            continue
        cmd = f"muse resume {session['session_id']}"
        if cfg["dry_run"]:
            log(f"dry-run: would send to {pane_id} ({cwd}): {cmd}")
            resumed.add(session["session_id"])
            continue
        try:
            herdr("pane", "run", pane_id, cmd, expect_json=False)
        except Exception as exc:
            log(f"ERROR: cannot resume in {pane_id}: {exc}")
            continue
        resumed.add(session["session_id"])
        log(f"resumed {pane_id} ({cwd}): {cmd} "
            f"[{session['session_name']}, {session['prompt_count']} prompts]")

    log(f"done, resumed {len(resumed)} pane(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
