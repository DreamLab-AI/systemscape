#!/usr/bin/env python3
"""Exercise the release binary in an isolated real tmux terminal."""
from pathlib import Path
import subprocess
import time
import uuid

binary = Path(__file__).resolve().parents[1] / 'target/release/systemscape'
socket = 'systemscape-smoke-' + uuid.uuid4().hex[:8]


def tmux(*args, check=True):
    return subprocess.run(['tmux', '-L', socket, *args], check=check,
                          capture_output=True, text=True).stdout


def capture():
    return tmux('capture-pane', '-p', '-t', 'test:0')


try:
    tmux('new-session', '-d', '-s', 'test', '-x', '120', '-y', '40', f'{binary} --activity --demo')
    time.sleep(0.6)
    first = capture()
    assert 'FOLLOWING LIVE' in first and 'DEMO' in first
    for expected in ('MOST RECENT', 'VisionFlow', 'agentbox', 'systemscape', '▀', '●', ' ago'):
        assert expected in first, f'activity city missing: {expected}'
    assert '[ + ]' not in first and 'No action in sight' not in first, 'centre box must be gone'
    time.sleep(1.2)
    assert capture() != first, 'tour camera did not move'
    tmux('send-keys', '-t', 'test:0', 'j')
    time.sleep(0.3)
    paused = capture()
    assert 'MANUAL' in paused
    assert [l for l in paused.splitlines() if l.startswith(' > ')] != \
        [l for l in first.splitlines() if l.startswith(' > ')], 'j did not move to the next session'
    tmux('send-keys', '-t', 'test:0', 'Space')
    time.sleep(0.3)
    assert 'FOLLOWING LIVE' in capture()
    tmux('send-keys', '-t', 'test:0', 'Enter')
    time.sleep(0.3)
    detail = capture()
    assert 'records ──' in detail and 'Project:' in detail, 'Enter opens the detail list'
    assert 'prompt' in detail and 'MOST RECENT' not in detail
    tmux('send-keys', '-t', 'test:0', 'j')
    tmux('send-keys', '-t', 'test:0', 'Enter')
    time.sleep(0.3)
    assert 'Source: synthetic demo' in capture()
    tmux('send-keys', '-t', 'test:0', 'f')
    time.sleep(0.3)
    assert 'MOST RECENT' in capture(), 'f returns to the city'
    tmux('resize-window', '-t', 'test:0', '-x', '60', '-y', '20')
    time.sleep(0.3)
    small = capture()
    assert 'records ──' in small and 'MOST RECENT' not in small, 'small panes show the detail list'
    # A tab in the background does no work; it catches up when shown again.
    tmux('resize-window', '-t', 'test:0', '-x', '120', '-y', '40')
    tmux('send-keys', '-t', 'test:0', 'f')
    tmux('send-keys', '-t', 'test:0', 'f')
    tmux('send-keys', '-t', 'test:0', 'Space')
    time.sleep(0.4)
    assert 'FOLLOWING LIVE' in capture()
    pid = tmux('list-panes', '-t', 'test:0', '-F', '#{pane_pid}').strip()
    ticks = lambda: sum(int(v) for v in Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[11:13])
    tmux('send-keys', '-t', 'test:0', '\x1b[O')
    time.sleep(0.4)
    before, frozen = ticks(), capture()
    time.sleep(1.5)
    assert capture() == frozen and ticks() - before <= 1, 'unfocused activity view kept working'
    tmux('send-keys', '-t', 'test:0', '\x1b[I')
    time.sleep(0.6)
    assert capture() != frozen, 'activity view did not resume on focus'
    tmux('resize-window', '-t', 'test:0', '-x', '20', '-y', '8')
    time.sleep(0.3)
    assert tmux('list-panes', '-t', 'test:0', '-F', '#{pane_dead}').strip() == '0'
    tmux('send-keys', '-t', 'test:0', 'q')
    time.sleep(0.3)
    result = subprocess.run(['tmux', '-L', socket, 'has-session', '-t', 'test'], capture_output=True)
    assert result.returncode != 0, 'quit left the renderer running'
    tmux('new-session', '-d', '-s', 'test', '-x', '120', '-y', '40', f'{binary} --demo')
    time.sleep(0.6)
    first = capture()
    for expected in ('SYSTEMSCAPE', 'CPU°', 'GPU0°', 'GPU2%', 'VRAM1', 'NET', '-1h', 'now'):
        assert expected in first, f'telemetry label missing: {expected}'
    assert '▀' in first, 'half-block ridges missing'
    time.sleep(0.6)
    assert capture() != first, 'telemetry flight did not move'
    tmux('send-keys', '-t', 'test:0', 'Space')
    time.sleep(0.3)
    held = capture()
    assert 'HOLDING' in held
    tmux('send-keys', '-t', 'test:0', 'Space')
    time.sleep(0.3)
    pid = tmux('list-panes', '-t', 'test:0', '-F', '#{pane_pid}').strip()
    tmux('send-keys', '-t', 'test:0', '\x1b[O')
    time.sleep(0.4)
    before, frozen = ticks(), capture()
    time.sleep(1.5)
    assert capture() == frozen and ticks() - before <= 3, 'unfocused telemetry kept drawing'
    tmux('send-keys', '-t', 'test:0', '\x1b[I')
    time.sleep(0.6)
    assert capture() != frozen, 'telemetry did not resume on focus'
    tmux('send-keys', '-t', 'test:0', 'Space')
    tmux('resize-window', '-t', 'test:0', '-x', '47', '-y', '27')
    time.sleep(0.3)
    narrow = capture()
    assert 'GPU1%' in narrow and '▁' in narrow, 'narrow panes show per-channel sparklines'
    tmux('resize-window', '-t', 'test:0', '-x', '20', '-y', '8')
    time.sleep(0.3)
    assert tmux('list-panes', '-t', 'test:0', '-F', '#{pane_dead}').strip() == '0'
    tmux('send-keys', '-t', 'test:0', 'q')
    time.sleep(0.3)
    assert subprocess.run(['tmux', '-L', socket, 'has-session', '-t', 'test'], capture_output=True).returncode != 0
    print('PASS: activity city, session select, detail list, source, small-pane list; telemetry ridges, labels, hold and narrow sparklines; both views idle while unfocused and resume; tiny resize and quit')
finally:
    tmux('kill-server', check=False)
