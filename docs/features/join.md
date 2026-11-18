# Join: activity marks on telemetry, a recorded history, heat on the towers

Telemetry and the activity city share a clock but run as separate processes.
This package joins them in three ways:

1. Telemetry marks the time axis where commits, errors and delegated-agent
   calls were recorded.
2. Telemetry can record a small history of its own readings to a file
   (`--record`).
3. The city can tint each floor by how hard the machine was working when that
   record was made, and give the machine's energy over a session's span.

## Activity marks on the telemetry time axis

The telemetry view starts the activity collector, the same bounded reader
`--activity` uses, with the same default sources (`HOME`, `WORKSPACE`,
`AGENTBOX_EVENT_ARCHIVE_DIR`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`). It draws a
mark for each 150 s slot of the two-hour window that holds one of these:

| Glyph | Record | City colour |
|-------|--------|-------------|
| `✕` | error (a failed tool call or aborted turn) | red |
| `◆` | recorded commit | gold |
| `↳` | delegated agent or skill call | pink |

- **Where.** In the 3D view, on the row just above the time ruler, under the
  slot the mark belongs to. Marks are bucketed into the ridges' own slots,
  which start when the view starts, not on the wall clock, so a mark stands
  exactly under the spike it may explain and slides left with it. In the
  sparkline view, an `activity` row under the channels puts each mark in that
  slot's sparkline column. The sparkline view shows it only when the pane has
  a spare row.
- **Legend.** The 3D view's second row reads `✕ error ◆ commit ↳ agent · m`,
  then the newest marked slots, for example
  `◆ -1m 1 commit, 1 agent call · agentbox`. The sparkline view puts the same
  line under the `activity` row.
- **Labels** are built only from record kinds, counts (shown up to `99+`) and
  the project's last path component. That name is cleaned of controls and
  invisible characters and cut to 16 columns. A slot with records from
  several projects says `3 projects`. The text of a prompt, command or tool
  call is never used.
- **Caps.** Each slot shows one glyph: the most urgent kind it holds, with
  errors first, then commits, then agent calls. Its label still counts every
  kind. At most 12 slots are marked. When more hold records, the kinds take
  turns: the newest error slot, the newest commit slot, the newest agent slot,
  then the next of each. A run of errors therefore cannot hide every commit.
  The legend gives the rest as `+N more`.
- **Cost.** The collector runs on its own thread, so a first poll that catches
  up on a large history never stalls a frame. It polls every 30 s, **only
  while the view is on screen**, and immediately when the view comes back. Its
  limits are the activity view's: at most 1 MiB per file and 8 MiB per poll,
  256 files and the newest 5,000 records.
- **`m`** hides or shows the marks. **`--no-marks`** never starts the
  collector, so the telemetry view opens no agent history at all. This is the
  behaviour from before this package.
- **`--demo`** marks the synthetic records of `activity_data::demo_events`,
  which fall inside the last two hours, and opens no agent history.

**With a hosts file**, marks describe the records of the machine SystemScape
runs on, and only those. They are drawn only while this machine's ridges are on
screen. Selecting another host hides them, because that host's agent records
are not read. A hosts file that lists no local machine starts no collector.

**This is new behaviour for the telemetry view.** Before this package, the
plain `systemscape` command read only sensors. It now also reads the local
agent histories, bounded and read-only exactly as `--activity` does, unless
`--no-marks` is given.

## `--record`: a recorded telemetry history

**This is the one place SystemScape writes a file, and it does so only when
`--record` is given**, to the telemetry view or to `--probe`.

### What is written, and where

One file:

```
$XDG_STATE_HOME/systemscape/telemetry.jsonl
```

If `XDG_STATE_HOME` is unset or not an absolute path, the file is
`~/.local/state/systemscape/telemetry.jsonl`. The directory is created with
mode `0700` and the file has mode `0600`.

Each closed 150 s slot of **this machine's** readings becomes one JSON line:

```json
{"v":1,"t":1763480100,"secs":150,"n":75,"power_w":412.3,"cpu_pct":63.1,"gpu_temp":71.0,"gpu_util":98.0,"mem_pct":41.2}
```

| Field | Meaning |
|-------|---------|
| `v` | line format version (1) |
| `t` | Unix seconds at which the slot opened, a multiple of 150 (slots are aligned to the wall clock) |
| `secs` | slot length, 150 |
| `n` | sensor polls folded into the slot |
| `power_w` | **mean** whole-system power over the slot (ACPI power meter), so power × time is energy |
| `cpu_pct` | **peak** CPU busy % in the slot |
| `gpu_temp` | **peak** temperature of the hottest card |
| `gpu_util` | **peak** utilisation of the busiest card |
| `mem_pct` | **peak** memory used % |

A reading the machine does not have is left out of the line. The PWR ridge
draws 150 s **peaks** while the file records the **mean**, so the two differ
whenever power moved within a slot. Nothing else is written: no record text,
no process names, no host names.

- **Bounded.** Rows older than 14 days are dropped, which leaves at most 8,064
  lines, about 1 MiB.
- **Atomic.** The file is never opened for writing in place. Every slot, the
  file is read (at most its last 4 MiB) and the new row is added. Rows older
  than 14 days, damaged lines and any older row for the same slot are dropped.
  Lines from a newer version within the 14 days are kept as they are. The
  result goes to a new temporary file beside it (`.telemetry.jsonl.<pid>.tmp`,
  created exclusively with mode `0600`), which is synced and then renamed
  over the final path.
- **No symlinks.** If the final path is a symbolic link, or anything but a
  regular file, nothing is written and the status line says why. A rename
  would replace the link rather than follow it. Refusing leaves your link
  alone as well.
- **One recorder per machine.** The recorder takes an exclusive lock on the
  `systemscape` state directory for as long as it runs. A second recorder (a
  view and a probe both given `--record`, say) writes nothing and says
  `record: another systemscape is already recording here`.
- **When.** A slot is written when the first reading of the next slot arrives.
  The first slot after start-up began part-way through and is never written.
  The slot open when SystemScape quits is lost. The view's sensor thread keeps
  sampling while the view is off screen, so recording continues in a
  background tab.
- **Failures never stop the view.** A directory that cannot be created, a
  read-only filesystem, a missing `HOME` or a link at the final path gives a
  status-line note (`record: …`). The view carries on. A probe prints the
  problem once to stderr and keeps sending readings.
- **The view records only this machine.** With a hosts file that lists no
  local machine, `--record` records nothing. A hosts file's `probe` entry must
  be a plain path, and the viewer starts it as `probe --probe --interval …`
  without `--record`. To record another host, run
  `systemscape --probe --record > /dev/null` on that host itself, for example
  as a user service. Its file is written on that host, and the viewer's own
  probe there runs alongside it, because only recorders take the lock.
- `--record` is refused with `--demo`, so synthetic readings can never reach
  the real history. It is also refused with `--activity`. Recording never
  changes the ridges: each view still starts with an empty two-hour window,
  and the file is not loaded back into them.

### Reading it back

`record::load` reads at most the last 4 MiB, from a regular file only. It
tolerates:

- a missing file, reported as "no recorded history";
- a stale file, whose newest row is days old: the city says which days it
  covers;
- a last line cut off mid-write, which is skipped;
- damaged lines, which are skipped and counted;
- lines from a newer format version, which are skipped and counted. If
  nothing else is left, the city says the history is from a newer
  SystemScape;
- impossible readings, such as negative power or a load above 100 %, which
  are dropped.

When two lines describe the same slot, the later one wins.

### README sentences that need a stated exception

- "No model calls, network requests, database or writes to source histories
  are needed." Still true of source histories. Suggested addition: "The one
  file SystemScape writes is the telemetry history, and only with `--record`
  (see below)."
- "Every machine keeps its own two-hour history from the moment the view
  starts, so switching between them never shows a gap." Still true of the
  ridges. Suggested addition: "That history lives in memory; `--record` also
  appends this machine's slots to a file, which the activity view reads for
  heat."
- "The probe needs no terminal, configuration or privileges, and reads the same
  sources as the local view." Suggested addition: "With `--record` it also
  writes this machine's history to its own state directory."
- "**Background tabs do no work.** … telemetry stops drawing but keeps
  sampling". With `--record` a background telemetry view also rewrites the
  history file once per 150 s slot. Suggested addition: "With `--record`, it
  also writes each closed slot to the history file."
- "SystemScape is designed to work as an immutable image package." Suggested
  addition: "`--record` needs a writable `$XDG_STATE_HOME` or
  `~/.local/state`; without one, telemetry shows a note and records nothing."
- The telemetry view now reads agent histories for its marks (see above).
  "Activity reads Claude Code and Codex session histories…" should gain:
  "the telemetry view reads the same histories for its time-axis marks,
  unless `--no-marks` is given."

## Heat on the towers (`e`)

`e` tints every floor of every tower by how hard the machine was working when
that floor's records were made. It is **off by default**: until `e` is
pressed, the city keeps its colour-by-record-kind view, and the only change is
a dim `e heat` hint at the end of the legend row when there is room.

- **The measure** is the busiest GPU's utilisation in the recorded slot, or
  the CPU load when that slot has no GPU reading. Both are 150 s peaks. A
  floor that merged several records takes the highest measure among their
  slots. The legend row becomes `heat 0% ■■■■■■■■■ 100%  ■ no reading  busiest
  GPU % in the slot, CPU load where no GPU reading · e off`.
- **Gaps stay gaps.** A floor whose records fall in no recorded slot is flat
  grey, shown as "no reading" in the legend. A missing slot is never filled
  from its neighbours. The newest records are usually "no reading" too,
  because a slot is written only once it has closed, up to 150 s later.
- **The day must be covered.** Heat tints only a day the history has closed
  slots for. Otherwise the legend row explains:
  - `e heat: no recorded history · start one with systemscape --record` when
    there is no file;
  - `no recorded history for 2026-11-17 (recorded 11-10 to 11-16)` for a day
    the history does not cover;
  - `cannot read the history: …` when the file cannot be read;
  - `the history is from a newer SystemScape` when only newer lines exist.
- **Reads.** Nothing is read while heat is off. While it is on and the view is
  on screen, the file is read when heat is switched on, then once a minute.
  `--activity --demo` never reads the file. It uses a synthetic history
  covering the demo's two days, with a twenty-minute gap 100 to 120 minutes
  ago, so the tint and "no reading" can both be seen.

### Machine-wide energy over a session's span

With heat on and a history covering the day, the selected session's callout
adds a line such as

```
machine-wide energy over this span: 219 Wh · 1m no reading
```

The detail view (Enter or `f`) puts the full wording after the session
separator:

```
whole machine over this session's span: 219 Wh · 1m no reading · shared by everything running then, not this session's use
```

**This is not the energy a session used.** It is the whole machine's energy
from the session's first record to its last, shared by everything that ran
then: other sessions, other programs, the idle machine itself. A power meter
cannot divide power between sessions that ran at the same time, and SystemScape
does not try.

- Energy is the sum, over recorded slots with a power reading, of mean power ×
  the seconds of the slot inside the span. A slot partly inside the span
  counts for that part, at the slot's mean.
- Time with no power reading counts as nothing and is reported as
  `Nm no reading`. It is never scaled up from the covered part.
- A machine without a power meter shows `no reading`. A session with a single
  record has no span (`span is one moment, nothing to measure`).

## Replay and the clock

Mark bucketing (`activity::marks::bucket`), floor tinting (`heat::Tint`),
coverage (`heat::view`) and energy all take the records and `now` as
arguments and never read the clock. A slot counts only if it had closed by
`now`, and records after `now` are ignored. A replay that passes a cut-off time
through the city's render path therefore sees only the readings and marks that
existed at that moment.

## Keys and flags

| Key / flag | View | Effect |
|------------|------|--------|
| `m` | telemetry | show / hide the activity marks |
| `--no-marks` | telemetry | never start the activity collector |
| `--record` | telemetry, `--probe` | append each closed slot to the history file |
| `e` | activity, city and detail view | heat on the towers and machine-wide energy; a view toggle like `h`, so the tour keeps flying |

`m` and `e` were free on this tree. `--no-marks` is refused with `--probe`;
`--record` is refused with `--demo` and `--activity`.

## Write volume

Each slot rewrites the whole file through a temporary file and a rename,
rather than appending in place. That is what makes the final path safe from
links and partial writes without the `O_NOFOLLOW` flag, which std does not
expose portably. The cost grows with the file: at the 14-day cap, about 1 MiB
is written every 150 s, roughly 0.6 GiB a day. A history only a few days old
costs a fraction of that.

## What this does not claim

- A mark means the activity collector found a record of that kind in that
  slot. It does not show that the record caused anything on the ridges.
- Heat is a recorded peak for the whole machine, not the load a session put
  on it.
- Energy over a span is the machine's, not the session's (see above). It is
  as accurate as the ACPI power meter and the 150 s mean.
- The history has slot resolution. Nothing inside a slot is recorded.

## Help lines and key-table rows to add

Usage lines:

```
Usage: systemscape [--demo | --hosts FILE] [--record] [--no-marks]
       systemscape --probe [--interval SECS] [--gpu-interval SECS] [--record]
```

Options:

```
  --record   append this machine's 150 s slots to $XDG_STATE_HOME/systemscape/telemetry.jsonl
             (the only file SystemScape writes; 14 days kept)
  --no-marks telemetry: do not read agent histories for time-axis marks
```

Telemetry paragraph: `m marks` (commits ◆, errors ✕, agent calls ↳ on the time axis).
Activity paragraph: `e heat` (floors tinted by recorded load; machine-wide energy).

Telemetry key table:

```
| `m` | Show / hide activity marks on the time axis (this machine only) |
```

Activity key table:

```
| `e` | Heat: tint floors by recorded machine load; machine-wide energy over the session | same (energy line) |
```
