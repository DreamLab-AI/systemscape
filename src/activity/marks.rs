//! Activity marks for the telemetry time axis: when commits, errors and
//! delegated-agent calls were recorded, bucketed into the ridges' own slots.
//!
//! A mark's label is built only from record kinds, counts and a cleaned,
//! shortened project name. The text of a prompt, a command or a tool call
//! never reaches the telemetry view.
//!
//! Everything here takes the records and the time as arguments; nothing reads
//! the clock.
use super::{district_name, seconds, Mark};
use crate::activity_data::{clean, Event};
use crate::pixels::Rgb;

/// Marked slots drawn at most; the rest are counted in the legend.
pub(crate) const MAX_MARKS: usize = 12;
/// Columns of a project name in a label.
const PROJECT_COLUMNS: usize = 16;

/// What a mark stands for, most urgent first: a slot with several kinds
/// shows the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Error,
    Commit,
    Agent,
}

impl Kind {
    pub(crate) const ALL: [Kind; 3] = [Kind::Error, Kind::Commit, Kind::Agent];

    /// The record kinds the city colours red, gold and pink.
    fn of(event: &Event) -> Option<Self> {
        match Mark::of(event) {
            Mark::Failure => Some(Self::Error),
            Mark::Receipt => Some(Self::Commit),
            Mark::Agent => Some(Self::Agent),
            _ => None,
        }
    }

    pub(crate) fn glyph(self) -> char {
        match self {
            Self::Error => '✕',
            Self::Commit => '◆',
            Self::Agent => '↳',
        }
    }

    /// The city's colour for the same records.
    pub(crate) fn hue(self) -> Rgb {
        match self {
            Self::Error => Mark::Failure.hue(),
            Self::Commit => Mark::Receipt.hue(),
            Self::Agent => Mark::Agent.hue(),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Commit => "commit",
            Self::Agent => "agent",
        }
    }

    fn count(self, n: usize) -> String {
        let shown = if n > 99 {
            "99+".to_owned()
        } else {
            n.to_string()
        };
        let noun = match (self, n == 1) {
            (Self::Error, true) => "error",
            (Self::Error, false) => "errors",
            (Self::Commit, true) => "commit",
            (Self::Commit, false) => "commits",
            (Self::Agent, true) => "agent call",
            (Self::Agent, false) => "agent calls",
        };
        format!("{shown} {noun}")
    }
}

/// One record worth marking: when, what, and which project.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Hit {
    pub(crate) at: f64,
    pub(crate) kind: Kind,
    /// The project's last path component, cleaned and cut short.
    pub(crate) project: String,
}

/// The records worth marking at or after unix second `since`.
pub(crate) fn hits(events: &[Event], since: f64) -> Vec<Hit> {
    events
        .iter()
        .filter_map(|event| {
            let kind = Kind::of(event)?;
            let at = seconds(&event.at);
            (at >= since).then(|| Hit {
                at,
                kind,
                project: clean(district_name(&event.project))
                    .chars()
                    .take(PROJECT_COLUMNS)
                    .collect(),
            })
        })
        .collect()
}

/// The marks in one ridge slot.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Slot {
    /// Slots back from the live one: 0 is the live slot.
    pub(crate) back: usize,
    /// Records of each kind, in `Kind::ALL` order.
    pub(crate) counts: [usize; 3],
    /// The project with the most marked records here.
    pub(crate) project: String,
    /// How many projects the records came from.
    pub(crate) projects: usize,
    /// Unix second of the newest marked record.
    pub(crate) newest: f64,
}

impl Slot {
    /// The most urgent kind present, which the slot's glyph shows.
    pub(crate) fn top(&self) -> Kind {
        Kind::ALL
            .into_iter()
            .zip(self.counts)
            .find(|(_, n)| *n > 0)
            .map_or(Kind::Agent, |(kind, _)| kind)
    }

    /// For example `-12m 3 errors, 1 commit · VisionFlow`.
    pub(crate) fn label(&self, now: f64) -> String {
        let minutes = ((now - self.newest).max(0.0) / 60.0).round() as u64;
        let ago = if minutes == 0 {
            "now".to_owned()
        } else {
            format!("-{minutes}m")
        };
        let counts: Vec<String> = Kind::ALL
            .into_iter()
            .zip(self.counts)
            .filter(|(_, n)| *n > 0)
            .map(|(kind, n)| kind.count(n))
            .collect();
        let whose = if self.projects > 1 {
            format!("{} projects", self.projects)
        } else {
            self.project.clone()
        };
        format!("{ago} {} · {whose}", counts.join(", "))
    }
}

/// The marks for one telemetry view: at most `MAX_MARKS` slots, newest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TimeMarks {
    pub(crate) slots: Vec<Slot>,
    /// Marked slots left out by the cap.
    pub(crate) more: usize,
    /// The newest record time considered: labels count back from here.
    pub(crate) now: f64,
}

/// Bucket `hits` into the ridges' slots. The live slot opened at unix second
/// `live_from` and runs to `now`; slot `back` covers the `slot_secs` before
/// the one after it, out to `slots` back. Records after `now` are ignored.
/// When more than `MAX_MARKS` slots hold records, the kinds take turns, by
/// each slot's glyph: the newest error slot, the newest commit slot, the
/// newest agent slot, then the next of each, so a busy run of errors cannot
/// hide every commit.
pub(crate) fn bucket(
    hits: &[Hit],
    live_from: f64,
    now: f64,
    slot_secs: f64,
    slots: usize,
) -> TimeMarks {
    let mut found: Vec<(Slot, Vec<(String, usize)>)> = Vec::new();
    for hit in hits.iter().filter(|h| h.at <= now) {
        let back = if hit.at >= live_from {
            0
        } else {
            ((live_from - hit.at) / slot_secs).ceil() as usize
        };
        if back > slots {
            continue;
        }
        let i = match found.iter().position(|(s, _)| s.back == back) {
            Some(i) => i,
            None => {
                found.push((
                    Slot {
                        back,
                        counts: [0; 3],
                        project: String::new(),
                        projects: 0,
                        newest: hit.at,
                    },
                    Vec::new(),
                ));
                found.len() - 1
            }
        };
        let (slot, projects) = &mut found[i];
        slot.counts[hit.kind as usize] += 1;
        slot.newest = slot.newest.max(hit.at);
        match projects.iter_mut().find(|(p, _)| *p == hit.project) {
            Some((_, n)) => *n += 1,
            None => projects.push((hit.project.clone(), 1)),
        }
    }
    let mut slots: Vec<Slot> = found
        .into_iter()
        .map(|(mut slot, mut projects)| {
            // Most records first; ties go to the name, so the label is stable.
            projects.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            slot.projects = projects.len();
            slot.project = projects.swap_remove(0).0;
            slot
        })
        .collect();
    // Rank each slot within its glyph's kind, newest first, then take rank 0
    // of every kind, rank 1 of every kind, and so on.
    slots.sort_by(|a, b| a.top().cmp(&b.top()).then(a.back.cmp(&b.back)));
    let mut rank = Vec::with_capacity(slots.len());
    for (i, slot) in slots.iter().enumerate() {
        let same = i > 0 && slots[i - 1].top() == slot.top();
        rank.push(if same { rank[i - 1] + 1 } else { 0usize });
    }
    let mut ranked: Vec<(usize, Slot)> = rank.into_iter().zip(slots).collect();
    ranked.sort_by(|(ra, a), (rb, b)| ra.cmp(rb).then(a.top().cmp(&b.top())));
    let more = ranked.len().saturating_sub(MAX_MARKS);
    ranked.truncate(MAX_MARKS);
    let mut slots: Vec<Slot> = ranked.into_iter().map(|(_, slot)| slot).collect();
    slots.sort_by_key(|s| s.back);
    TimeMarks { slots, more, now }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity_data::demo_events;

    fn event(at: f64, kind: &str, text: &str, project: &str) -> Event {
        Event {
            key: format!("{at}{text}"),
            at: chrono::DateTime::from_timestamp(at as i64, 0)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            agent: "claude".into(),
            session: "s".into(),
            project: project.into(),
            kind: kind.into(),
            text: text.into(),
            source: "fixture".into(),
            parent: None,
            path: None,
        }
    }

    #[test]
    fn only_errors_commits_and_agent_calls_are_marked_and_no_record_text_is_kept() {
        let now = 1_763_000_000.0;
        let events = [
            event(
                now - 10.0,
                "failure",
                "secret token in a command",
                "/w/VisionFlow",
            ),
            event(now - 20.0, "receipt", "Recorded commit abc", "/w/agentbox"),
            event(
                now - 30.0,
                "tool",
                "Agent review the diff",
                "/w/x\u{1b}[31mred",
            ),
            event(now - 40.0, "tool", "Bash rm -rf /", "/w/agentbox"),
            event(now - 50.0, "prompt", "do the thing", "/w/agentbox"),
            event(now - 9_000.0, "failure", "old", "/w/agentbox"),
        ];
        let found = hits(&events, now - 7_200.0);
        assert_eq!(
            found.iter().map(|h| h.kind).collect::<Vec<_>>(),
            [Kind::Error, Kind::Commit, Kind::Agent]
        );
        assert_eq!(found[2].project, "x[31mred", "the project name is cleaned");
        let marks = bucket(&found, now - 100.0, now, 150.0, 48);
        let label = marks.slots[0].label(now);
        assert_eq!(label, "now 1 error, 1 commit, 1 agent call · 3 projects");
        for secret in ["secret", "review", "rm -rf", "abc"] {
            assert!(!label.contains(secret), "{label}");
        }
    }

    #[test]
    fn records_land_in_the_same_slots_as_the_ridges() {
        let live_from = 10_000.0;
        let now = live_from + 40.0;
        let at = |t: f64| Hit {
            at: t,
            kind: Kind::Commit,
            project: "p".into(),
        };
        let marks = bucket(
            &[
                at(live_from + 5.0),                // the live slot
                at(live_from - 1.0),                // one back
                at(live_from - 150.0),              // still one back
                at(live_from - 150.5),              // two back
                at(live_from - 48.0 * 150.0),       // the oldest slot shown
                at(live_from - 48.0 * 150.0 - 1.0), // off the axis
                at(now + 60.0),                     // after `now`, as in a replay
            ],
            live_from,
            now,
            150.0,
            48,
        );
        assert_eq!(
            marks
                .slots
                .iter()
                .map(|s| (s.back, s.counts[1]))
                .collect::<Vec<_>>(),
            [(0, 1), (1, 2), (2, 1), (48, 1)]
        );
    }

    #[test]
    fn the_cap_gives_every_kind_a_turn_newest_first_and_counts_the_rest() {
        let now = 100_000.0;
        let mut found = Vec::new();
        for back in 0..20 {
            let kind = match back {
                19 => Kind::Error,
                18 => Kind::Commit,
                _ if back % 2 == 0 => Kind::Error,
                _ => Kind::Agent,
            };
            found.push(Hit {
                at: now - 150.0 * back as f64 - 1.0,
                kind,
                project: "p".into(),
            });
        }
        let marks = bucket(&found, now - 10.0, now, 150.0, 48);
        assert_eq!(marks.slots.len(), MAX_MARKS);
        assert_eq!(marks.more, 20 - MAX_MARKS);
        assert!(
            marks
                .slots
                .iter()
                .any(|s| s.back == 18 && s.top() == Kind::Commit),
            "the one commit survives many errors"
        );
        let errors = marks
            .slots
            .iter()
            .filter(|s| s.top() == Kind::Error)
            .count();
        let agents = marks
            .slots
            .iter()
            .filter(|s| s.top() == Kind::Agent)
            .count();
        assert!(
            errors.abs_diff(agents) <= 1,
            "{errors} errors, {agents} agents"
        );
        assert!(
            !marks.slots.iter().any(|s| s.back == 19),
            "the oldest error loses its turn"
        );
        assert_eq!(marks.slots[0].back, 0, "newest first");
        let backs: Vec<usize> = marks.slots.iter().map(|s| s.back).collect();
        assert!(backs.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn the_demo_has_every_kind_of_mark_inside_the_last_two_hours() {
        let events = demo_events();
        let now = events.iter().map(|e| seconds(&e.at)).fold(0.0, f64::max);
        let found = hits(&events, now - 7_200.0);
        for kind in Kind::ALL {
            assert!(found.iter().any(|h| h.kind == kind), "{kind:?} missing");
        }
    }
}
