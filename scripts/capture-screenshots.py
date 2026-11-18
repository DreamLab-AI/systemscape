#!/usr/bin/env python3
"""Capture real demo panes and a replay; rasterise their ANSI cells for the README.

Requires tmux, Pillow and a monospace TrueType font. No real history is read.
Usage: uv run --with pillow scripts/capture-screenshots.py --font /path/font.ttf
"""
import argparse
import json
from pathlib import Path
import re
import subprocess
import time
import uuid
from PIL import Image, ImageDraw, ImageFont

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--font', required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
binary = root / 'target/release/systemscape'
font = ImageFont.truetype(args.font, 11)
socket = 'systemscape-capture-' + uuid.uuid4().hex[:8]


def tmux(*args):
    return subprocess.check_output(['tmux', '-L', socket, *args], text=True)


DEFAULT_FG, DEFAULT_BG = (204, 219, 235), (16, 23, 34)
# Lower blocks, as eighths of a cell filled from the bottom (sparklines).
EIGHTHS = {c: i + 1 for i, c in enumerate('▁▂▃▄▅▆▇█')}


def draw_frame(lines, cols, rows):
    """Rasterise rows of ANSI truecolour cells to an image."""
    cell_w, cell_h, margin = 7, 14, 18
    image = Image.new('RGB', (cols * cell_w + 2 * margin, rows * cell_h + 2 * margin), '#101722')
    draw = ImageDraw.Draw(image)
    for y, line in enumerate(lines[:rows]):
        colour, background = DEFAULT_FG, DEFAULT_BG
        x = 0
        for chunk in re.split(r'(\x1b\[[0-9;]*m)', line):
            if chunk.startswith('\x1b['):
                codes = [int(c) if c else 0 for c in chunk[2:-1].split(';')]
                k = 0
                while k < len(codes):
                    code = codes[k]
                    if code in (38, 48) and codes[k+1:k+2] == [2] and k+4 < len(codes):
                        rgb = tuple(codes[k+2:k+5])
                        if code == 38: colour = rgb
                        else: background = rgb
                        k += 5
                        continue
                    if code in (0, 39): colour = DEFAULT_FG
                    if code in (0, 49): background = DEFAULT_BG
                    if code == 40: background = (0, 0, 0)
                    k += 1
                continue
            for char in chunk:
                if x >= cols:
                    break
                left, top = margin + x * cell_w, margin + y * cell_h
                right, bottom = left + cell_w - 1, top + cell_h - 1
                draw.rectangle((left, top, right, bottom), fill=background)
                if char == '▀':
                    draw.rectangle((left, top, right, top + cell_h // 2 - 1), fill=colour)
                elif char in EIGHTHS:
                    draw.rectangle((left, bottom + 1 - cell_h * EIGHTHS[char] // 8, right, bottom), fill=colour)
                elif char != ' ':
                    draw.text((left, top), char, font=font, fill=colour)
                x += 1
    return image


def raster(raw, destination):
    draw_frame(raw.splitlines(), 200, 65).save(destination)


def replay_gif(destination, cols=120, rows=38, seconds=12, step=2):
    """Turn `--activity --cast --demo` into a looping GIF.

    A cast frame holds only the rows that changed, each addressed by a cursor
    move, so the rows are kept between frames.
    """
    cast = subprocess.check_output(
        [binary, '--activity', '--cast', '--demo', '--size', f'{cols}x{rows}', '--length', str(seconds)],
        text=True).splitlines()
    screen, frames, times = [''] * rows, [], []
    for line in cast[1:]:
        at, _, data = json.loads(line)
        parts = re.split(r'\x1b\[(\d+);1H', data)
        for row, text in zip(parts[1::2], parts[2::2]):
            if 1 <= int(row) <= rows:
                screen[int(row) - 1] = re.sub(r'\x1b\[\?[0-9]+[hl]|\x1b\[2J', '', text)
        frames.append(list(screen))
        times.append(at)
    picked = list(range(0, len(frames), step))
    if picked[-1] != len(frames) - 1:
        picked.append(len(frames) - 1)
    images = [draw_frame(frames[i], cols, rows).quantize(colors=128, dither=Image.Dither.NONE) for i in picked]
    # Hold the finished day before the loop restarts.
    durations = [int(1000 * seconds / len(frames) * step)] * (len(images) - 1) + [2500]
    images[0].save(destination, save_all=True, append_images=images[1:], duration=durations, loop=0, optimize=True)


def key(*keys, wait=0.5):
    for k in keys:
        tmux('send-keys', '-t', 'capture:0', k)
        time.sleep(0.12)
    time.sleep(wait)


def shot(name):
    raster(tmux('capture-pane', '-p', '-e', '-N', '-t', 'capture:0'), root / 'docs' / f'{name}.png')


try:
    tmux('new-session', '-d', '-s', 'capture', '-x', '200', '-y', '65', f'{binary} --activity --demo')
    # A control-mode client makes the pane count as on screen, so views keep
    # drawing; a fixed window size stops the client resizing the capture.
    tmux('set', '-g', 'window-size', 'manual')
    client = subprocess.Popen(['tmux', '-L', socket, '-C', 'attach', '-t', 'capture'],
                              stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
    tmux('resize-window', '-t', 'capture:0', '-x', '200', '-y', '65')
    time.sleep(1.5)
    key('Space')            # hold the tour so every capture is of a settled frame
    shot('activity-3d')     # arcs, shared-file roads, ghost skyline, alert banner
    key('e')
    shot('activity-heat')   # floors tinted by recorded machine load
    key('e', 'Enter')
    shot('activity-flat')
    key('f', 'r', wait=15.0)  # replay plays from the start of the day
    key('Space')            # pause part-way through
    shot('activity-replay')
    key('r', '!')
    shot('alerts')          # the rule list
    tmux('respawn-pane', '-k', '-t', 'capture:0', f'{binary} --demo')
    time.sleep(7.0)         # long enough for the demo alert rule to fire
    key('Space')
    shot('telemetry')       # ridges, activity marks, VRAM processes, anomaly flag
    # Narrow panes, and `f`, give one sparkline per channel.
    key('f')
    shot('telemetry-flat')
    replay_gif(root / 'docs' / 'replay.gif')
finally:
    subprocess.run(['tmux', '-L', socket, 'kill-server'], check=False, capture_output=True)
