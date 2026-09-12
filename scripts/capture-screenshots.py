#!/usr/bin/env python3
"""Capture real demo panes; rasterise their ANSI cells for the README.

Requires tmux, Pillow and a monospace TrueType font. No real history is read.
Usage: uv run --with pillow scripts/capture-screenshots.py --font /path/font.ttf
"""
import argparse
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
font = ImageFont.truetype(args.font, 15)
socket = 'systemscape-capture-' + uuid.uuid4().hex[:8]


def tmux(*args):
    return subprocess.check_output(['tmux', '-L', socket, *args], text=True)


def raster(raw, destination):
    cell_w, cell_h, margin = 10, 21, 18
    image = Image.new('RGB', (120 * cell_w + 2 * margin, 40 * cell_h + 2 * margin), '#101722')
    draw = ImageDraw.Draw(image)
    colour = (204, 219, 235)
    for y, line in enumerate(raw.splitlines()[:40]):
        x = 0
        for chunk in re.split(r'(\x1b\[[0-9;]*m)', line):
            if chunk.startswith('\x1b['):
                codes = [int(c) if c else 0 for c in chunk[2:-1].split(';')]
                if len(codes) >= 5 and codes[:2] == [38, 2]:
                    colour = tuple(codes[2:5])
                elif 0 in codes or 39 in codes:
                    colour = (204, 219, 235)
                continue
            for char in chunk:
                if char == '█':
                    draw.rectangle((margin+x*cell_w, margin+y*cell_h, margin+(x+1)*cell_w-1, margin+(y+1)*cell_h-1), fill=colour)
                elif char != ' ':
                    draw.text((margin+x*cell_w, margin+y*cell_h), char, font=font, fill=colour)
                x += 1
    image.save(destination)


try:
    tmux('new-session', '-d', '-s', 'capture', '-x', '120', '-y', '40', f'{binary} --activity --demo')
    time.sleep(0.6)
    for name, key in [('activity-3d', None), ('activity-flat', 'f')]:
        if key:
            tmux('send-keys', '-t', 'capture:0', key)
            time.sleep(0.3)
        raw = tmux('capture-pane', '-p', '-e', '-t', 'capture:0')
        raster(raw, root / 'docs' / f'{name}.png')
    tmux('respawn-pane', '-k', '-t', 'capture:0', f'{binary} --demo')
    time.sleep(0.8)
    tmux('send-keys', '-t', 'capture:0', 'Space')
    time.sleep(0.3)
    raster(tmux('capture-pane', '-p', '-e', '-t', 'capture:0'), root / 'docs' / 'telemetry.png')
finally:
    subprocess.run(['tmux', '-L', socket, 'kill-server'], check=False, capture_output=True)
