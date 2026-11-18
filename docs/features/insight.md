# Insight: digests, status line, stuck sessions, MCP and metrics

Insight asks questions of the same records the activity city draws, and
answers without a terminal UI: a day's digest, a status-bar line, an MCP
server for agents and an OpenMetrics endpoint. Every answer is computed by
the pure functions in `src/insight.rs`, so the summary, the status line, the
MCP tools, the metrics and the city's stuck marker agree for the same
records. Like the rest of the activity view, the answers describe what was
logged. They do not show that an agent is live or that a task succeeded.

Days are UTC calendar days (the first ten characters of a record's
timestamp), as in the city. All modes take the activity source flags
`--demo`, `--home PATH`, `--workspace PATH` and `--archive PATH` with the same
defaults as `--activity`.

## `--summary`: a day's digest

```sh
systemscape --activity --summary [--day YYYY-MM-DD] [--json] [--verify-commits]
```

`--summary` prints the coverage line, then totals for the day, then a call-out
for each stuck session (`STUCK`) and contended file (`CONTENDED`). After that
come the projects, most recent first. Each project lists its most recent
sessions with prompts, tool calls, edits, errors, commits and the first and
last record times. Commits are listed under their session. The order is
deterministic. A normal day fits on one screen: at most 6 projects, 5
sessions per project, 5 call-outs of each kind and 4 commits per session are
shown, and each cut list says how many it left out. `--day` defaults to the
newest day with records. `--json` prints `{"coverage", "digest"}` (and
`"commits"` with `--verify-commits`) with nothing cut, every string scrubbed
of controls, directional formatting and invisible characters.

Like `--text` and `--json`, the summary is one bounded collector poll (at most
8 MiB, newest files first), so on a large history the coverage line can
report files still pending. Check it before treating the digest as complete.

## `--status`: one line for tmux or a prompt

```sh
systemscape --status [--tmux] [--gpu] [--no-sensors]
```

The status line prints, for example, `3 active · 1 error · 1 stuck · 71°C 412W`.
Each session is counted once, in the first of these states that applies:

- **stuck**: a stuck run (below) whose last record is under 15 minutes old;
- **error**: the newest record is a failure under 15 minutes old (the city's `!`);
- **active**: the newest record is under 5 minutes old (the city's `●`).

The status line makes one bounded collector poll and one read of the
`/sys/class/hwmon` sensors, which gives the hottest CPU sensor and the power
meter. It takes well under a second. nvidia-smi runs only with `--gpu`, which
adds `GPU 64°C` (the hottest card), bounded at 1.5 s. `--no-sensors` drops
the telemetry part, and `--demo` never reads sensors. The output is plain
text with no escape codes. `--tmux` adds tmux `#[fg=…]` colours. Only
numbers are printed, never record text.

```tmux
set -g status-interval 15
set -g status-right '#(systemscape --status --tmux) %H:%M'
```

## Stuck sessions

A session is stuck when its most recent tool calls repeat one command with a
failure after each, three times running (`insight::STUCK_RUNS`). Prompts in
between do not break a run. A call that did not fail does break it, and so
does a different command. This is a heuristic on logged records. Failures are
paired with calls the way the collector keys them:

- A Claude tool result that reports an error is recorded under its call's key
  plus `:error`, so it pairs with exactly that call, even among parallel calls.
- Any other failure carries no call key and pairs with the tool call it
  immediately follows. In practice that is a Codex aborted turn, or the demo.
- Codex tool output is not recorded, so a Codex session is only seen failing
  through an aborted turn.
- Archive records are steps and failures, never tool calls, so an archive
  session is never marked stuck.

The city marks a stuck session with a red `↻` in the most-recent strip and in
the flat session list, ahead of `!`. Unlike the status line, the city shows
the mark at any age: on an earlier day, a session that ended stuck is still
marked.

## Who is editing a file, and contended files

An edit is a tool call whose name contains `edit`, `write` or `patch` (the
city's edit class) and which recorded a file path. A relative path (Codex
`apply_patch`) is joined to the session's working directory, so it compares
equal to the absolute path Claude records for the same file. An absolute
query matches exactly. A relative query matches whole trailing components:
`src/a.rs` matches `/x/src/a.rs` but not `/x/mysrc/a.rs`. A contended file is
one edited by more than one session on a day. A subagent and its parent count
as two sessions.

## `--mcp`: a read-only MCP server on stdio

```sh
systemscape --mcp [--verify-commits]
```

The server speaks JSON-RPC 2.0, one message per line on stdin and stdout.
Clients on MCP 2025-11-25, 2025-06-18 or 2025-03-26 use `initialize`,
`notifications/initialized` and `ping`. Clients on 2026-07-28, which dropped
the handshake, use `server/discover`, and the server checks the version each
request carries in `_meta` (an unsupported one gets error -32022). The server
also implements `tools/list` and `tools/call`.

| Tool | Arguments | Answer |
|------|-----------|--------|
| `recent_sessions` | `hours` (24, ≤168), `limit` (20, ≤50) | sessions with a record in the window, newest first, with state and newest record |
| `who_is_editing` | `path` (required), `hours` (24, ≤720) | sessions with edit or write records for the file |
| `contended_files` | `day` | files edited by more than one session |
| `stuck_sessions` | `hours` (24, ≤168) | sessions in the window whose latest calls form a stuck run |
| `day_digest` | `day` | the `--summary` digest; with `--verify-commits`, each commit's receipt |
| `telemetry` | `gpu` (true) | one sensor poll; GPU cards cached for ten seconds |

Tool results are data. Every string is scrubbed of controls, directional
formatting and invisible characters and cut to 1,024 characters. Lists stop at
50 entries and report how many were left out. A result over 256 KiB is
refused. Request lines are limited to 1 MiB. Nothing a record contains is
ever run. An unknown tool is a protocol error (-32602). A bad argument value
is a result with `isError`, so the calling model can read it and fix the
call. The collector is kept between calls, so each call reads only what was
appended since the last one. With `--demo`, the records are synthetic and
`telemetry` answers with an error rather than read the machine. A client
configuration looks like this:

```json
{"mcpServers": {"systemscape": {"command": "systemscape", "args": ["--mcp"]}}}
```

## `--metrics ADDR`: OpenMetrics over HTTP

```sh
systemscape --metrics 127.0.0.1:9464 [--metrics-allow-remote] [--gpu]
```

The endpoint serves `GET /metrics` as `application/openmetrics-text`.
Anything else gets 404, 405, 400, 408 or 431. Connections are handled one at
a time, with an 8 KiB request limit and a 5 s limit to send the request and
take the reply. The endpoint binds loopback only. Any address that is not
loopback, including `0.0.0.0` and `[::]`, is refused unless
`--metrics-allow-remote` is also given. It runs headless until interrupted
and does its work per scrape: one bounded collector poll and one sensor poll.
nvidia-smi runs only with `--gpu`, at most every ten seconds.

The families are all gauges:

- `systemscape_temperature_celsius{sensor=cpu|disk|chipset|nic}`
- `systemscape_power_watts`
- `systemscape_cpu_busy_ratio`, `systemscape_memory_used_ratio`
- `systemscape_disk_bytes_per_second`, `systemscape_network_bytes_per_second`
- `systemscape_gpu_temperature_celsius{gpu}`, `systemscape_gpu_busy_ratio{gpu}`,
  `systemscape_gpu_memory_used_bytes{gpu}`, `systemscape_gpu_memory_total_bytes{gpu}`
- `systemscape_activity_sessions{state=active|error|stuck}`
- `systemscape_activity_records{kind}`: the current UTC day's retained records
- `systemscape_activity_contended_files`

Busy ratios and transfer rates need two samples, so they appear from the
second scrape. Record counts are gauges, not counters, because the history
is bounded and drops old records. Labels come from fixed sets, never from
record text. With `--demo`, only the activity families are exported.

## `--verify-commits`: the one exception to "never opens a repository"

With `--verify-commits`, `--summary` (text and `--json`) and the MCP
`day_digest` tool check each recorded commit:

- First `git -C <project> rev-parse --git-dir`, once per project, to confirm
  the directory is a repository.
- Then `git -C <project> cat-file -e <sha>^{commit}`.

Each receipt is marked **present**, **missing** (the repository answered and
does not have it) or **unchecked**. Unchecked covers an invalid SHA or
directory, not a repository, git missing, slow or refusing (for example over
dubious ownership), or a run past the cap. git runs with an argv, never a
shell. The SHA must be 7 to 64 hex digits and the project an existing
absolute directory. Each call has a 2 s timeout and `core.fsmonitor=false`,
and a run makes at most 48 git calls. `--demo` never runs git. Without the
flag, nothing opens a repository.

Two sentences elsewhere must change when this lands:

- `docs/ACTIVITY.md`, line 3: "It never executes commands found in a record,
  opens a repository, calls an API or sends history to a model." This should
  gain: "except that `--verify-commits` asks git whether each recorded commit
  exists".
- `README.md`, the "Coverage" paragraph: "Commit receipts report what the
  transcript recorded and are not checked against current git reachability."
  This should gain: "unless `--verify-commits` is given".

The README sentence "No model calls, network requests, database or writes to
source histories are needed" stays true. `--metrics` listens only when asked,
and `--mcp` uses stdio.

## Not claimed

- A stuck run means a command was logged failing repeatedly, not that the agent
  cannot recover. A failure the collector did not record is invisible.
- Counts are of retained records (newest 5,000), not billing or task totals.
- Contention means two sessions recorded edits to one path on one day. It does
  not show that the edits overlapped or conflicted.
- The status line and the digest are as complete as one bounded poll allows.

## For the lead: help lines and README rows

Help lines to add:

```
       systemscape --activity --summary [--day YYYY-MM-DD] [--json] [--verify-commits]
       systemscape --status [--tmux] [--gpu] [--no-sensors]
       systemscape --mcp [--verify-commits]
       systemscape --metrics ADDR [--metrics-allow-remote] [--gpu]

  --summary  one day's digest: projects, sessions, stuck sessions, contended files
  --status   one short line for a tmux status bar or prompt (--tmux adds colour)
  --mcp      read-only MCP server on stdio for agents
  --metrics  OpenMetrics endpoint at ADDR (loopback unless --metrics-allow-remote)
  --verify-commits  ask git whether each recorded commit exists (opens repositories)
```

Key-table row (activity legend): `↻` marks a stuck session in the most-recent
strip and the flat list; no new key.

`MODES` order matters. The four insight lines sit before `--activity` because
`--activity --summary` must reach `--summary`. Any other package's mode that
is also combined with `--activity` needs the same placement.
