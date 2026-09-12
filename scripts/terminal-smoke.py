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
    time.sleep(0.5)
    first = capture()
    assert 'FLYING TOUR' in first
    time.sleep(1.2)
    assert capture() != first, 'tour camera did not move'
    tmux('send-keys', '-t', 'test:0', 'Left')
    time.sleep(0.3)
    paused = capture()
    assert 'MANUAL' in paused
    time.sleep(1.2)
    assert capture() == paused, 'manual mode changed without input or data'
    tmux('send-keys', '-t', 'test:0', 'Space')
    time.sleep(0.3)
    assert 'FLYING TOUR' in capture()
    tmux('send-keys', '-t', 'test:0', 'Enter')
    time.sleep(0.3)
    assert 'Source:' in capture()
    tmux('send-keys', '-t', 'test:0', 'f')
    time.sleep(0.3)
    assert ' | ' in capture(), 'flat record rows missing'
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
    assert '2h history' in first and 'NOW' in first
    time.sleep(0.6)
    assert capture() != first, 'telemetry flight did not move'
    ansi = tmux('capture-pane', '-e', '-p', '-t', 'test:0')
    assert '48;2;0;0;0' in ansi or '[40m' in ansi, 'black panel missing'
    tmux('resize-window', '-t', 'test:0', '-x', '20', '-y', '8')
    time.sleep(0.3)
    assert tmux('list-panes', '-t', 'test:0', '-F', '#{pane_dead}').strip() == '0'
    tmux('send-keys', '-t', 'test:0', 'q')
    time.sleep(0.3)
    assert subprocess.run(['tmux', '-L', socket, 'has-session', '-t', 'test'], capture_output=True).returncode != 0
    print('PASS: activity tour, manual pause, resume, source, flat; telemetry flight and black panel; tiny resize and quit in both modes')
finally:
    tmux('kill-server', check=False)
