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
history. The session id is read (read-only) from Muse's `session-index.db`:
the most recent `valid` session for that workspace — the same one
`muse resume --last` would pick.

Automatically skipped:

- panes already running an agent (e.g. natively resumed codex/claude),
- cwds with no Muse history,
- a session already resumed in another pane during the same run (no double
  attach when two panes share one cwd).

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

`resume.py` supports `--dry-run` and `--pane <id>` for safe manual runs.
End-to-end verified with an isolated named Herdr session: two workspaces
created (one cwd with Muse history, one without), server stopped and
restarted — the hook resumed Muse in the first pane and left the second as a
plain shell. See the script header for details.

## License

MIT — see [LICENSE](LICENSE).
