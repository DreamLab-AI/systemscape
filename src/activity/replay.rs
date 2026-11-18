//! Replay: one recorded day drawn as it stood at a cut-off time, and the same
//! replay written out as an asciinema recording.
//!
//! Everything that draws takes its history and its `now` from [`window`], so
//! the city, the delegation arcs and shared-file roads, the status dots, the
//! strip and the detail view all agree on the moment shown. Records after the
//! cut-off are dropped from the slice (later days too, as they had not
//! happened yet), and `now` becomes the cut-off, so "active" and the ages read
//! as they would have then. Tower heights use the whole day's scale, so towers
//! grow floor by floor instead of rescaling as the day fills in.
//!
//! A replay is a view of records already read: it opens no files of its own,
//! and nothing advances while the view is off screen.
use super::{
    city, height_cap, layout, raw_height, render, Action, Event, State, GOLD, MUTED, PANEL,
};
use crate::activity_data::clean;
use crate::pixels::PixelCanvas;
use crate::report::{invalid, SourceFlags};
use chrono::SecondsFormat;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::time::Duration;

/// Playback speeds, in recorded seconds per second of viewing.
const SPEEDS: [f64; 5] = [10.0, 60.0, 300.0, 900.0, 3600.0];
/// Five recorded minutes a second: a two-hour day plays in under half a minute.
const DEFAULT_SPEED: f64 = 300.0;

/// A replay in progress. The cut-off is whole milliseconds, the precision
/// records are stored at, so the day's last record is never lost to rounding.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Replay {
    /// The day replayed, `YYYY-MM-DD`; pinned so the view cannot drift to another.
    pub day: String,
    /// Unix milliseconds; records at or before it are shown.
    pub cutoff: i64,
    /// Recorded seconds per second while playing.
    pub speed: f64,
    pub playing: bool,
    /// The live view as it was left, given back on leaving.
    resume: Resume,
}

/// What the live view was showing when the replay began. The replay starts
/// with one session on screen, which takes over the selection, so the user's
/// own choice is kept here.
#[derive(Clone, Debug, Default, PartialEq)]
struct Resume {
    tour: bool,
    day: Option<String>,
    key: String,
    selected: usize,
    record: Option<String>,
}

/// Unix milliseconds of a normalised record timestamp.
fn millis(at: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// The normalised form records use (UTC, milliseconds, `Z`), which sorts as text.
fn stamp(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// Times of the first and last record of `day`, in milliseconds.
fn bounds(events: &[Event], day: &str) -> Option<(i64, i64)> {
    let mut stamps = events
        .iter()
        .filter(|e| e.at.starts_with(day))
        .map(|e| e.at.as_str());
    let first = stamps.next()?;
    let (low, high) = stamps.fold((first, first), |(lo, hi), at| (lo.min(at), hi.max(at)));
    Some((millis(low)?, millis(high)?))
}

/// Every recorded day, newest first.
fn days(events: &[Event]) -> Vec<&str> {
    let days: BTreeSet<&str> = events.iter().filter_map(|e| e.at.get(..10)).collect();
    days.into_iter().rev().collect()
}

/// The records that existed at the replay's cut-off, or all of them when not
/// replaying. The collector's rows are newest first, so the common case is a
/// borrowed tail found by binary search; anything else is filtered.
pub(super) fn cut<'a>(events: &'a [Event], state: &State) -> Cow<'a, [Event]> {
    let Some(replay) = &state.replay else {
        return Cow::Borrowed(events);
    };
    let cutoff = stamp(replay.cutoff);
    if events.is_sorted_by(|a, b| a.at >= b.at) {
        Cow::Borrowed(&events[events.partition_point(|e| e.at > cutoff)..])
    } else {
        Cow::Owned(events.iter().filter(|e| e.at <= cutoff).cloned().collect())
    }
}

/// What every drawing function is handed: the history as it stood, the
/// moment it is seen at, and while replaying the whole day's tower scale and
/// tallest tower, so the city and its camera hold still as towers grow.
pub(super) fn window<'a>(
    events: &'a [Event],
    state: &State,
    now: f64,
) -> (Cow<'a, [Event]>, f64, Option<(f64, f64)>) {
    let Some(replay) = &state.replay else {
        return (Cow::Borrowed(events), now, None);
    };
    (
        cut(events, state),
        replay.cutoff as f64 / 1000.0,
        Some(full_scale(events, &replay.day)),
    )
}

/// Scale and world height of the tallest tower of the whole day. Every
/// session of the day counts, not only the drawn ones, because a session that
/// drops out of the drawn set by the end of the day may be drawn earlier on.
fn full_scale(events: &[Event], day: &str) -> (f64, f64) {
    let mut probe = State {
        day: Some(day.to_owned()),
        ..State::default()
    };
    let whole = city(events, &mut probe);
    let tallest = whole.sessions.iter().map(raw_height).fold(0.0, f64::max);
    // The height cap depends on width over stretch, so stretch 1 stands for all.
    let scale = (height_cap(&layout(&whole, 1.0)) / tallest.max(0.001)).min(1.0);
    (scale, (tallest * scale).max(1.0))
}

/// "5 min/s": how fast recorded time passes.
fn rate(speed: f64) -> String {
    let speed = speed.max(0.0);
    if speed < 60.0 {
        format!("{} s/s", speed.round().max(1.0))
    } else if speed < 3600.0 {
        format!("{} min/s", (speed / 60.0).round())
    } else {
        format!("{} h/s", (speed / 3600.0).round())
    }
}

/// The header's marker, so a replay is never read as live work:
/// `REPLAY 10:42:13 UTC ▶ 5 min/s`.
pub(super) fn banner(state: &State) -> Option<String> {
    let replay = state.replay.as_ref()?;
    let at = stamp(replay.cutoff);
    Some(format!(
        "REPLAY {} UTC {} {}",
        at.get(11..19).unwrap_or("?"),
        if replay.playing { "▶" } else { "‖" },
        rate(replay.speed)
    ))
}

/// While replaying, the bottom row names the replay and its keys in place of
/// the live view's key line.
pub(super) fn footer(state: &State, dims: (usize, usize), rows: &mut [String]) {
    let (Some(replay), Some(banner), Some(last)) = (&state.replay, banner(state), rows.last_mut())
    else {
        return;
    };
    let mut canvas = PixelCanvas::new(dims.0, 1, PANEL, PANEL);
    let lead = format!("{} {}", replay.day, banner);
    canvas.label(1, 0, &lead, GOLD);
    canvas.label(
        lead.chars().count() as i64 + 3,
        0,
        "Space play/pause · ,/. record · </> speed · Home/End · [/] day · r live · q quit",
        MUTED,
    );
    if let Some(row) = canvas.rows().pop() {
        *last = row;
    }
}

/// Is a replay playing? The run loop draws at frame rate while it is.
pub(super) fn playing(state: &State) -> bool {
    state.replay.as_ref().is_some_and(|r| r.playing)
}

/// Move a playing replay on by `dt` seconds of viewing, stopping at the end
/// of the day. A day that has aged out of the history ends the replay.
pub(super) fn advance(state: &mut State, events: &[Event], dt: f64) {
    let Some(replay) = &mut state.replay else {
        return;
    };
    let Some((first, last)) = bounds(events, &replay.day) else {
        leave(state);
        return;
    };
    if replay.playing {
        replay.cutoff += (dt.clamp(0.0, 1.0) * replay.speed * 1000.0) as i64;
    }
    replay.cutoff = replay.cutoff.clamp(first, last);
    if replay.cutoff == last {
        replay.playing = false;
    }
}

/// Start replaying the day on screen from its first record. The tour stops,
/// so it never moves the selection under the user.
fn enter(state: &mut State, events: &[Event]) {
    let day = city(events, state).day;
    let Some((first, _)) = bounds(events, &day) else {
        return;
    };
    state.replay = Some(Replay {
        day: day.clone(),
        cutoff: first,
        speed: DEFAULT_SPEED,
        playing: true,
        resume: Resume {
            tour: state.tour,
            day: state.day.take(),
            key: state.key.clone(),
            selected: state.selected,
            record: state.record.clone(),
        },
    });
    state.day = Some(day);
    state.tour = false;
}

/// Back to the live view as it was left.
fn leave(state: &mut State) {
    if let Some(replay) = state.replay.take() {
        let resume = replay.resume;
        (state.tour, state.day, state.key) = (resume.tour, resume.day, resume.key);
        (state.selected, state.record) = (resume.selected, resume.record);
        state.dwell = 0.0;
    }
}

/// Replay keys. `r` enters and leaves; the rest act only while replaying.
/// Returns `None` for a key the replay does not use, which the live key
/// handling then sees.
pub(super) fn handle_key(state: &mut State, key: KeyEvent, events: &[Event]) -> Option<Action> {
    if !(key.modifiers - KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    if key.code == KeyCode::Char('r') {
        if state.replay.is_some() {
            leave(state);
        } else {
            enter(state, events);
        }
        return Some(Action::Redraw);
    }
    let replay = state.replay.as_mut()?;
    let (first, last) = bounds(events, &replay.day)?;
    let at = stamp(replay.cutoff);
    let day_records = || events.iter().filter(|e| e.at.starts_with(&replay.day));
    match key.code {
        KeyCode::Char(' ') => {
            if !replay.playing && replay.cutoff >= last {
                replay.cutoff = first; // play again from the start
            }
            replay.playing = !replay.playing;
        }
        KeyCode::Char(',') => {
            let previous = day_records()
                .map(|e| e.at.as_str())
                .filter(|t| *t < at.as_str())
                .max();
            replay.cutoff = previous.and_then(millis).unwrap_or(first);
            replay.playing = false;
        }
        KeyCode::Char('.') => {
            let next = day_records()
                .map(|e| e.at.as_str())
                .filter(|t| *t > at.as_str())
                .min();
            replay.cutoff = next.and_then(millis).unwrap_or(last);
            replay.playing = false;
        }
        KeyCode::Char('<') => {
            replay.speed = SPEEDS
                .iter()
                .rev()
                .find(|s| **s < replay.speed)
                .copied()
                .unwrap_or(SPEEDS[0]);
        }
        KeyCode::Char('>') => {
            replay.speed = SPEEDS
                .iter()
                .find(|s| **s > replay.speed)
                .copied()
                .unwrap_or(SPEEDS[SPEEDS.len() - 1]);
        }
        KeyCode::Home => replay.cutoff = first,
        KeyCode::End => {
            replay.cutoff = last;
            replay.playing = false;
        }
        KeyCode::Char('[') | KeyCode::Char(']') => {
            // Days are newest first: `[` is older, `]` newer. At either end,
            // or for a day no longer held, nothing changes; the live day keys
            // never see the press, so the pinned day cannot drift.
            let all = days(events);
            let older = key.code == KeyCode::Char('[');
            let day = all
                .iter()
                .position(|d| *d == replay.day)
                .and_then(|i| {
                    if older {
                        i.checked_add(1)
                    } else {
                        i.checked_sub(1)
                    }
                })
                .and_then(|i| all.get(i))
                .map(|d| (*d).to_owned());
            let Some((day, (start, _))) = day.and_then(|d| Some((d.clone(), bounds(events, &d)?)))
            else {
                return Some(Action::Ignore);
            };
            replay.cutoff = start;
            replay.day = day.clone();
            state.day = Some(day);
            state.key.clear();
            state.selected = 0;
            state.record = None;
        }
        KeyCode::Esc => leave(state),
        _ => return None,
    }
    Some(Action::Redraw)
}

/// Frames per second of a cast.
const CAST_FPS: f64 = 8.0;
/// Upper bounds on a cast whatever the options ask for.
const MAX_FRAMES: usize = 960;
const MAX_BYTES: usize = 24 << 20;
const MAX_LENGTH: f64 = 600.0;

/// What a cast records.
#[derive(Clone, Debug, PartialEq)]
struct CastOptions {
    /// `YYYY-MM-DD`; the newest recorded day when absent.
    day: Option<String>,
    width: usize,
    height: usize,
    /// Seconds of recording from the day's first record to its last.
    length: f64,
}

impl Default for CastOptions {
    fn default() -> Self {
        Self {
            day: None,
            width: 120,
            height: 38,
            length: 20.0,
        }
    }
}

/// `systemscape --activity --cast [--day YYYY-MM-DD] [--size WxH] [--length SECS]`
/// with the usual source flags: an asciicast v2 recording of a replay of one
/// day, written to standard output. No terminal is needed.
pub fn cast(args: &[String]) -> io::Result<()> {
    let (options, events, coverage, demo) = prepare(args)?;
    let lines = recording(&events, &coverage, &options, demo, MAX_BYTES).map_err(invalid)?;
    let mut out = io::stdout().lock();
    let written = lines
        .iter()
        .try_for_each(|line| writeln!(out, "{line}"))
        .and_then(|()| out.flush());
    match written {
        // The reader went away, as `| head` does: not an error.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// Longest wait for the history to drain before a cast is drawn.
const DRAIN_BUDGET: Duration = Duration::from_secs(3);

/// The cast's options and the history it replays. One collector poll reads
/// a bounded share of the tracked files, so a cast built from it would
/// replay part of a day and miss older days entirely; the history is
/// drained first, as `--summary` does, within `DRAIN_BUDGET`.
fn prepare(args: &[String]) -> io::Result<(CastOptions, Vec<Event>, String, bool)> {
    let mut sources = SourceFlags::new();
    let mut options = CastOptions::default();
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if sources.take(arg, &mut rest)? {
            continue;
        }
        let mut value = || {
            rest.next()
                .ok_or_else(|| invalid(format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "--cast" | "--activity" => {}
            "--day" => {
                let day = value()?;
                chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
                    .map_err(|_| invalid("--day needs YYYY-MM-DD"))?;
                options.day = Some(day.clone());
            }
            "--size" => (options.width, options.height) = size(value()?)?,
            "--length" => {
                options.length = value()?
                    .parse::<f64>()
                    .ok()
                    .filter(|l| (1.0..=MAX_LENGTH).contains(l))
                    .ok_or_else(|| invalid(format!("--length needs 1 to {MAX_LENGTH} seconds")))?;
            }
            other => return Err(invalid(format!("unknown cast option {other}; see --help"))),
        }
    }
    let mut sources = sources.open();
    let demo = sources.demo();
    let (events, coverage) = sources.load_drained(DRAIN_BUDGET);
    Ok((options, events, coverage, demo))
}

/// `120x38`, within the sizes the live view draws.
fn size(text: &str) -> io::Result<(usize, usize)> {
    let parsed = text
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .filter(|(w, h): &(usize, usize)| (20..=512).contains(w) && (8..=180).contains(h));
    parsed.ok_or_else(|| invalid("--size needs WIDTHxHEIGHT, 20x8 to 512x180"))
}

/// The state a cast frame is drawn from. Each frame starts afresh, so the
/// callout follows the most recent session at every moment, as a newly
/// opened view would, and the last frame is the whole day as `r` then End shows it.
fn cast_state(day: &str, cutoff: i64, speed: f64, playing: bool) -> State {
    State {
        day: Some(day.to_owned()),
        tour: false,
        replay: Some(Replay {
            day: day.to_owned(),
            cutoff,
            speed,
            playing,
            resume: Resume::default(),
        }),
        ..State::default()
    }
}

/// Bytes that bring a screen showing `front` up to `rows`: only changed rows
/// are rewritten, each placed by an absolute cursor move. Every row ends in
/// an attribute reset, so nothing leaks from one row into the next.
fn frame_diff(front: &[String], rows: &[String]) -> String {
    let mut out = String::new();
    for (y, row) in rows.iter().enumerate() {
        if front.get(y) != Some(row) {
            out.push_str(&format!("\x1b[{};1H\x1b[0m{row}", y + 1));
        }
    }
    out
}

/// The recording as lines: the asciicast v2 header, then one `[time, "o",
/// data]` event per frame that changed anything. The day plays linearly from
/// its first record to its last over `length` seconds; the final frame, the
/// whole day, is always present however the frame and byte caps fall.
fn recording(
    events: &[Event],
    coverage: &str,
    options: &CastOptions,
    demo: bool,
    max_bytes: usize,
) -> Result<Vec<String>, String> {
    let all = days(events);
    let day = match &options.day {
        Some(day) if all.contains(&day.as_str()) => day.clone(),
        Some(day) => return Err(format!("no records on {day}")),
        None => (*all.first().ok_or("no records to replay")?).to_owned(),
    };
    let (first, last) = bounds(events, &day).ok_or("no records to replay")?;
    let length = options.length.clamp(1.0, MAX_LENGTH);
    let frames = ((length * CAST_FPS) as usize).clamp(1, MAX_FRAMES);
    let speed = (last - first) as f64 / 1000.0 / length;
    let dims = (options.width, options.height);
    let draw = |cutoff: i64, playing: bool| {
        let mut state = cast_state(&day, cutoff, speed, playing);
        render(events, coverage, &mut state, dims, demo, 0.0)
    };
    let header = serde_json::json!({
        "version": 2,
        "width": dims.0,
        "height": dims.1,
        "title": clean(&format!("SystemScape replay of {day}")),
        "env": {"TERM": "xterm-256color"},
    });
    let event = |time: f64, data: &str| serde_json::json!([time, "o", data]).to_string();
    let mut lines = vec![header.to_string()];
    let finale = draw(last, false);
    // Room kept for the final frame: a full redraw is its largest possible size.
    let reserve = event(length, &frame_diff(&[], &finale)).len() + 64;
    let mut used = lines[0].len();
    let mut front: Vec<String> = Vec::new();
    for i in 0..frames {
        let cutoff = first + ((last - first) as f64 * i as f64 / frames as f64) as i64;
        let rows = draw(cutoff, true);
        let mut data = frame_diff(&front, &rows);
        if data.is_empty() {
            continue; // nothing changed: the player holds the last frame
        }
        if i == 0 {
            data.insert_str(0, "\x1b[?25l\x1b[2J");
        }
        let line = event(i as f64 * length / frames as f64, &data);
        if used + line.len() + reserve > max_bytes {
            break; // out of room: go straight to the whole day
        }
        used += line.len();
        lines.push(line);
        front = rows;
    }
    let mut data = frame_diff(&front, &finale);
    if front.is_empty() {
        data.insert_str(0, "\x1b[?25l\x1b[2J");
    }
    data.push_str("\x1b[?25h");
    lines.push(event(length, &data));
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::super::tests::plain;
    use super::super::{
        city, city_corners, city_links, city_view, handle_key, scene_frame, select, tour_step,
        unix_now, City, Transform3D, LIVE_SECS,
    };
    use super::*;
    use crate::activity_data::demo_events;

    fn record(key: usize, at: &str, session: &str, kind: &str, text: &str) -> Event {
        Event {
            key: format!("{key:05}"),
            at: at.into(),
            agent: "claude".into(),
            session: session.into(),
            project: "/work/alpha".into(),
            kind: kind.into(),
            text: text.into(),
            source: "fixture".into(),
            parent: None,
            path: None,
        }
    }

    fn press(state: &mut State, events: &[Event], code: KeyCode) -> Action {
        handle_key(
            state,
            KeyEvent::new(code, KeyModifiers::NONE),
            events,
            false,
        )
    }

    fn cutoff(state: &State) -> i64 {
        state.replay.as_ref().unwrap().cutoff
    }

    /// The demo, newest first as the collector hands records over.
    fn sorted_demo() -> Vec<Event> {
        let mut events = demo_events();
        events.sort_by(|a, b| b.at.cmp(&a.at).then(a.key.cmp(&b.key)));
        events
    }

    #[test]
    fn the_window_holds_only_records_up_to_the_cutoff_and_now_is_the_cutoff() {
        for events in [demo_events(), sorted_demo()] {
            let mut state = State::default();
            press(&mut state, &events, KeyCode::Char('r'));
            let replay = state.replay.as_ref().unwrap();
            let (first, last) = bounds(&events, &replay.day).unwrap();
            state.replay.as_mut().unwrap().cutoff = (first + last) / 2;
            let (seen, now, fixed) = window(&events, &state, unix_now());
            let at = stamp((first + last) / 2);
            let expected = events.iter().filter(|e| e.at <= at).count();
            assert!(expected > 0 && expected < events.len());
            assert_eq!(seen.len(), expected);
            assert!(seen.iter().all(|e| e.at <= at));
            assert_eq!(now, ((first + last) / 2) as f64 / 1000.0);
            assert!(fixed.is_some());
            // Not replaying: everything, at the moment given.
            let live = State::default();
            let (all, now, fixed) = super::window(&events, &live, 12.5);
            assert_eq!((all.len(), now, fixed), (events.len(), 12.5, None));
        }
    }

    #[test]
    fn towers_grow_floor_by_floor_at_the_whole_days_scale() {
        let events = sorted_demo();
        let mut state = State::default();
        press(&mut state, &events, KeyCode::Char('r'));
        let day = state.replay.as_ref().unwrap().day.clone();
        let (first, last) = bounds(&events, &day).unwrap();
        let whole = city(&events, &mut State::default());
        let scale = super::super::tower_scale(&whole, &layout(&whole, 1.0));
        let mut floors = 0;
        let mut at = first;
        loop {
            state.replay.as_mut().unwrap().cutoff = at;
            let (seen, _, fixed) = window(&events, &state, 0.0);
            let mut built = city(&seen, &mut state);
            assert_eq!(built.day, day, "the replayed day stays on screen");
            built.fixed = fixed;
            assert!((super::super::tower_scale(&built, &layout(&built, 1.0)) - scale).abs() < 1e-9);
            let count: usize = built.sessions.iter().map(|s| s.records.len()).sum();
            assert!(count > floors, "each record step adds a floor");
            floors = count;
            if at == last {
                break;
            }
            press(&mut state, &events, KeyCode::Char('.'));
            at = cutoff(&state);
        }
        let all: usize = whole.sessions.iter().map(|s| s.records.len()).sum();
        assert_eq!(floors, all, "the end of the replay is the whole day");
    }

    #[test]
    fn status_roads_and_ages_are_read_at_the_cutoff() {
        let mut events = vec![
            record(0, "2026-01-02T10:00:00.000Z", "a", "tool", "Edit x.rs"),
            record(1, "2026-01-02T10:01:00.000Z", "b", "tool", "Read y.rs"),
            record(2, "2026-01-02T11:00:00.000Z", "b", "tool", "Edit x.rs"),
        ];
        for i in [0, 2] {
            events[i].path = Some("/work/alpha/x.rs".into());
        }
        let mut state = State::default();
        press(&mut state, &events, KeyCode::Char('r'));
        press(&mut state, &events, KeyCode::Char('.')); // 10:01, before b's edit
        let (seen, now, _) = window(&events, &state, unix_now());
        let built = city(&seen, &mut state);
        let links = city_links::Links::build(&seen, &built, city_links::Show::default(), now);
        assert!(links.roads.is_empty(), "the shared file is not shared yet");
        let b = built.sessions.iter().find(|s| s.id == "b").unwrap();
        assert_eq!(b.status(now).0, '●', "b had just recorded something");
        assert!(now - b.last < LIVE_SECS);
        press(&mut state, &events, KeyCode::End);
        let (seen, now, _) = window(&events, &state, unix_now());
        let built = city(&seen, &mut state);
        let links = city_links::Links::build(&seen, &built, city_links::Show::default(), now);
        assert_eq!(links.roads.len(), 1);
        assert_eq!(built.sessions[0].status(now).0, '●');
        // Seen live, months later, the same sessions are idle.
        let live = city(&events, &mut State::default());
        assert_eq!(live.sessions[0].status(unix_now()).0, '○');
    }

    #[test]
    fn replay_keys_step_play_and_leave_back_to_the_live_view() {
        let events = sorted_demo();
        let mut state = State::default();
        assert!(state.tour);
        // Outside a replay its keys mean nothing, and the tour flies on.
        assert_eq!(
            press(&mut state, &events, KeyCode::Char(',')),
            Action::Ignore
        );
        let mut state = State::default();
        let live = city(&events, &mut state);
        select(&mut state, &live, 3);
        let chosen = state.key.clone();
        press(&mut state, &events, KeyCode::Char('r'));
        let replay = state.replay.clone().unwrap();
        let (first, last) = bounds(&events, &replay.day).unwrap();
        assert!(replay.playing && !state.tour);
        assert_eq!(
            (replay.cutoff, state.day.as_deref()),
            (first, Some(replay.day.as_str()))
        );
        press(&mut state, &events, KeyCode::Char(' '));
        assert!(!playing(&state));
        press(&mut state, &events, KeyCode::Char('.'));
        let second = cutoff(&state);
        assert!(second > first);
        press(&mut state, &events, KeyCode::Char(','));
        assert_eq!(cutoff(&state), first);
        press(&mut state, &events, KeyCode::Char(','));
        assert_eq!(cutoff(&state), first, "the start holds");
        press(&mut state, &events, KeyCode::End);
        assert_eq!(cutoff(&state), last);
        press(&mut state, &events, KeyCode::Char(' '));
        assert!(
            playing(&state) && cutoff(&state) == first,
            "play at the end starts again"
        );
        press(&mut state, &events, KeyCode::Home);
        assert_eq!(cutoff(&state), first);
        for _ in 0..9 {
            press(&mut state, &events, KeyCode::Char('>'));
        }
        assert_eq!(state.replay.as_ref().unwrap().speed, 3600.0);
        for _ in 0..9 {
            press(&mut state, &events, KeyCode::Char('<'));
        }
        assert_eq!(state.replay.as_ref().unwrap().speed, 10.0);
        // An older day, then back; there is no newer day than the newest.
        let today = state.replay.as_ref().unwrap().day.clone();
        press(&mut state, &events, KeyCode::Char('['));
        let older = state.replay.as_ref().unwrap().day.clone();
        assert!(older < today);
        assert_eq!(state.day.as_deref(), Some(older.as_str()));
        assert_eq!(cutoff(&state), bounds(&events, &older).unwrap().0);
        press(&mut state, &events, KeyCode::Char(']'));
        assert_eq!(state.replay.as_ref().unwrap().day, today);
        assert_eq!(
            press(&mut state, &events, KeyCode::Char(']')),
            Action::Ignore
        );
        assert_eq!(state.replay.as_ref().unwrap().day, today);
        assert_eq!(state.day.as_deref(), Some(today.as_str()));
        // Leaving gives back the live view: following, newest day.
        press(&mut state, &events, KeyCode::Char('r'));
        assert!(state.replay.is_none() && state.tour && state.day.is_none());
        assert_eq!((state.key.as_str(), state.selected), (chosen.as_str(), 3));
        press(&mut state, &events, KeyCode::Char('r'));
        assert_eq!(press(&mut state, &events, KeyCode::Esc), Action::Redraw);
        assert!(
            state.replay.is_none(),
            "Esc leaves a replay rather than quitting"
        );
        assert_eq!(press(&mut state, &events, KeyCode::Esc), Action::Quit);
    }

    #[test]
    fn the_tour_never_moves_the_selection_during_a_replay() {
        let events = sorted_demo();
        let mut state = State::default();
        press(&mut state, &events, KeyCode::Char('r'));
        press(&mut state, &events, KeyCode::End);
        let seen = cut(&events, &state);
        let built = city(&seen, &mut state);
        select(&mut state, &built, 2);
        let key = state.key.clone();
        state.tour = true; // even if something switched it back on
        for _ in 0..50 {
            tour_step(&events, &mut state, 1.0, unix_now());
        }
        assert_eq!(state.key, key);
        // Playing advances by speed, then stops at the end of the day.
        press(&mut state, &events, KeyCode::Home);
        press(&mut state, &events, KeyCode::Char(' '));
        let start = cutoff(&state);
        advance(&mut state, &events, 0.5);
        assert_eq!(cutoff(&state) - start, 150_000, "half a second at 5 min/s");
        advance(&mut state, &events, 30.0); // clamped to one second a step
        assert_eq!(cutoff(&state) - start, 450_000);
        for _ in 0..10_000 {
            advance(&mut state, &events, 1.0);
        }
        let last = bounds(&events, &state.replay.as_ref().unwrap().day)
            .unwrap()
            .1;
        assert!(!playing(&state) && cutoff(&state) == last);
        // A day that leaves the history ends the replay.
        advance(&mut state, &[], 0.1);
        assert!(state.replay.is_none());
    }

    #[test]
    fn a_replay_says_so_with_its_time_and_speed_at_every_size() {
        let events = sorted_demo();
        for (dims, flat) in [
            ((80, 20), false),
            ((120, 38), false),
            ((165, 35), false),
            ((200, 65), false),
            ((47, 27), true),
            ((120, 40), true),
        ] {
            let mut state = State {
                flat,
                ..State::default()
            };
            press(&mut state, &events, KeyCode::Char('r'));
            press(&mut state, &events, KeyCode::Char('.'));
            let time = stamp(cutoff(&state))[11..19].to_owned();
            let rows = render(&events, "fixture", &mut state, dims, true, unix_now());
            assert_eq!(rows.len(), dims.1);
            let text = plain(&rows);
            let lines: Vec<&str> = text.lines().collect();
            assert!(lines[0].contains("REPLAY"), "{dims:?}: header {}", lines[0]);
            assert!(
                lines[dims.1 - 1].contains(&format!("REPLAY {time} UTC")),
                "{dims:?}"
            );
            assert!(!text.contains("FOLLOWING LIVE"), "{dims:?}");
            if dims.0 >= 120 {
                assert!(lines[0].contains(&format!("REPLAY {time} UTC ‖ 5 min/s")));
            }
        }
    }

    #[test]
    fn a_growing_city_stays_in_frame_throughout_the_replay() {
        let events = sorted_demo();
        for dims in [(80usize, 20usize), (120, 38), (165, 35), (512, 180)] {
            let mut state = State::default();
            press(&mut state, &events, KeyCode::Char('r'));
            let (first, last) = bounds(&events, &state.replay.as_ref().unwrap().day).unwrap();
            // Twelve moments through the day, the first record and the last among them.
            for step in 0..=11 {
                state.replay.as_mut().unwrap().cutoff = first + (last - first) * step / 11;
                assert_eq!(
                    render(&events, "f", &mut state, dims, false, 0.0).len(),
                    dims.1
                );
                let (seen, _, fixed) = window(&events, &state, 0.0);
                let mut built: City<'_> = city(&seen, &mut state);
                built.fixed = fixed;
                let strip = (built.sessions.len().min(5) as i64 + 1).min(dims.1 as i64 / 3);
                let bottom = dims.1 as i64 - 1 - strip;
                let (centre, frame) = scene_frame(dims, 2, bottom);
                let (lay, tallest, fitted) = city_view(&built, 0.0, 0.0, centre, frame);
                let (width, height) = PixelCanvas::samples_for(dims.0, dims.1);
                let (width, cell) = (width as i64, height as i64 / dims.1 as i64);
                let viewport = super::super::city_camera(tallest, 0.0, fitted, 0.0, centre);
                for corner in city_corners(&lay, tallest) {
                    let p = super::super::project(&viewport, Transform3D::IDENTITY, corner);
                    assert!(
                        p.x >= 0 && p.x <= width && p.y >= 2 * cell && p.y <= bottom * cell,
                        "{dims:?} step {step} corner {corner:?} at {p:?}"
                    );
                }
            }
        }
    }

    /// Where the next cursor move (`ESC [ row ; 1 H`) starts in `text`, and its row.
    fn next_move(text: &str) -> Option<(usize, usize, usize)> {
        let mut from = 0;
        while let Some(at) = text[from..].find("\x1b[") {
            let start = from + at;
            let after = &text[start + 2..];
            let digits = after.chars().take_while(char::is_ascii_digit).count();
            if digits > 0 && after[digits..].starts_with(";1H") {
                let row = after[..digits].parse().ok()?;
                return Some((start, start + 2 + digits + 3, row));
            }
            from = start + 2;
        }
        None
    }

    /// A screen as a player would hold it, from a recording's frames. Each
    /// row runs from its cursor move to the next one, or to the cursor being
    /// shown again at the very end.
    fn play(lines: &[String], rows: usize) -> Vec<String> {
        let mut screen = vec![String::new(); rows];
        for line in &lines[1..] {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            let data = value[2].as_str().unwrap();
            let data = data.strip_suffix("\x1b[?25h").unwrap_or(data);
            let mut rest = data;
            while let Some((_, body, row)) = next_move(rest) {
                let tail = &rest[body..];
                let end = next_move(tail).map_or(tail.len(), |(start, _, _)| start);
                let text = &tail[..end];
                screen[row - 1] = text.strip_prefix("\x1b[0m").unwrap_or(text).to_owned();
                rest = &tail[end..];
            }
        }
        screen
    }

    #[test]
    fn a_cast_is_asciicast_v2_ending_on_the_whole_day() {
        let events = sorted_demo();
        let options = CastOptions {
            length: 6.0,
            ..CastOptions::default()
        };
        let lines = recording(&events, "fixture", &options, true, MAX_BYTES).unwrap();
        let header: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(
            (header["width"].as_u64(), header["height"].as_u64()),
            (Some(120), Some(38))
        );
        assert!(header["title"]
            .as_str()
            .unwrap()
            .starts_with("SystemScape replay of "));
        let frames = &lines[1..];
        assert!(frames.len() >= 3 && frames.len() <= (6.0 * CAST_FPS) as usize + 1);
        let mut previous = -1.0;
        for line in frames {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            let time = value[0].as_f64().unwrap();
            assert!(time >= previous, "times run forwards");
            previous = time;
            assert_eq!(value[1], "o");
            assert!(value[2].is_string());
        }
        assert_eq!(previous, 6.0);
        // The last screen is the whole day, drawn directly.
        let day = days(&events)[0].to_owned();
        let (first, last) = bounds(&events, &day).unwrap();
        let speed = (last - first) as f64 / 1000.0 / 6.0;
        let mut state = cast_state(&day, last, speed, false);
        let direct = render(&events, "fixture", &mut state, (120, 38), true, 0.0);
        let screen = play(&lines, 38);
        assert_eq!(screen, direct);
        let text = plain(&screen);
        assert!(text.contains("REPLAY") && text.contains("DEMO"));
        // That final moment holds every record of the day.
        let whole = city(&events, &mut State::default());
        let floors: usize = whole.sessions.iter().map(|s| s.records.len()).sum();
        let seen = cut(&events, &state);
        let shown = city(&seen, &mut state);
        assert_eq!(
            shown
                .sessions
                .iter()
                .map(|s| s.records.len())
                .sum::<usize>(),
            floors
        );
    }

    #[test]
    fn a_cast_reads_the_whole_history_not_one_polls_share() {
        // Twelve transcripts of over a megabyte each, one prompt per day at
        // the end of each: more than one poll's eight-megabyte read budget.
        let root = std::env::temp_dir().join(format!("systemscape-cast-{}", std::process::id()));
        let filler = serde_json::json!({"ignored": "x".repeat(1100)}).to_string() + "\n";
        for i in 0..12 {
            let prompt = serde_json::json!({
                "timestamp": format!("2026-09-{:02}T12:00:00Z", i + 1),
                "type": "user", "uuid": format!("u{i}"), "sessionId": format!("s{i}"),
                "message": {"content": "hello"},
            });
            let path = root.join(format!(".claude/projects/p/{i}.jsonl"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, filler.repeat(1000) + &prompt.to_string() + "\n").unwrap();
        }
        let dir = root.to_string_lossy().into_owned();
        let args: Vec<String> = [
            "systemscape",
            "--activity",
            "--cast",
            "--home",
            &dir,
            "--workspace",
            &dir,
            "--archive",
            &format!("{dir}/archive"),
            "--day",
            "2026-09-01",
            "--size",
            "20x8",
            "--length",
            "1",
        ]
        .map(String::from)
        .to_vec();
        let (options, events, coverage, demo) = prepare(&args).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(!demo);
        assert_eq!(days(&events).len(), 12, "every day read: {coverage}");
        assert!(coverage.contains("0 files pending"), "{coverage}");
        for day in days(&events) {
            let options = CastOptions {
                day: Some(day.to_owned()),
                ..options.clone()
            };
            assert!(
                recording(&events, &coverage, &options, demo, MAX_BYTES).is_ok(),
                "{day}"
            );
        }
    }

    #[test]
    fn a_cast_keeps_within_its_caps_and_still_ends_on_the_whole_day() {
        let events = sorted_demo();
        let options = CastOptions {
            length: MAX_LENGTH,
            width: 80,
            height: 24,
            ..CastOptions::default()
        };
        let roomy = recording(&events, "", &options, true, MAX_BYTES).unwrap();
        assert!(roomy.len() - 1 <= MAX_FRAMES + 1);
        let full: usize = roomy.iter().map(String::len).sum();
        let tight = recording(&events, "", &options, true, full / 3).unwrap();
        let bytes: usize = tight.iter().map(String::len).sum();
        assert!(bytes <= full / 3 + 64, "{bytes} bytes");
        assert!(tight.len() < roomy.len());
        assert_eq!(
            play(&tight, 24),
            play(&roomy, 24),
            "both end on the whole day"
        );
        // Options are checked.
        assert_eq!(size("120x38").unwrap(), (120, 38));
        for bad in ["120", "10x38", "120x2", "axb", "9999x40"] {
            assert!(size(bad).is_err(), "{bad}");
        }
        let missing = CastOptions {
            day: Some("1999-01-01".into()),
            ..CastOptions::default()
        };
        assert_eq!(
            recording(&events, "", &missing, true, MAX_BYTES).unwrap_err(),
            "no records on 1999-01-01"
        );
        assert!(recording(&[], "", &CastOptions::default(), true, MAX_BYTES).is_err());
        assert_eq!(rate(300.0), "5 min/s");
        assert_eq!(rate(7200.0), "2 h/s");
        assert_eq!(rate(0.2), "1 s/s");
    }
}
