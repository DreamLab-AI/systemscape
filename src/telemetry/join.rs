//! Where telemetry meets recorded agent work: the `--record` and `--no-marks`
//! flags, the activity collector the view polls for marks, and the marks
//! drawn along the time axis.
//!
//! The collector is the activity view's own bounded reader. It runs on its
//! own thread, so a first poll that catches up on a large history never stalls
//! a frame, and it polls only while the view is on screen, every 30 seconds.
//! Marks describe this machine's records and are drawn only when this
//! machine's ridges are on screen.
use crate::activity::marks::{self, Kind, TimeMarks};
use crate::activity_data::{demo_events, Collector};
use crate::pixels::{PixelCanvas, Rgb};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Collector polls while the view is on screen. Marks sit in 150 s slots, so
/// anything quicker would only spend reads.
const MARK_POLL: Duration = Duration::from_secs(30);
const LEGEND_TEXT: Rgb = (126, 144, 164);

/// The flags this module adds to the telemetry command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Flags {
    /// `--record`: append each closed slot to the history file.
    pub(crate) record: bool,
    /// Off with `--no-marks`: never start the activity collector.
    pub(crate) marks: bool,
}

impl Default for Flags {
    fn default() -> Self {
        Self {
            record: false,
            marks: true,
        }
    }
}

/// Take `--record` and `--no-marks` out of a telemetry command line, leaving
/// the rest for the telemetry parser. Refuses `--record` with `--demo`, which
/// would put synthetic readings in the real history, and `--no-marks` with
/// `--probe`, which has no screen to mark.
pub(crate) fn split_flags(args: &[String]) -> Result<(Vec<String>, Flags), String> {
    let mut flags = Flags::default();
    let mut rest = Vec::with_capacity(args.len());
    for arg in args {
        match arg.as_str() {
            "--record" => flags.record = true,
            "--no-marks" => flags.marks = false,
            _ => rest.push(arg.clone()),
        }
    }
    let has = |flag: &str| rest.iter().skip(1).any(|a| a == flag);
    if flags.record && has("--demo") {
        return Err(
            "--record writes this machine's real readings and cannot be combined with --demo"
                .into(),
        );
    }
    if !flags.marks && has("--probe") {
        return Err("--probe has no screen to mark; --no-marks applies to the view".into());
    }
    Ok((rest, flags))
}

pub(crate) fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// The marks source for this machine's view.
pub(crate) struct MarkFeed {
    hits: Vec<marks::Hit>,
    fresh: Option<mpsc::Receiver<Vec<marks::Hit>>>,
    worker: Option<std::thread::Thread>,
    /// Toggled with `m`.
    pub(crate) show: bool,
}

impl MarkFeed {
    /// Start the feed: none with `--no-marks` or without a local machine on
    /// screen, the synthetic records with `--demo`, otherwise a collector
    /// thread that polls only while `on_screen` holds.
    pub(crate) fn start(
        flags: Flags,
        demo: bool,
        local: bool,
        on_screen: Arc<AtomicBool>,
    ) -> Option<Self> {
        if !flags.marks || !local {
            return None;
        }
        let window = super::HISTORY as f64 * super::SLOT_SECS + super::SLOT_SECS;
        if demo {
            return Some(Self {
                hits: marks::hits(&demo_events(), unix_now() - window),
                fresh: None,
                worker: None,
                show: true,
            });
        }
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut collector = activity_collector();
            loop {
                if on_screen.load(Ordering::Relaxed) {
                    collector.poll();
                    let found = marks::hits(&collector.rows(), unix_now() - window);
                    if tx.send(found).is_err() {
                        break;
                    }
                }
                // `unpark` when the view comes back on screen cuts the wait short.
                std::thread::park_timeout(MARK_POLL);
            }
        });
        Some(Self {
            hits: Vec::new(),
            fresh: Some(rx),
            worker: Some(worker.thread().clone()),
            show: true,
        })
    }

    /// A handle that wakes the collector thread, if there is one.
    pub(crate) fn waker(&self) -> Option<std::thread::Thread> {
        self.worker.clone()
    }

    /// Take the newest records from the collector. Returns whether they changed.
    pub(crate) fn drain(&mut self) -> bool {
        let Some(rx) = &self.fresh else {
            return false;
        };
        match rx.try_iter().last() {
            Some(hits) if hits != self.hits => {
                self.hits = hits;
                true
            }
            _ => false,
        }
    }

    /// The marks for a view whose live slot opened at unix second `live_from`.
    pub(crate) fn marks(&self, live_from: f64, now: f64) -> Option<TimeMarks> {
        self.show
            .then(|| marks::bucket(&self.hits, live_from, now, super::SLOT_SECS, super::HISTORY))
    }
}

/// The activity view's collector with the activity view's default sources.
fn activity_collector() -> Collector {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let workspace = std::path::PathBuf::from(
        std::env::var_os("WORKSPACE").unwrap_or_else(|| "/home/devuser/workspace".into()),
    );
    let archive = std::env::var_os("AGENTBOX_EVENT_ARCHIVE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| workspace.join(".agentbox/agent-events"));
    Collector::new(home, workspace, archive).with_env_roots()
}

/// The legend and the newest labels on one row, from `col`, in at most
/// `room` columns: `✕ error ◆ commit ↳ agent · m · -4m 3 errors · VisionFlow`.
pub(crate) fn legend(canvas: &mut PixelCanvas, marks: &TimeMarks, col: i64, row: i64, room: usize) {
    let mut text: Vec<(String, Rgb)> = Vec::new();
    for kind in Kind::ALL {
        text.push((format!("{} ", kind.glyph()), kind.hue()));
        text.push((format!("{} ", kind.name()), LEGEND_TEXT));
    }
    text.push(("· m".into(), LEGEND_TEXT));
    for slot in marks.slots.iter().take(3) {
        text.push((" · ".into(), LEGEND_TEXT));
        text.push((slot.top().glyph().to_string(), slot.top().hue()));
        text.push((format!(" {}", slot.label(marks.now)), LEGEND_TEXT));
    }
    if marks.more > 0 {
        text.push((format!(" · +{} more", marks.more), LEGEND_TEXT));
    }
    let mut used = 0usize;
    for (piece, colour) in text {
        let width = crate::sensors::columns(&piece);
        if used + width > room {
            break;
        }
        canvas.label(col + used as i64, row, &piece, colour);
        used += width;
    }
}

/// Glyphs along the 3D time axis. `cell` projects a slot position (0 is the
/// oldest slot, `HISTORY` the live one) on the ruler's line; the glyphs sit
/// on the row above the ruler, so its tick labels stay readable.
pub(crate) fn axis(
    canvas: &mut PixelCanvas,
    marks: &TimeMarks,
    scroll: f64,
    dims: (usize, usize),
    cell: impl Fn(f64) -> (i64, i64),
) {
    let history = super::HISTORY as f64;
    let ruler = cell(history).1.clamp(1, dims.1 as i64 - 2);
    let row = ruler - 1;
    if row < 1 {
        return;
    }
    for slot in &marks.slots {
        let position = history - slot.back as f64 - scroll;
        if position < -0.5 {
            continue; // slid off the floor with its ridge slot
        }
        let col = cell(position).0;
        if (0..dims.0 as i64).contains(&col) {
            canvas.label(col, row, &slot.top().glyph().to_string(), slot.top().hue());
        }
    }
}

/// The sparkline view's marks row at `row`, under the channels, with the
/// legend on the row after it when there is one to spare. A slot `back` from
/// the live one sits in column `spark + (slots - 1 - back) * stride`, the
/// column of that slot in every sparkline.
pub(crate) fn flat(
    canvas: &mut PixelCanvas,
    marks: &TimeMarks,
    row: i64,
    columns: (usize, usize, usize),
    dims: (usize, usize),
) {
    let (spark, slots, stride) = columns;
    let status = dims.1 as i64 - 1;
    if row >= status {
        return;
    }
    canvas.label(1, row, "activity", LEGEND_TEXT);
    for slot in marks.slots.iter().filter(|s| s.back < slots) {
        let col = spark + (slots - 1 - slot.back) * stride;
        canvas.label(
            col as i64,
            row,
            &slot.top().glyph().to_string(),
            slot.top().hue(),
        );
    }
    if row + 1 < status {
        legend(canvas, marks, 1, row + 1, dims.0.saturating_sub(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        std::iter::once("systemscape")
            .chain(line.split_whitespace())
            .map(String::from)
            .collect()
    }

    #[test]
    fn record_and_marks_flags_are_taken_out_and_senseless_pairs_refused() {
        let (rest, flags) = split_flags(&argv("--hosts h.toml --record")).unwrap();
        assert_eq!(rest, argv("--hosts h.toml"));
        assert_eq!(
            flags,
            Flags {
                record: true,
                marks: true
            }
        );
        let (rest, flags) = split_flags(&argv("--probe --record --interval 5")).unwrap();
        assert_eq!(rest, argv("--probe --interval 5"));
        assert!(flags.record);
        let (_, flags) = split_flags(&argv("--demo --no-marks")).unwrap();
        assert_eq!(
            flags,
            Flags {
                record: false,
                marks: false
            }
        );
        assert_eq!(split_flags(&argv("")).unwrap().1, Flags::default());
        for bad in ["--demo --record", "--record --demo", "--probe --no-marks"] {
            assert!(split_flags(&argv(bad)).is_err(), "{bad} should be refused");
        }
    }

    /// Text of rendered rows without colour escapes.
    fn plain(rows: &[String]) -> String {
        let mut text = String::new();
        for row in rows {
            let mut escape = false;
            for c in row.chars() {
                match (escape, c) {
                    (false, '\x1b') => escape = true,
                    (true, 'm') => escape = false,
                    (false, _) => text.push(c),
                    _ => {}
                }
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn activity_marks_sit_on_the_time_axis_in_both_views_at_any_size() {
        use super::super::{channels_for, demo_prefill, render_marked, Camera, Snapshot};
        use crate::activity::marks::{bucket, Hit};
        let mut channels = channels_for(3);
        demo_prefill(&mut channels);
        for channel in &mut channels {
            let last = *channel.history.back().unwrap();
            channel.poll(last);
        }
        let (live_from, now) = (1_000_000.0, 1_000_060.0);
        let hit = |back: f64, kind| Hit {
            at: live_from - back * 150.0 + 1.0,
            kind,
            project: "agentbox".into(),
        };
        let hits = [
            hit(0.0, Kind::Error),
            hit(20.0, Kind::Commit),
            hit(30.0, Kind::Agent),
            hit(47.0, Kind::Commit),
        ];
        let marks = bucket(&hits, live_from, now, 150.0, 48);
        let snapshot = Snapshot::default();
        let ridges = channels.len();
        for dims in [
            (200, 65),
            (165, 35),
            (120, 40),
            (80, 20),
            (79, 30),
            (47, 27),
            (30, 10),
            (12, 4),
            (512, 180),
        ] {
            for flat in [false, true] {
                let draw = |marks| {
                    render_marked(
                        &channels,
                        &snapshot,
                        &Camera::default(),
                        0.4,
                        dims,
                        flat,
                        None,
                        marks,
                    )
                };
                let rows = draw(Some(&marks));
                assert_eq!(rows.len(), dims.1, "{dims:?}");
                let text = plain(&rows);
                let without = plain(&draw(None));
                assert!(!without.contains('◆') && !without.contains('↳'));
                let three_d = !flat && dims.0 >= 80 && dims.1 >= 20 && ridges + 3 <= dims.1;
                if three_d {
                    assert!(text.contains('◆') && text.contains('↳'), "{dims:?}\n{text}");
                    for tick in ["-2h", "-90m", "-1h", "-30m", "now"] {
                        assert!(
                            text.contains(tick) || !without.contains(tick),
                            "{dims:?}: a mark hid {tick}"
                        );
                    }
                    assert!(text.contains("✕ error ◆ commit ↳ agent"), "{dims:?}");
                } else if ridges + 2 < dims.1 {
                    assert!(
                        text.contains("activity") && text.contains('◆'),
                        "{dims:?}\n{text}"
                    );
                }
                for secret in ["review", "cargo", "Bash"] {
                    assert!(!text.contains(secret));
                }
            }
        }
    }

    #[test]
    fn no_feed_without_a_local_machine_or_with_no_marks() {
        let sight = Arc::new(AtomicBool::new(true));
        let off = Flags {
            record: false,
            marks: false,
        };
        assert!(MarkFeed::start(off, true, true, sight.clone()).is_none());
        assert!(MarkFeed::start(Flags::default(), true, false, sight.clone()).is_none());
        let demo = MarkFeed::start(Flags::default(), true, true, sight).unwrap();
        let now = unix_now();
        let marks = demo.marks(now - 30.0, now).unwrap();
        assert!(
            !marks.slots.is_empty(),
            "the demo's records fall in the last two hours"
        );
    }
}
