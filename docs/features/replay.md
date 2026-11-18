# Replay, cast export and subagents beside their parent

Three additions to the activity city: a replay scrubber that shows one
recorded day as it stood at a chosen time, `--cast`, which writes that replay
out as an asciinema recording, and a layout rule that stands each subagent's
tower beside its parent's.

## Replay

Press `r` in the activity view to replay the day on screen. The city is drawn
as it stood at a cut-off time: records after the cut-off are left out, so
towers grow floor by floor as the cut-off advances. Replay starts at the day's
first record and plays at five recorded minutes a second.

Everything on screen is read at the cut-off, not the clock: the city, the
delegation arcs and shared-file roads, the status dots (`●` means a record
within five minutes *of the cut-off*), the ages in the strip and the detail
view, and the active count in the header. Tower heights use the whole day's
scale, so towers grow into the final skyline instead of rescaling as the day
fills in, and the camera holds its height.

A replay never passes for live work. The header leads with
`REPLAY 10:42:13 UTC ▶ 5 min/s` in gold (`‖` when paused) instead of
`FOLLOWING LIVE`, and the bottom row names the day, the cut-off, the speed and
the replay keys at every pane size, including the flat view.

| Key | While replaying |
|---|---|
| `r` | Enter replay; press again to go back to the live view |
| `Space` | Play / pause (play at the end of the day starts again from the first record) |
| `,` / `.` | Step back / forward one record, which is one floor; pauses |
| `<` / `>` | Slower / faster: 10 s, 1 min, 5 min, 15 min or 1 h of record time a second |
| `Home` / `End` | Jump to the day's first / last record (`End` pauses) |
| `[` / `]` | Replay the previous / next recorded day, from its first record |
| `Esc` | Leave replay (outside replay `Esc` still quits) |

The other keys work as usual: `j`/`k` and `Tab` choose among the sessions that
existed at the cut-off, `f` and Enter open the detail view, and the camera keys
steer. Entering replay stops the live tour, so automatic selection never moves
the selection while you are watching. Leaving puts back the live view as it
was: following or held, and the day it showed.

Limits:

- "Start of the day" is the day's first record and "end" is its last, not
  midnight. Days are UTC dates, as elsewhere in the view.
- A replay plays only while the view is on screen, like the tour. While it is
  off screen nothing advances. A day that leaves the history (aged out of the
  bounded read) ends the replay.
- Replaying today while agents work: the end of the day is the newest record
  read so far, and it moves as new records arrive.
- A replay shows what was recorded and when. The status at a cut-off is
  derived from timestamps; it does not show what an agent was doing between
  records.

## Cast export

```
systemscape --activity --cast [--day YYYY-MM-DD] [--size WxH] [--length SECS]
            [--demo] [--home PATH] [--workspace PATH] [--archive PATH]
```

This writes an [asciicast v2](https://docs.asciinema.org/manual/asciicast/v2/)
recording of a replay to standard output: a JSON header line (`version` 2,
`width`, `height`, `title`, `env`), then one `[time, "o", data]` line per
frame, `time` being seconds from the start. Frames are drawn by the same code
as the screen, at a fixed size, with no terminal needed, so
`systemscape --activity --cast --demo > city.cast` works from a script. Play
the result with `asciinema play city.cast`.

- `--day` chooses the day (default: the newest recorded day). A day with no
  records is an error.
- `--size` sets the frame size (default `120x38`, from `20x8` to `512x180`).
  Below 80 columns or 20 rows the flat view is recorded, as on screen.
- `--length` is the recording length in seconds, from the day's first record
  to its last (default 20, at most 600).
- Caps: 8 frames a second, at most 960 frames, at most 24 MiB of output. A
  frame repeats only the rows that changed, and a frame that changes nothing is
  left out. The last frame is always the whole day, whatever the caps: if the
  byte cap is reached, the recording jumps straight to it.
- It honours `--demo` and the usual source flags. With `--demo` no agent
  history is opened.

**Recordings contain record text.** The renderer strips terminal controls and
directional formatting, but it does not redact credentials typed into prompts
or commands. Prompts and commands can hold sensitive text, and a cast keeps
every frame. Use `--demo` for anything you share.

Not claimed: the output was checked by parsing it in the tests (header fields,
times that only run forwards, the frame count, and a last frame equal to a
direct render of the whole day). It has not been played in a real asciinema
player here, because asciinema is not installed in this environment.

## Subagents beside their parent

Within a district, towers stand in a grid three deep, filled in order of
recent activity. A subagent that started straight after its parent used to
stand directly behind it, which hid it and turned the delegation arc into a
loop over one tower. Now a parent and its subagents stand as a family along one
row, one column apart. The family's most recent member takes the first free
slot, so the most recent session still stands at the front of the first
column. Districts still run by recent activity, and the caps (`MAX_TOWERS`,
`MAX_DISTRICTS`) are unchanged. A district grows a column when a family needs
one, so a district holding only a parent and its subagent now stands two wide
and one deep. A subagent whose parent is in another district or not drawn keeps
the ordinary order. When a parent has several subagents, they line up after it
in the same row, so only the first stands right beside the parent.

## Suggested README and help additions

Help text lines:

```
       systemscape --activity --cast [--day YYYY-MM-DD] [--size WxH] [--length SECS]
  --cast     asciicast v2 recording of a replay of one day, to stdout (record text included; use --demo to share)
  r          activity: replay the day shown; Space play/pause, ,/. record, </> speed, Home/End, Esc or r live
```

Key table rows (City column / Detail view column):

```
| `r` | Replay the day shown / back to live | same |
| `Space` (replaying) | Play / pause | same |
| `,` / `.` (replaying) | Step one record back / forward | same |
| `<` / `>` (replaying) | Slower / faster | same |
| `Home` / `End` (replaying) | First / last record of the day | same |
```

City hint line (bottom row), suggested addition: `r replay`.
