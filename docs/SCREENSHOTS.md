# Reproducing the README screenshots

The README PNGs are rasterisations of ANSI cells captured from real 200×65 tmux
panes. The activity tour is held for each complete capture. They are not concept art. Activity uses `--demo`; telemetry uses `--demo`
for its history and still polls the local system for its live values.

```sh
cargo build --locked --release
uv run --with pillow scripts/capture-screenshots.py --font /path/to/DejaVuSansMono.ttf
```

Requirements: tmux, Python with Pillow, and a monospace TrueType font. These are
documentation tools and are not dependencies of the SystemScape binary. The script
creates a uniquely named tmux server, captures the activity city and detail view,
then 3D and sparkline telemetry,
then removes only its own server. It opens no real agent histories.

For an ANSI frame on a terminal, `systemscape --activity --demo --snapshot` exits
after rendering. Use `--text` or `--json` for redirected output.
