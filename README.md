# herdr-muse-resume

Codex-style automatic session resume for [Muse](https://dev.meta.ai/docs/muse-code)
(`muse`) panes in [Herdr](https://herdr.dev).

## The problem

Herdr restores workspaces, tabs, panes, and cwds after a server restart (e.g.
reboot) — but it only **auto-resumes agents it knows how to launch** (codex,
claude, ...). Muse is *detected* but has no native resume support, so restored
Muse panes come back as empty shells while Codex panes pick up where they left
off.

## What this plugin does

A Herdr startup hook that runs once after every session restore and sends
`muse resume <session-id>` to each plain-shell pane whose cwd has Muse
history. Session ids are read (read-only) from Muse's `session-index.db`:
the most recent `valid` sessions for that workspace — the same pool
`muse resume --last` picks from.

Panes sharing one cwd each get their own distinct session (newest first), so
three shells in `/repo` resume the three most recent sessions for `/repo`
instead of fighting over the newest one. Assignments persist in
`resume-state.json` next to the plugin config, so each pane gets its same
session back on the next restart; panes whose mapped session went stale fall
back to the newest free session.

Automatically skipped:

- panes already running an agent (e.g. natively resumed codex/claude),
- cwds with no Muse history,
- extra panes when more panes share a cwd than there are sessions for it
  (each session attaches at most once per run).

## Install

Requires Herdr 0.9+ and `python3` (stdlib only, no dependencies):

```bash
herdr plugin install yavuzhansarmaz/herdr-muse-resume
```

Takes effect on the next Herdr server start. Check what it did after a restart:

```bash
herdr plugin log list --plugin muse.resume
```

Disable/remove without deleting anything:

```bash
herdr plugin disable muse.resume
herdr plugin unlink muse.resume
```

## Config (optional)

Create `<config-dir>/config.toml` (find the dir with
`herdr plugin config-dir muse.resume`):

```toml
delay_seconds = 5        # wait for restored shells to reach a prompt
ignore_cwds = ["/tmp"]   # never resume under these paths
only_cwds = []           # if non-empty, resume only under these paths
dry_run = false          # log actions without running them
```

The hook also writes `resume-state.json` into that config dir to remember
which pane got which session. Delete it to let the next restart reassign
from newest to oldest.

## Compatibility

Tested on Linux with Herdr 0.9.1 and Muse 1.4.0. macOS is declared and the
code is portable (bash + python3 stdlib), but not yet verified there —
reports welcome.

## Relation to herdr-muse

[akshat12/herdr-muse](https://github.com/akshat12/herdr-muse) reports Muse
pane state (idle/working/blocked) to Herdr via Muse lifecycle hooks. It does
not (and cannot, without Herdr core support) auto-resume sessions. This plugin
solves the resume side; the two are complementary and can be installed
together.

## How it is tested

`resume.py` supports `--dry-run` and `--pane <id>` for safe manual runs, and
`tests/` holds stdlib-only regression tests
(`python3 -m unittest discover -s tests -t .`).
End-to-end verified with an isolated named Herdr session: two workspaces
created (one cwd with Muse history, one without), server stopped and
restarted — the hook resumed Muse in the first pane and left the second as a
plain shell. Multi-pane sharing one cwd is covered by the assignment tests
plus a read-only check against a real `session-index.db` (three panes in one
cwd resolve to three distinct sessions). See the script header for details.

## License

MIT — see [LICENSE](LICENSE).
