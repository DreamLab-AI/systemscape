# City links: delegation arcs, shared-file roads and the ghost skyline

The activity city (`systemscape --activity`) draws a day of recorded agent work:
district = project, tower = session, floor = record. Three overlays show how
those towers relate to each other and to the day before. Each overlay has a key,
and all three start switched on. `systemscape --activity --demo` shows all three.

## Delegation arcs (`b`)

A Claude subagent's records carry the parent session's ID. Its session is named
`parent/agentId`. When the parent and the subagent both stand in the city, a
pink skywalk arcs from the parent's roof to the subagent's roof. An arc whose
subagent has gone idle is dimmed.

Subagents are now named apart from their parents everywhere a session is
named:

- The callout and the detail-view header show `93bc1f6c/7d1e0c`: eight
  characters of the parent's ID and six of the agent's.
- The most-recent strip and the session table mark a subagent `↳claude 7d1e0c`.
- When a subagent is selected, the detail view adds the line `subagent of claude
  <parent ID>`. When a parent is selected, it adds `started N subagents`.

The layout is unchanged. Within a district the most recent sessions stand at the
front, so a subagent working alongside its parent already stands next to it,
usually directly behind. Moving it elsewhere would break that ordering. When
the subagent is behind its parent, the arc rises over the parent's roof and
leans back. Turning the camera (Left or Right) shows the whole span.

## Shared-file roads (`c`)

Two drawn sessions that both edited or wrote the same file on the day shown are
joined by a road on the ground. A record counts when the city colours it as an
edit (the `edit`, `write` and `patch` tools) and it carries a file path
(`Event.path`). Reads and searches do not count. Paths are made whole before
they are compared, by the same rule `--summary`, the MCP server and the metrics
endpoint use: Codex records paths relative to the project, so `src/x.rs` in
`/work/alpha` matches Claude's `/work/alpha/src/x.rs`. The city's roads and the
summary's contended files therefore name the same files (tested on the demo).

- A road is **red** while both sessions are active, meaning each has a record
  within the last five minutes (`LIVE_SECS`). Otherwise it is a muted grey-rose.
- A road leaves each tower sideways into the lane beside its column, runs along
  the lane, and turns in. Towers in one column stand front to back, so a
  straight road would be hidden behind the front tower. Roads within one column
  alternate sides.
- One road joins each pair of sessions, however many files they share. At most
  six roads are drawn: active pairs first, then the pairs sharing the most
  files. The key gives the rest as `+N more`.
- When the selected session shares a file, the detail view names it. The line
  reads, for example, `shares src/activity.rs with claude 93bc1f6c (both
  active) · 2 more shared`. The path is relative to the project when the file
  is inside it.

Only the drawn towers are compared: the first 36 sessions of the day, the same
set the city draws.

## Ghost skyline (`g`)

Behind today's districts, a faint wireframe outlines the previous recorded day's
towers. That is the newest earlier day with records, not necessarily
yesterday. Each district's outline stands in a row behind the district of the
same name, tallest first, at today's tower scale. Drift shows directly: a
district busier today than last time has taller towers in front of shorter
outlines.

To stay secondary and never move the camera:

- The outlines are one-sample wireframe lines, drawn before the towers so the
  towers stand in front.
- The outlines sit inside the box the camera already fits. Toggling `g`
  therefore never moves or rescales the view, and the whole city stays in frame
  at every pane size (tested).
- An outline taller than today's tallest tower is cut at that height and given a
  notched top. The key counts these (`∨N cut`).
- An earlier day's session whose project has no district in today's city is not
  drawn. Neither is one beyond its district's room (about one outline per 0.6
  tower widths). The key counts both (`+N unplaced`).

## Key line and small panes

The row under the colour legend holds a key, right-aligned, for example
`⌒ b delegation 1  ═ c shared files 5  ▯ g ghost 11-17`. A switched-off overlay
reads `b off`. In a narrow pane the key shortens to `b⌒ 1  c═ 5  g▯`. When the
source coverage line (`?`) takes the row, the key is left out.

Nothing new is drawn where it would not fit:

- Arcs, roads and the key need at least 8 scene rows.
- The ghost needs at least 12. Below that the key says `ghost needs a taller
  pane`. At 80×20 with the strip shown, the ghost appears once `h` hides the
  strip.
- The detail-view line appears only when the list keeps at least three record
  rows. In panes smaller than that the detail view is unchanged.

The three keys are view toggles like `h`: they redraw but leave the tour flying.

## What it does not claim

- A road means two sessions' records both name the file as edited that day. It
  does not show that their edits conflicted, overlapped in time or touched the
  same lines. Relative paths are resolved against the project, but nothing
  else is normalised, so the same file reached through a symlink or a `..`
  is not matched.
- An arc means a subagent's transcript names that parent session. Codex and
  archive sessions record no parent and get no arcs.
- The ghost compares record counts weighted by kind, as tower heights do. It
  does not measure effort or outcome.
- Every overlay is derived from the event slice and a `now` passed in, never the
  clock. A replay that hands in a cut-off history gets overlays as they stood
  at that moment.

## Suggested README and help additions

Key table rows (City column / Detail view column):

```
| `b` | Show / hide delegation arcs | same (and the subagent line) |
| `c` | Show / hide shared-file roads | same (and the shared-file line) |
| `g` | Show / hide the previous day's ghost skyline | |
```

City hint line (bottom row), suggested addition: `b/c/g links`.

Help text line:

```
  b / c / g       activity city: delegation arcs, shared-file roads, ghost skyline
```
