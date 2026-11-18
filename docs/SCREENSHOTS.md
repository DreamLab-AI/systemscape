# Reproducing the README screenshots

The README PNGs are rasterisations of ANSI cells captured from real 200×65 tmux
panes, and `docs/replay.gif` is the output of `--activity --cast --demo`
(120×38, 12 seconds) drawn by the same rasteriser. The activity tour is held for each complete capture. They are not concept art. Activity uses `--demo`; telemetry uses `--demo`
for its history and still polls the local system for its live values.

```sh
cargo build --locked --release
uv run --with pillow scripts/capture-screenshots.py --font /path/to/DejaVuSansMono.ttf
```

Requirements: tmux, Python with Pillow, and a monospace TrueType font. These are
documentation tools and are not dependencies of the SystemScape binary. The script
creates a uniquely named tmux server and captures, in order: the activity city
(`activity-3d`), the city with heat on (`activity-heat`), the detail view
(`activity-flat`), a replay paused part-way through the day
(`activity-replay`), the alert rule list (`alerts`), then 3D and sparkline
telemetry (`telemetry`, `telemetry-flat`). It then removes only its own server
and writes the replay GIF. It opens no real agent histories, and under `--demo`
the heat history and alert rules are synthetic too.

The demo's timestamps are relative to the moment of capture, so the replay
capture shows whatever point of the day the replay has reached after fifteen
seconds; the others are stable from run to run apart from live sensor values.

For an ANSI frame on a terminal, `systemscape --activity --demo --snapshot` exits
after rendering. Use `--text` or `--json` for redirected output.
