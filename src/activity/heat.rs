//! Heat on the towers: how hard this machine was working when each floor's
//! records were made, from the recorded telemetry history (`--record`).
//!
//! The measure is the busiest GPU's utilisation in the recorded slot, or the
//! CPU load when that slot has no GPU reading, both as the slot's peak. A
//! floor takes the highest measure among the slots its records fall in. A
//! floor whose records fall in no recorded slot has no reading and is drawn
//! a flat grey: gaps are never filled from the slots either side.
//!
//! The selected session's callout and detail view give the machine's energy
//! over that session's time span. That is the whole machine's energy while
//! the session ran, shared with everything else running then; power cannot
//! be divided between sessions, and the wording on screen says so.
//!
//! Every function that colours or measures takes the records and `now` as
//! arguments and never reads the clock, so a replay that hands in a cut-off
//! history and an old `now` sees only the readings recorded before it.
use super::{seconds, Mark, LEGEND, MUTED, PANEL};
use crate::activity_data::{clean, Event};
use crate::pixels::{mix, PixelCanvas, Rgb};
use crate::telemetry::record::{self, History, Loaded, Row};
use crossterm::event::KeyCode;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// How often the history file is read again while heat is on and visible.
const RELOAD: Duration = Duration::from_secs(60);
/// A floor with no reading: no tint at all.
pub(super) const NO_READING: Rgb = (72, 78, 90);
/// The ramp from idle to flat out.
const RAMP: [Rgb; 3] = [(38, 82, 170), (238, 178, 52), (255, 58, 48)];

/// Whether heat is on, and the history it reads.
#[derive(Default)]
pub(super) struct Heat {
    /// Toggled with `e`; off by default, so the city keeps its record colours.
    pub(super) on: bool,
    /// `--demo`: a synthetic history, never the file.
    demo: bool,
    path: Option<PathBuf>,
    loaded: Option<Loaded>,
    read_at: Option<Instant>,
}

impl Heat {
    pub(super) fn new(demo: bool) -> Self {
        Self {
            demo,
            path: if demo { None } else { record::default_path() },
            ..Self::default()
        }
    }

    /// Read the history when heat is on and the last read is a minute old.
    /// Nothing is read while heat is off. Returns whether a read happened.
    pub(super) fn refresh(&mut self, now: f64) -> bool {
        if !self.on || self.read_at.is_some_and(|at| at.elapsed() < RELOAD) {
            return false;
        }
        self.read_at = Some(Instant::now());
        self.loaded = Some(if self.demo {
            Loaded::Read(demo_history(now))
        } else {
            record::load(self.path.as_deref())
        });
        true
    }
}

/// Flip heat with `e`. Returns whether the key was `e`.
pub(super) fn toggle(heat: &mut Heat, key: KeyCode) -> bool {
    if key != KeyCode::Char('e') {
        return false;
    }
    heat.on = !heat.on;
    true
}

/// What heat can show for one day.
pub(super) enum View<'a> {
    Off,
    /// On, but there is nothing to tint with; the text says why.
    Unavailable(String),
    Ready(Tint<'a>),
}

/// Readings for one render: the history, and the moment readings stop.
pub(super) struct Tint<'a> {
    history: &'a History,
    now: f64,
    /// Each session's stack, worked out on its first floor this render.
    stacks: RefCell<HashMap<StackKey, Rc<Stack>>>,
}

/// A session's record list (its address and length) and the scale it is
/// stacked at. A `Tint` lives for one render, while the records cannot move.
type StackKey = (usize, usize, u64);

/// A session's records as the city stacks them: where each record's part of
/// the tower ends, and the machine's measure when it was made.
struct Stack {
    /// Top of each record's part, rising from the ground.
    tops: Vec<f64>,
    measures: Vec<Option<f64>>,
}

/// The heat view for UTC day `day` (`YYYY-MM-DD`) as of `now`.
pub(super) fn view<'a>(heat: &'a Heat, day: &str, now: f64) -> View<'a> {
    if !heat.on {
        return View::Off;
    }
    let history = match &heat.loaded {
        None => return View::Unavailable("reading the history…".into()),
        Some(Loaded::Absent) => {
            return View::Unavailable(
                "no recorded history · start one with systemscape --record".into(),
            )
        }
        Some(Loaded::Failed(reason)) => {
            return View::Unavailable(format!("cannot read the history: {}", clean(reason)))
        }
        Some(Loaded::Read(history)) => history,
    };
    let tint = Tint::new(history, now);
    let start = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc().timestamp() as f64);
    let on_day = |r: &&Row| {
        start.is_some_and(|start| r.t as f64 >= start && (r.t as f64) < start + 86_400.0)
    };
    if history.rows.iter().filter(on_day).any(|r| tint.usable(r)) {
        return View::Ready(tint);
    }
    let reason = if history.rows.iter().any(|r| on_day(&r)) {
        // The day was recorded, but `now` (a replay's cut-off) is before
        // its first slot closed.
        "no recorded slot had closed yet at this time".to_owned()
    } else if history.rows.is_empty() && history.newer > 0 {
        "the history is from a newer SystemScape".to_owned()
    } else {
        match (history.rows.first(), history.rows.last()) {
            (Some(first), Some(last)) => format!(
                "no recorded history for {day} (recorded {} to {})",
                date(first.t),
                date(last.t)
            ),
            _ => "the recorded history is empty".to_owned(),
        }
    };
    View::Unavailable(reason)
}

fn date(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map_or_else(|| "?".into(), |d| d.format("%m-%d").to_string())
}

/// How hard the machine was working in a slot, 0 to 1: the busiest GPU,
/// else the CPU.
fn measure(row: &Row) -> Option<f64> {
    row.gpu_util
        .or(row.cpu_pct)
        .map(|v| (v / 100.0).clamp(0.0, 1.0))
}

/// The ramp colour of a measure from 0 to 1.
fn ramp(n: f64) -> Rgb {
    let n = n.clamp(0.0, 1.0);
    if n < 0.5 {
        mix(RAMP[0], RAMP[1], n * 2.0)
    } else {
        mix(RAMP[1], RAMP[2], (n - 0.5) * 2.0)
    }
}

impl<'a> Tint<'a> {
    pub(super) fn new(history: &'a History, now: f64) -> Self {
        Self {
            history,
            now,
            stacks: RefCell::default(),
        }
    }

    /// A slot counts once it had closed by `now`.
    fn usable(&self, row: &Row) -> bool {
        row.end() as f64 <= self.now
    }

    /// The reading for a record made at unix second `at`, if its slot was recorded.
    fn reading(&self, at: f64) -> Option<&Row> {
        (at <= self.now)
            .then(|| self.history.at(at))
            .flatten()
            .filter(|r| self.usable(r))
    }

    /// The session's stack: each record's top and measure, in one pass that
    /// parses each timestamp once, kept for the rest of this render.
    fn stack(&self, records: &[&Event], scale: f64) -> Rc<Stack> {
        let key = (records.as_ptr() as usize, records.len(), scale.to_bits());
        if let Some(stack) = self.stacks.borrow().get(&key) {
            return Rc::clone(stack);
        }
        let mut y = 0.0;
        let mut stack = Stack {
            tops: Vec::with_capacity(records.len()),
            measures: Vec::with_capacity(records.len()),
        };
        for record in records {
            // The same sum, in the same order, as the city's floors.
            y += Mark::of(record).weight() * scale;
            stack.tops.push(y);
            stack
                .measures
                .push(self.reading(seconds(&record.at)).and_then(measure));
        }
        let stack = Rc::new(stack);
        self.stacks.borrow_mut().insert(key, Rc::clone(&stack));
        stack
    }

    /// The highest measure among the records of a session (oldest first)
    /// whose part of the stack overlaps the floor from `span.0` to `span.1`.
    /// Records stack in order, each `weight × scale` tall, exactly as the
    /// city's floors do, so a merged floor covers the records it merged.
    /// Only the floor's own records are visited.
    pub(super) fn floor_measure(
        &self,
        records: &[&Event],
        scale: f64,
        span: (f64, f64),
    ) -> Option<f64> {
        let stack = self.stack(records, scale);
        // Every weight is positive, so the tops rise and the first record
        // reaching above the floor's base is found by bisection.
        let first = stack.tops.partition_point(|top| *top <= span.0 + 1e-9);
        let mut found: Option<f64> = None;
        for i in first..stack.tops.len() {
            let bottom = if i == 0 { 0.0 } else { stack.tops[i - 1] };
            if bottom >= span.1 - 1e-9 {
                break;
            }
            if let Some(n) = stack.measures[i] {
                found = Some(found.map_or(n, |f: f64| f.max(n)));
            }
        }
        found
    }

    /// A floor's colour: the ramp, or flat grey with no reading.
    pub(super) fn floor(
        &self,
        records: &[&Event],
        scale: f64,
        span: (f64, f64),
        selected: bool,
    ) -> Rgb {
        let base = self
            .floor_measure(records, scale, span)
            .map_or(NO_READING, ramp);
        if selected {
            mix(base, (255, 255, 255), 0.14)
        } else {
            base
        }
    }

    /// The machine's energy from `from` to `to` (unix seconds), cut at `now`.
    pub(super) fn energy(&self, from: f64, to: f64) -> Energy {
        let to = to.min(self.now);
        let mut energy = Energy {
            span: (to - from).max(0.0),
            ..Energy::default()
        };
        for row in self.history.rows.iter().filter(|r| self.usable(r)) {
            let overlap = (row.end() as f64).min(to) - (row.t as f64).max(from);
            if overlap <= 0.0 {
                continue;
            }
            if let Some(watts) = row.power_w {
                energy.wh += watts * overlap / 3600.0;
                energy.covered += overlap;
            }
        }
        energy
    }
}

/// Machine-wide energy over a span, and how much of the span had a reading.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Energy {
    pub(super) wh: f64,
    /// Seconds of the span with a power reading.
    pub(super) covered: f64,
    pub(super) span: f64,
}

impl Energy {
    fn amount(&self) -> String {
        if self.wh >= 1000.0 {
            format!("{:.2} kWh", self.wh / 1000.0)
        } else {
            format!("{:.0} Wh", self.wh)
        }
    }

    /// The uncovered part, for example ` · 12m no reading`.
    fn gap(&self) -> String {
        let missing = ((self.span - self.covered) / 60.0).round() as u64;
        if missing > 0 {
            format!(" · {missing}m no reading")
        } else {
            String::new()
        }
    }

    /// One line for the callout.
    pub(super) fn short(&self) -> String {
        if self.span < 1.0 {
            "machine-wide energy: span is one moment, nothing to measure".into()
        } else if self.covered <= 0.0 {
            "machine-wide energy over this span: no reading".into()
        } else {
            format!(
                "machine-wide energy over this span: {}{}",
                self.amount(),
                self.gap()
            )
        }
    }

    /// One line for the detail view, with the disclaimer in full.
    pub(super) fn long(&self) -> String {
        if self.span < 1.0 || self.covered <= 0.0 {
            return self.short();
        }
        format!(
            "whole machine over this session's span: {}{} · shared by everything running then, not this session's use",
            self.amount(),
            self.gap()
        )
    }
}

/// The selected session's energy line, when heat is ready. Callers pick
/// `Energy::short` or `Energy::long`.
pub(super) fn session_energy(view: &View<'_>, records: &[&Event]) -> Option<Energy> {
    let View::Ready(tint) = view else {
        return None;
    };
    let first = seconds(&records.first()?.at);
    let last = seconds(&records.last()?.at);
    Some(tint.energy(first, last))
}

/// The legend row. With heat on it replaces the record colours with the
/// ramp, the no-reading swatch and the measure's name, or says why there is
/// nothing to show. With heat off it adds `e heat` at the end, when there is room.
pub(super) fn legend(canvas: &mut PixelCanvas, view: &View<'_>, row: i64, columns: i64) {
    let blank = |canvas: &mut PixelCanvas| {
        canvas.label(0, row, &" ".repeat(columns.max(0) as usize), PANEL);
    };
    match view {
        View::Off => {
            let used: i64 = LEGEND
                .iter()
                .map(|m| m.name().len() as i64 + 4)
                .sum::<i64>()
                + "floors: oldest at ground, newest on top".len() as i64
                + 1;
            let hint = "e heat";
            if used + 2 + hint.len() as i64 <= columns {
                canvas.label(columns - 1 - hint.len() as i64, row, hint, MUTED);
            }
        }
        View::Unavailable(reason) => {
            blank(canvas);
            canvas.label(1, row, &format!("e heat: {reason}"), MUTED);
        }
        View::Ready(_) => {
            blank(canvas);
            canvas.label(1, row, "heat 0%", MUTED);
            let mut col = 9;
            for step in 0..=8 {
                canvas.label(col, row, "■", ramp(f64::from(step) / 8.0));
                col += 1;
            }
            canvas.label(col + 1, row, "100%", MUTED);
            canvas.label(col + 7, row, "■", NO_READING);
            canvas.label(col + 9, row, "no reading", MUTED);
            canvas.label(
                col + 21,
                row,
                "busiest GPU % in the slot, CPU load where no GPU reading · e off",
                MUTED,
            );
        }
    }
}

/// `--activity --demo`: a synthetic history covering the demo's records,
/// with a twenty-minute gap so "no reading" can be seen.
pub(super) fn demo_history(now: f64) -> History {
    let slot = record::SLOT_SECS;
    let last = (now / slot as f64).floor() as i64 * slot - slot; // the newest closed slot
    let gauss = |t: f64, c: f64, w: f64| (-((t - c) / w).powi(2)).exp();
    let mut rows = Vec::new();
    for k in 0..(30 * 3600 / slot) {
        let t = last - k * slot;
        let minutes = (now - t as f64) / 60.0;
        if (100.0..120.0).contains(&minutes) {
            continue; // the gap
        }
        let jitter = ((k as f64 * 12.9898).sin() * 43_758.545).fract().abs();
        let busy = gauss(minutes, 8.0, 10.0)
            + gauss(minutes, 70.0, 25.0) * 0.8
            + gauss(minutes, 1600.0, 40.0);
        let gpu = (4.0 + 92.0 * busy.min(1.0) + 4.0 * jitter).min(100.0);
        let cpu = (6.0 + 50.0 * busy.min(1.0) + 10.0 * jitter).min(100.0);
        rows.push(Row {
            v: record::VERSION,
            t,
            secs: slot,
            n: 75,
            power_w: Some(180.0 + 4.0 * gpu + 1.5 * cpu),
            cpu_pct: Some(cpu),
            gpu_temp: Some(35.0 + 0.45 * gpu),
            gpu_util: Some(gpu),
            mem_pct: Some(42.0),
        });
    }
    rows.reverse();
    History {
        rows,
        ..History::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(t: i64, gpu: Option<f64>, cpu: Option<f64>, power: Option<f64>) -> Row {
        Row {
            v: record::VERSION,
            t,
            secs: 150,
            n: 75,
            power_w: power,
            cpu_pct: cpu,
            gpu_util: gpu,
            ..Row::default()
        }
    }

    fn event(at: f64, kind: &str) -> Event {
        Event {
            key: at.to_string(),
            at: chrono::DateTime::from_timestamp_millis((at * 1000.0) as i64)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            agent: "claude".into(),
            session: "s".into(),
            project: "/p".into(),
            kind: kind.into(),
            text: "Bash cargo test".into(),
            source: "fixture".into(),
            parent: None,
            path: None,
        }
    }

    #[test]
    fn a_floor_takes_the_peak_measure_of_its_records_and_a_gap_stays_grey() {
        let base = 1_763_337_600; // a UTC midnight
        let history = History {
            rows: vec![
                row(base, Some(20.0), Some(90.0), None), // GPU wins over CPU
                row(base + 150, None, Some(60.0), None), // no GPU: CPU load
                // base + 300 is a gap
                row(base + 450, Some(100.0), None, None),
            ],
            ..History::default()
        };
        let tint = Tint::new(&history, base as f64 + 10_000.0);
        let events: Vec<Event> = [10.0, 160.0, 310.0, 460.0]
            .iter()
            .map(|dt| event(base as f64 + dt, "tool"))
            .collect();
        let records: Vec<&Event> = events.iter().collect();
        // Each tool record is 0.5 tall at scale 1.
        let at = |from: f64, to: f64| tint.floor_measure(&records, 1.0, (from, to));
        assert_eq!(at(0.0, 0.5), Some(0.2));
        assert_eq!(at(0.5, 1.0), Some(0.6));
        assert_eq!(at(1.0, 1.5), None, "a record in a gap has no reading");
        assert_eq!(at(1.0, 2.0), Some(1.0), "a merged floor takes the peak");
        assert_eq!(at(0.0, 1.0), Some(0.6));
        assert_eq!(tint.floor(&records, 1.0, (1.0, 1.5), false), NO_READING);
        assert_ne!(tint.floor(&records, 1.0, (0.0, 0.5), false), NO_READING);
    }

    #[test]
    fn readings_after_now_are_not_seen_as_in_a_replay() {
        let base = 1_763_337_600;
        let history = History {
            rows: vec![
                row(base, Some(50.0), None, Some(300.0)),
                row(base + 150, Some(90.0), None, Some(600.0)),
            ],
            ..History::default()
        };
        let events = [
            event(base as f64 + 10.0, "tool"),
            event(base as f64 + 200.0, "tool"),
        ];
        let records: Vec<&Event> = events.iter().collect();
        // `now` inside the second slot: that slot had not closed yet.
        let tint = Tint::new(&history, base as f64 + 250.0);
        assert_eq!(tint.floor_measure(&records, 1.0, (0.5, 1.0)), None);
        assert_eq!(tint.floor_measure(&records, 1.0, (0.0, 0.5)), Some(0.5));
        let energy = tint.energy(base as f64, base as f64 + 300.0);
        assert_eq!(energy.span, 250.0, "the span is cut at now");
        assert_eq!(energy.covered, 150.0);
        assert!((energy.wh - 300.0 * 150.0 / 3600.0).abs() < 1e-9);
    }

    #[test]
    fn energy_counts_only_covered_time_and_says_so() {
        let base = 1_763_337_600;
        let history = History {
            rows: vec![
                row(base, None, None, Some(360.0)),
                row(base + 150, None, None, None), // no power meter reading
                row(base + 600, None, None, Some(744.0)),
            ],
            ..History::default()
        };
        let tint = Tint::new(&history, base as f64 + 100_000.0);
        let energy = tint.energy(base as f64 + 75.0, base as f64 + 675.0);
        assert_eq!(energy.covered, 150.0, "75 s of each powered slot");
        assert!((energy.wh - (360.0 * 75.0 + 744.0 * 75.0) / 3600.0).abs() < 1e-9);
        let short = energy.short();
        assert!(
            short.starts_with("machine-wide energy over this span: 23 Wh"),
            "{short}"
        );
        assert!(short.contains("8m no reading"), "{short}");
        let long = energy.long();
        assert!(
            long.contains("whole machine") && long.contains("not this session's use"),
            "{long}"
        );
        for wrong in ["used by", "session used", "consumed by"] {
            assert!(!long.contains(wrong) && !short.contains(wrong));
        }
        let none = tint.energy(base as f64 + 300.0, base as f64 + 450.0);
        assert_eq!(
            none.short(),
            "machine-wide energy over this span: no reading"
        );
        assert!(tint
            .energy(base as f64, base as f64)
            .short()
            .contains("one moment"));
    }

    #[test]
    fn the_view_explains_a_missing_stale_or_newer_history() {
        let mut heat = Heat {
            on: true,
            ..Heat::default()
        };
        let day = "2025-11-17";
        let midnight = 1_763_337_600.0;
        assert!(matches!(view(&heat, day, midnight), View::Unavailable(_)));
        heat.loaded = Some(Loaded::Absent);
        let View::Unavailable(text) = view(&heat, day, midnight) else {
            panic!("no history should not be ready");
        };
        assert!(text.contains("systemscape --record"), "{text}");
        heat.loaded = Some(Loaded::Read(History {
            rows: vec![row(midnight as i64 - 86_400 * 3, Some(10.0), None, None)],
            ..History::default()
        }));
        let View::Unavailable(text) = view(&heat, day, midnight + 3600.0) else {
            panic!("a stale history does not cover the day");
        };
        assert!(
            text.contains("no recorded history for 2025-11-17"),
            "{text}"
        );
        heat.loaded = Some(Loaded::Read(History {
            newer: 3,
            ..History::default()
        }));
        let View::Unavailable(text) = view(&heat, day, midnight) else {
            panic!("a newer history is not read");
        };
        assert!(text.contains("newer"), "{text}");
        heat.loaded = Some(Loaded::Read(History {
            rows: vec![row(midnight as i64 + 600, Some(10.0), None, None)],
            ..History::default()
        }));
        assert!(matches!(
            view(&heat, day, midnight + 3600.0),
            View::Ready(_)
        ));
        let View::Unavailable(text) = view(&heat, day, midnight + 700.0) else {
            panic!("a slot still open at `now` does not count");
        };
        assert_eq!(text, "no recorded slot had closed yet at this time");
        let View::Unavailable(text) = view(&heat, day, midnight + 60.0) else {
            panic!("early in a replay of a recorded day");
        };
        assert!(!text.contains("no recorded history"), "{text}");
        heat.on = false;
        assert!(matches!(view(&heat, day, midnight + 3600.0), View::Off));
    }

    /// One session of `n` records over the last two hours, in runs of reads,
    /// shell calls and edits with an occasional prompt and error, so the
    /// tower has many floors of different heights.
    fn tall_tower(n: usize, now: f64) -> Vec<Event> {
        (0..n)
            .map(|i| {
                let mut record = event(now - 7_200.0 + 7_190.0 * i as f64 / n as f64, "tool");
                record.key = format!("r{i}");
                match i {
                    _ if i % 97 == 0 => record.kind = "failure".into(),
                    _ if i % 41 == 0 => record.kind = "prompt".into(),
                    _ => {
                        record.text = ["Read src/a.rs", "Bash cargo test", "Edit src/b.rs"]
                            [(i / 13) % 3]
                            .into()
                    }
                }
                record
            })
            .collect()
    }

    /// The quadratic walk the stack replaced: every floor from the ground,
    /// every timestamp parsed again. Kept as the reference result.
    fn walk_from_the_ground(
        tint: &Tint,
        records: &[&Event],
        scale: f64,
        span: (f64, f64),
    ) -> Option<f64> {
        let mut y = 0.0;
        let mut found: Option<f64> = None;
        for record in records {
            let top = y + Mark::of(record).weight() * scale;
            if y >= span.1 - 1e-9 {
                break;
            }
            if top > span.0 + 1e-9 {
                if let Some(n) = tint.reading(seconds(&record.at)).and_then(measure) {
                    found = Some(found.map_or(n, |f: f64| f.max(n)));
                }
            }
            y = top;
        }
        found
    }

    #[test]
    fn one_pass_per_tower_gives_the_same_floors_as_walking_from_the_ground() {
        use super::super::{bands, city, layout, tower_scale, State};
        let now = 1_763_400_000.0;
        let history = demo_history(now);
        for events in [tall_tower(2_000, now), crate::activity_data::demo_events()] {
            let now = events.iter().map(|e| seconds(&e.at)).fold(now, f64::max);
            let mut state = State::default();
            let shown = city(&events, &mut state);
            let lay = layout(&shown, 1.0);
            let scale = tower_scale(&shown, &lay);
            let tint = Tint::new(&history, now);
            let mut floors = 0;
            for session in &shown.sessions {
                let mut y = 0.0;
                for (height, _) in bands(session, scale) {
                    let span = (y, y + height);
                    assert_eq!(
                        tint.floor_measure(&session.records, scale, span),
                        walk_from_the_ground(&tint, &session.records, scale, span),
                        "floor {span:?}"
                    );
                    // Spans that start or end inside a record, too.
                    let inner = (y + height * 0.3, y + height * 0.7);
                    assert_eq!(
                        tint.floor_measure(&session.records, scale, inner),
                        walk_from_the_ground(&tint, &session.records, scale, inner)
                    );
                    y += height;
                    floors += 1;
                }
            }
            assert!(floors > 10, "{floors} floors is too few to test");
            assert_eq!(
                tint.stacks.borrow().len(),
                shown.sessions.len(),
                "one stack per tower"
            );
        }
    }

    #[test]
    #[ignore = "timing, run by hand with --ignored --nocapture"]
    fn time_heat_on_a_large_tower() {
        use super::super::{build_scene, city, layout, State};
        let now = 1_763_400_000.0;
        let events = tall_tower(2_000, now);
        let mut state = State::default();
        let shown = city(&events, &mut state);
        let lay = layout(&shown, 1.0);
        let history = demo_history(now);
        let frames = 200;
        let time = |heat: bool| {
            let started = std::time::Instant::now();
            for _ in 0..frames {
                let tint = Tint::new(&history, now);
                let tint = heat.then_some(&tint);
                std::hint::black_box(build_scene(&shown, &lay, 0, 0.0, now, tint));
            }
            started.elapsed().as_secs_f64() * 1000.0 / f64::from(frames)
        };
        let floors =
            super::super::bands(&shown.sessions[0], super::super::tower_scale(&shown, &lay)).len();
        eprintln!("{floors} floors");
        let (plain, heated) = (time(false), time(true));
        eprintln!("2,000-record tower: {plain:.3} ms/frame plain, {heated:.3} ms/frame with heat");
    }

    #[test]
    fn heat_recolours_floors_but_never_moves_a_tower() {
        use super::super::{build_scene, city, layout, tower_scale, State};
        let events = crate::activity_data::demo_events();
        let now = events.iter().map(|e| seconds(&e.at)).fold(0.0, f64::max) + 5.0;
        let mut state = State::default();
        let shown = city(&events, &mut state);
        let lay = layout(&shown, 1.0);
        let history = demo_history(now);
        let tint = Tint::new(&history, now);
        let (_, plain) = build_scene(&shown, &lay, 0, 0.0, now, None);
        let (_, heated) = build_scene(&shown, &lay, 0, 0.0, now, Some(&tint));
        assert_eq!(plain.vertices, heated.vertices, "heat moves nothing");
        assert_eq!(plain.faces.len(), heated.faces.len());
        let colours = |mesh: &crate::Mesh3D| {
            mesh.faces
                .iter()
                .map(|f| format!("{:?}", f.fill_char))
                .collect::<Vec<_>>()
        };
        assert_ne!(colours(&plain), colours(&heated), "heat changes colours");
        let scale = tower_scale(&shown, &lay);
        let gaps = shown
            .sessions
            .iter()
            .filter(|s| {
                let h: f64 = s.records.iter().map(|e| Mark::of(e).weight() * scale).sum();
                tint.floor_measure(&s.records, scale, (0.0, h)).is_none()
            })
            .count();
        assert!(
            gaps < shown.sessions.len(),
            "the demo history covers the demo day"
        );
    }

    #[test]
    fn heat_explains_itself_in_the_city_and_the_detail_view_at_every_size() {
        use super::super::{render, tests::plain, State};
        let events = crate::activity_data::demo_events();
        let now = events.iter().map(|e| seconds(&e.at)).fold(0.0, f64::max) + 5.0;
        for dims in [
            (200, 65),
            (165, 35),
            (120, 38),
            (100, 30),
            (80, 20),
            (79, 30),
            (60, 20),
            (40, 12),
            (12, 4),
        ] {
            let city_view = dims.0 >= 80 && dims.1 >= 20;
            for flat in [false, true] {
                let mut on = State {
                    flat,
                    heat: Heat::new(true),
                    ..State::default()
                };
                on.heat.on = true;
                assert!(on.heat.refresh(now));
                let mut off = State {
                    flat,
                    ..State::default()
                };
                let lit = render(&events, "fixture", &mut on, dims, true, now);
                assert_eq!(lit.len(), dims.1, "{dims:?}");
                let lit = plain(&lit);
                let dark = plain(&render(&events, "fixture", &mut off, dims, true, now));
                assert!(!dark.contains("machine-wide") && !dark.contains("whole machine"));
                if city_view && !flat {
                    assert!(
                        lit.contains("no reading") && lit.contains("busiest GPU"),
                        "{dims:?}\n{lit}"
                    );
                    assert!(
                        lit.contains("machine-wide energy over this span"),
                        "{dims:?}\n{lit}"
                    );
                    assert!(!dark.contains("no reading"));
                } else if dims.0 >= 120 && dims.1 >= 12 {
                    assert!(
                        lit.contains("whole machine over this session's span"),
                        "{dims:?}\n{lit}"
                    );
                }
            }
        }
        // Without a history file the key says how to start one.
        let mut state = State {
            heat: Heat {
                on: true,
                loaded: Some(Loaded::Absent),
                ..Heat::default()
            },
            ..State::default()
        };
        let text = plain(&render(
            &events,
            "fixture",
            &mut state,
            (165, 35),
            true,
            now,
        ));
        assert!(
            text.contains("e heat: no recorded history · start one with systemscape --record"),
            "{text}"
        );
        assert!(
            !text.contains("machine-wide"),
            "no energy without a history"
        );
    }

    #[test]
    fn heat_reads_nothing_until_switched_on_and_the_demo_never_reads_the_file() {
        let mut heat = Heat::new(true);
        assert!(!heat.refresh(1_763_337_600.0));
        assert!(heat.loaded.is_none());
        assert!(toggle(&mut heat, KeyCode::Char('e')));
        assert!(!toggle(&mut heat, KeyCode::Char('x')));
        assert!(heat.refresh(1_763_337_600.0));
        let Some(Loaded::Read(history)) = &heat.loaded else {
            panic!("the demo has a synthetic history");
        };
        assert!(heat.path.is_none());
        let minutes: Vec<f64> = history
            .rows
            .iter()
            .map(|r| (1_763_337_600.0 - r.t as f64) / 60.0)
            .collect();
        assert!(
            minutes.iter().all(|m| !(100.0..120.0).contains(m)),
            "the demo keeps a gap"
        );
        assert!(
            minutes.iter().any(|m| *m > 26.0 * 60.0),
            "and covers the earlier demo day"
        );
    }
}
