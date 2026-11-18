# Alerts

Threshold rules over both views. A rule fires when its condition holds and
clears when a separate clear condition holds. Each change can ring the bell,
send a desktop notification, or show in tmux. While any rule fires, the view
gives up its top row to a one-line banner naming the firing rules, and `!`
lists every rule with its state.

## The rules file

Rules are read from `$XDG_CONFIG_HOME/systemscape/alerts.toml` (or
`~/.config/systemscape/alerts.toml`) if it exists, or from `--alerts FILE`. An
explicit file must exist. Without a file there are no rules, no banner and no
change to either view. [`docs/alerts.example.toml`](../alerts.example.toml)
documents every field, and a test parses it.

Parsing is strict, in the manner of the hosts file. Problems include:

- an unknown field
- a field that does not belong to the rule's kind
- a duplicate name
- a channel name that could mean two channels (`GPU0`, `CPU`)
- a `clear` level on the wrong side of the threshold
- a rule naming a host or a link that this run's hosts file does not have

Both views read the whole file. What a problem does depends on how the file
was found:

- **`--alerts FILE`** (explicit): the view refuses to start, and the error names
  the rule and the reason.
- **The default file** (found at the default path, no `--alerts`): the view
  starts anyway. It skips each rule that has a problem and keeps the rest. A
  file that does not parse as TOML at all, or cannot be read, gives no rules.
  An amber status note in the top row gives the first reason, then the file,
  for example `ALERTS  rule "gpu1-hot": host "gpu1" is not in the hosts file;
  skipped from ~/.config/systemscape/alerts.toml`. The note stays until `!` opens the
  rule list once, and the list keeps showing every reason. So one default file
  serves both views and every hosts file: a rule for a host this run does not
  watch is simply skipped.

| `when` | View | Fires | Clears |
|---|---|---|---|
| `channel` | telemetry | a channel `above`/`below` its threshold for `for` s | the value reaches `clear` (`<=` for above, `>=` for below) |
| `link-lost` | telemetry | a remote host not live (connecting, stale or down) for `for` s | readings arrive |
| `session-error` | activity | a session's newest record is an error, quiet `for` s (default 60), no older than `within` s (default 600) | the session records again, or the ending is older than `within` |
| `error-burst` | activity | a session with more than `errors` errors in the last `within` s | that session has had no error for `within` s |
| `quiet` | activity | no record from any session for `for` s, once a newer record has been seen while the view runs | the next record |

- **Channels** are named as the view labels them, and case does not matter.
  - The fixed channels are `CPU°`, `GPU0°`, `DISK°`, `PWR`, `LOAD`, `MEM`,
    `GPU0%`, `VRAM0`, `IO` and `NET`. Their units are those shown: °C, W, %,
    VRAM in GiB used, IO and NET in MB/s.
  - Any other name is a named channel, such as `LLM tok`, `LLM busy`, or a
    container's `<name> cpu` / `<name> mem`. It is kept as written (1 to 32
    characters) and matched when a ridge of that name appears.
  - A channel that is absent (not appeared yet, or gone) is a missing reading.
  - A name shaped like a card channel but not one (`GPU1`, `VRAM01`, `GPU°`) is
    a problem, not a named channel.
- **Hysteresis.** `clear` is required and must lie strictly on the safe side of
  the threshold. A value hovering at the threshold therefore fires once, and
  fires again only after it has come back to `clear`. A dip into the band
  between threshold and clear restarts a pending `for` hold.
- **Hosts.** Without `host`, a telemetry rule is evaluated for every host
  separately, each with its own state. `host` must name a host in the hosts
  file. `link-lost` needs at least one remote `[[host]]` and cannot name this
  machine. Without a hosts file, any rule naming a host or a link has a
  problem (see above: refused with `--alerts`, skipped from the default file).
- **Missing readings.** A missing reading (a stale or down host, a card not
  reported yet, a named channel not present) neither fires nor clears a rule.
  A pending hold starts again.
- **Old history.** Activity rules are measured on record timestamps and bounded
  by `within`, so opening the view on a day of history does not set off
  yesterday's errors. `quiet` arms only once it has seen a record newer than its
  `for`. Records whose time does not parse are skipped.

## When rules are evaluated

- **Telemetry** evaluates once a second from the start, whether or not the view
  is on screen. Its sensors keep sampling off screen, so its rules keep
  evaluating there. Off screen the GPU is queried only every 10 s. On the polls
  in between, the last GPU readings are carried forward for up to 25 s, so a
  GPU rule's `for` hold keeps running and a firing GPU rule can clear. When
  nvidia-smi stops answering, the cards read as missing after 25 s rather than
  holding a stale value. A remote host's readings arrive from its
  probe at the host's own interval. Nothing is drawn off screen, so the banner
  appears when the view is next drawn.
- **Activity** evaluates once a second, and only while the view is on screen,
  just after its records are refreshed. Off screen it does no work at all, the
  promise that view already made. No records are read, so no rule is evaluated
  and nothing fires, including `quiet`. On return the view re-reads its records
  first, then evaluates, so something that happened while it was away fires
  then, if it is still within its rule's bounds.
- When the activity view leaves the screen it unsets `@systemscape_activity`,
  because a value it is no longer checking would go stale. Its banner and rule
  states are kept. On return, the first evaluation sets the option again if
  rules still fire. A status line therefore shows activity alerts only while
  that view is on screen. `@systemscape_telemetry` is never unset this way,
  because telemetry keeps evaluating.
- `--text`, `--json`, `--snapshot` and `--probe` evaluate nothing and refuse
  `--alerts`.

## Notifications

`notify` lists any of the following. An empty or absent list means the banner
only.

| Name | What is sent |
|---|---|
| `bell` | BEL. Inside tmux it is not wrapped: tmux handles bells itself (`bell-action`, `visual-bell`, `monitor-bell`). |
| `osc9` | `ESC ] 9 ; text BEL`: a desktop notification in terminals that implement it, for example iTerm2 and WezTerm. |
| `osc777` | `ESC ] 777 ; notify ; SystemScape ; text BEL`: a desktop notification in terminals that implement it, for example urxvt and foot. |
| `tmux-message` | `tmux display-message -t $TMUX_PANE text`, passed as an argv. |
| `tmux-option` | Window option `@systemscape_telemetry` or `@systemscape_activity`, holding the names of firing rules (comma separated) and unset when none fire, and on exit. A status line can show it, for example `set -g window-status-format '#I:#W#{?@systemscape_telemetry, !#{@systemscape_telemetry},}'`. |

**Text.** Every notification reads `systemscape: <rule> fired|cleared[ on host
N]: <numbers>`, for example `systemscape: cpu-hot fired on host 2: 91 above 85
for 60 s`. It is built only from the rule name, the host's number in the host
strip (the 1-9 key) and numbers, never from a host name or record text. Rule
names are limited to letters, digits, spaces, `-`, `_` and `.`, because the text
goes to tmux (which expands `#{…}`) and to terminals (OSC 777 splits on `;`,
and some terminals read `9;<digit>` as a sub-command). The banner and the rule
list, which are drawn rather than sent, do name the host and the channel, after
the usual cleaning.

**tmux passthrough.** Inside tmux, OSC 9 and OSC 777 are meant for the outer
terminal, so they are wrapped in tmux's passthrough sequence
(`ESC P tmux; … ESC \`, with every ESC doubled). tmux forwards that only if
`allow-passthrough` allows it, and SystemScape asks tmux which setting applies
to its pane once, at start-up:

- `all`: the wrapped sequence is sent.
- `on`: tmux forwards it only from a pane that is on screen. While the view is
  on screen the wrapped sequence is sent. Off screen, where telemetry rules can
  still fire, it would be dropped, so the same text goes out as a tmux message
  instead.
- `off` (tmux's default): the text goes out as a tmux message instead.

A rule that already lists `tmux-message` is not sent the message twice. Outside
tmux, OSC goes straight to the terminal, and `tmux-message` and `tmux-option`
do nothing. The rule list says which of these cases applies. To get desktop
notifications through tmux, use `set -g allow-passthrough on` (or `all`).

**Rate limit.** `rate_limit` (seconds, default 60) is the shortest gap between
interrupting notifications (bell, OSC, tmux message) from one rule, counting
fires and clears alike. A notification inside the gap is held back and counted,
and the count shows in the rule list. The tmux option and the banner are state,
not interruptions: they always follow the rules. tmux runs only when the
option's value changes, never on every evaluation.

## Keys and the screen

- **Banner.** While any rule fires, the top row reads
  `▲ ALERT  cpu-hot render-box CPU° 91 > 85 · session-failed 1 session · ! rules`,
  and the view draws in the rows below. The same row carries the amber note on
  skipped rules. The layout (3D or flat ridges, city or record list, and so
  what `j` and `k` do) is chosen from the whole pane. A banner coming and going
  never switches it: only the drawing area loses a row. With nothing firing and
  no note, the view is drawn exactly as before.
- **`!`** opens or closes the rule list over the top of either view: each rule's
  state (`● firing` with how long, `◐ pending`, `○ ok`), its condition, its
  notifiers, any held-back count, how many rules belong to the other view, and
  the tmux note. In activity, `!` does not stop the tour.

## Demo

`--demo` (either view) ignores the rules file and runs built-in rules. It
refuses `--alerts`.

- **Telemetry:** `demo heat` watches `DEMO`, a synthetic signal that exists only
  for the rules (it is not a ridge). The signal crosses 85 for about six seconds
  in every 24, so the rule fires about five seconds after start, clears about
  six seconds later, and repeats. `cpu hot` (real `CPU°` above 95 for 60 s)
  shows a rule that stays ok.
- **Activity:** the fixture's session that ended in repeated failures four
  minutes ago sets off `demo failed` (session-error) and `demo burst`
  (more than 2 errors in 15 min). `demo quiet` stays ok.
- The demo rules notify through `tmux-option` and `tmux-message` only (never
  the bell or OSC), so they can be seen in a tmux capture. The smoke script
  checks the banner, the rule list and the tmux option for both views.

## Limits, and what this does not claim

- An alert reflects what SystemScape reads: telemetry's sampled values and the
  records the activity view can see. It is not a monitoring system. Nothing
  fires while SystemScape is not running, and activity rules do not fire while
  the activity view is off screen.
- The tmux passthrough setting is read once at start. Changing it later takes
  effect on the next start.
- Two SystemScape views of the same kind in one tmux window share one window
  option. Telemetry and activity use different options.
- The tmux option is unset on a normal quit or an unwind. It is not unset if
  the process is killed by a signal.
- The copied example file works on one machine as it stands. Its two rules
  that need a hosts file (`render-box-power`, `host-lost`) are commented out
  until you uncomment them.
- No command hook: a rule cannot run a program.
- Delivery of OSC notifications depends on the terminal. A terminal that does
  not understand OSC 9 or OSC 777 ignores it silently.

## Help lines and key-table rows to add

Help text, usage lines:
```
Usage: systemscape [--demo | --hosts FILE] [--alerts FILE]
       systemscape --activity [--demo] [--alerts FILE] [--text|--json|--snapshot] …
```
Help text, options:
```
  --alerts   alert rules file (default $XDG_CONFIG_HOME/systemscape/alerts.toml, if present)
```
Help text, keys (both views): `! lists alert rules`.

Key table rows (README):

| Key | View | Action |
|---|---|---|
| `!` | both | open or close the alert rule list |

Flag table row (README):

| Flag | Meaning |
|---|---|
| `--alerts FILE` | alert rules file; default `$XDG_CONFIG_HOME/systemscape/alerts.toml` if present; not with `--demo`, `--probe`, `--text`, `--json`, `--snapshot` |
