# Hero GIF — capture notes

Intent: a ~25 s looping GIF for the README hero showing thermal3d in
`--demo` mode doing one graceful partial rotation, so a first-time viewer
sees (1) the six colour-coded walls, (2) the scrolling time axis, and
(3) the depth stacking as the scene turns.

## Recipe A — VHS (preferred, single command)

[VHS](https://github.com/charmbracelet/vhs) renders a scripted terminal
session straight to GIF. A ready tape is in this directory:

```sh
vhs docs/hero.tape
```

## Recipe B — asciinema + agg

```sh
asciinema rec /tmp/t3d.cast -c "./target/release/thermal3d --demo" \
  --cols 160 --rows 40   # let it run ~25 s, then Ctrl-C
agg --font-size 14 --speed 1 /tmp/t3d.cast docs/hero.gif
```

## Recipe C — raw ffmpeg screen grab

Record the terminal window region (X11):

```sh
ffmpeg -f x11grab -framerate 12 -video_size 1280x720 -i :0.0+X,Y -t 25 \
  -vf "fps=12,scale=960:-1:flags=lanczos,split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse" \
  docs/hero.gif
```

## Framing guidance

- Terminal ≈ 160×40 cells, dark background, truecolour enabled.
- `--demo` mode so all walls are fully populated with the synthetic
  correlated-events story (two CPU/load bursts + one GPU/disk event).
- Start capture just before the scene swings through the frontal view.
- Keep the GIF under ~8 MB for a snappy README load: 10–12 fps,
  ~960 px wide is plenty.
