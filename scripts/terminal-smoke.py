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
    return tmux('capture-pane', '-p', '-t', 'test:0.0')


def start(command):
    """New session plus a control-mode client, so the pane counts as on screen."""
    tmux('new-session', '-d', '-s', 'test', '-x', '120', '-y', '40', command)
    tmux('set', '-g', 'window-size', 'manual')
    tmux('set', '-g', 'focus-events', 'on')
    client = subprocess.Popen(['tmux', '-L', socket, '-C', 'attach', '-t', 'test'],
                              stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
    tmux('resize-window', '-t', 'test:0', '-x', '120', '-y', '40')
    return client


def ticks():
    pid = tmux('list-panes', '-t', 'test:0', '-F', '#{pane_pid}').splitlines()[0].strip()
    fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
    return int(fields[11]) + int(fields[12])


def off_screen_then_back(name):
    """Another window takes the screen: no work. Selecting it again resumes."""
    tmux('new-window', '-t', 'test')
    time.sleep(2.6)  # one visibility check
    before, frozen = ticks(), capture()
    time.sleep(2.0)
    assert capture() == frozen, f'{name} drew while off screen'
    assert ticks() - before <= 3, f'{name} kept working off screen'
    tmux('select-window', '-t', 'test:0')
    time.sleep(0.8)
    assert capture() != frozen, f'{name} did not resume on screen'
    tmux('kill-window', '-t', 'test:1')


try:
    client = start(f'{binary} --activity --demo')
    time.sleep(0.8)
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
    tmux('resize-window', '-t', 'test:0', '-x', '120', '-y', '40')
    tmux('send-keys', '-t', 'test:0', 'Space')  # f held the tour; resume it
    time.sleep(0.4)
    assert 'FOLLOWING LIVE' in capture()
    off_screen_then_back('activity')
    # Visible beside a focused split pane: the tour holds, ages keep updating.
    tmux('split-window', '-h', '-t', 'test:0')
    time.sleep(0.6)
    held = capture()
    time.sleep(2.2)
    assert capture() != held, 'visible split pane stopped updating'
    tmux('kill-pane', '-t', 'test:0.1')
    tmux('resize-window', '-t', 'test:0', '-x', '20', '-y', '8')
    time.sleep(0.3)
    assert tmux('list-panes', '-t', 'test:0', '-F', '#{pane_dead}').strip() == '0'
    tmux('send-keys', '-t', 'test:0', 'q')
    time.sleep(0.3)
    result = subprocess.run(['tmux', '-L', socket, 'has-session', '-t', 'test'], capture_output=True)
    assert result.returncode != 0, 'quit left the renderer running'
    client.terminate()
    client = start(f'{binary} --demo')
    time.sleep(0.8)
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
    off_screen_then_back('telemetry')
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
    print('PASS: activity city, session select, detail list, source, small-pane list; telemetry ridges, labels, hold and narrow sparklines; both views idle off screen, keep updating in a visible split pane, and resume; tiny resize and quit')
finally:
    tmux('kill-server', check=False)
