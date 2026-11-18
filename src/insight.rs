//! Questions asked of recorded activity: what a day held, which sessions look
//! stuck, who is editing a file and which files more than one session edited.
//!
//! Everything here is a pure function of a slice of events (and, where age
//! matters, a `now`), so the one-shot outputs, the MCP server, the metrics
//! endpoint and the city view all give the same answer for the same records.
//! Like the records themselves, every answer describes what was logged, not
//! what an agent is doing or whether its work succeeded.
//!
//! Days are UTC calendar days, the first ten characters of a record's
//! timestamp, exactly as the city groups them.
use crate::activity_data::Event;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// How many failing repeats of one command, running, make a session stuck.
pub(crate) const STUCK_RUNS: usize = 3;
/// A session whose newest record is this recent counts as active (as in the view).
pub(crate) const LIVE_SECS: i64 = 300;
/// A failure or a stuck run this recent is still news (the view's `!` window).
pub(crate) const RECENT_SECS: i64 = 3 * LIVE_SECS;

/// Unix milliseconds of a record timestamp; collector timestamps are RFC 3339.
pub(crate) fn millis(at: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// A bound in the collector's timestamp format, so it compares as text with `at`.
pub(crate) fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Every day that has records, newest first.
pub(crate) fn days(events: &[Event]) -> Vec<&str> {
    let days: BTreeSet<&str> = events.iter().filter_map(|e| e.at.get(..10)).collect();
    days.into_iter().rev().collect()
}

/// Records grouped by `(agent, session)`, each group oldest first, groups in
/// key order. This is the session identity the view uses.
fn sessions<'a>(
    events: impl Iterator<Item = &'a Event>,
) -> BTreeMap<(&'a str, &'a str), Vec<&'a Event>> {
    let mut groups: BTreeMap<(&str, &str), Vec<&Event>> = BTreeMap::new();
    for event in events {
        groups
            .entry((event.agent.as_str(), event.session.as_str()))
            .or_default()
            .push(event);
    }
    for records in groups.values_mut() {
        // Timestamps are normalised to UTC milliseconds, so they sort as text.
        records.sort_by(|a, b| a.at.cmp(&b.at).then(a.key.cmp(&b.key)));
    }
    groups
}

/// A run of one command failing again and again at the end of a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Run {
    /// The repeated tool call, as recorded (cleaned, at most 240 characters).
    pub(crate) command: String,
    /// How many times running it was called and followed by a failure.
    pub(crate) runs: usize,
}

/// Whether a session's most recent tool calls repeat one command with a
/// failure after each, at least [`STUCK_RUNS`] times running.
///
/// `records` is one session, oldest first. This is a heuristic on logged
/// records, and failures are paired with calls the way the collector keys them:
///
/// - A Claude tool result that reports an error is recorded under the key of
///   the call it answers plus `:error`, so it pairs with exactly that call even
///   when several calls ran in parallel.
/// - Any other failure (an archive step that failed, a Codex aborted turn, the
///   demo fixture) carries no call key, so it pairs with the tool call it
///   immediately follows.
///
/// Codex tool output is not recorded at all, so a Codex session is only seen
/// failing through an aborted turn. The scan runs backwards and stops at the
/// first call that did not fail, so it is cheap enough to ask every frame.
pub(crate) fn stuck_run(records: &[&Event]) -> Option<Run> {
    // Call keys answered by a keyed failure later in the session than the scan point.
    let mut answered: Vec<&str> = Vec::new();
    let mut next: Option<&Event> = None;
    let mut command: Option<&str> = None;
    let mut runs = 0;
    for event in records.iter().rev() {
        match event.kind.as_str() {
            "failure" => {
                if let Some(call) = event.key.strip_suffix(":error") {
                    answered.push(call);
                }
            }
            "tool" => {
                let failed = answered.contains(&event.key.as_str())
                    || next.is_some_and(|n| n.kind == "failure" && !n.key.ends_with(":error"));
                if !failed || command.is_some_and(|c| c != event.text) {
                    break;
                }
                command = Some(&event.text);
                runs += 1;
            }
            _ => {}
        }
        next = Some(event);
    }
    (runs >= STUCK_RUNS).then(|| Run {
        command: command.unwrap_or_default().to_owned(),
        runs,
    })
}

/// Is this tool call an edit or a write of a file? Matches the view's edit
/// class (`edit`, `write` or `patch` in the tool name) and needs a recorded
/// path, which keeps `TodoWrite` and the like out.
fn is_edit(event: &Event) -> bool {
    let name = event
        .text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    event.kind == "tool"
        && event.path.is_some()
        && ["edit", "write", "patch"].iter().any(|w| name.contains(w))
}

/// The file an edit touched, as an absolute path where one can be formed:
/// Codex patches name files relative to the session's working directory,
/// Claude names them whole, and the two must compare equal.
pub(crate) fn whole_path(event: &Event) -> Option<String> {
    let path = event.path.as_deref()?;
    let path = path.strip_prefix("./").unwrap_or(path);
    if path.starts_with('/') || !event.project.starts_with('/') {
        Some(path.to_owned())
    } else {
        Some(format!("{}/{path}", event.project.trim_end_matches('/')))
    }
}

/// Does `path` name `query`? An absolute query must match exactly; a relative
/// one matches whole trailing components, so `src/a.rs` matches
/// `/x/src/a.rs` but not `/x/mysrc/a.rs`.
pub(crate) fn names(path: &str, query: &str) -> bool {
    let query = query.strip_prefix("./").unwrap_or(query);
    if query.is_empty() {
        return false;
    }
    if query.starts_with('/') {
        return path == query;
    }
    path == query
        || path
            .strip_suffix(query)
            .is_some_and(|head| head.ends_with('/'))
}

/// Record counts by kind, with edits and commits picked out of tools and receipts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Counts {
    pub(crate) records: usize,
    pub(crate) prompts: usize,
    pub(crate) tools: usize,
    pub(crate) edits: usize,
    pub(crate) failures: usize,
    pub(crate) commits: usize,
    pub(crate) events: usize,
}

impl Counts {
    fn add(&mut self, event: &Event) {
        self.records += 1;
        match event.kind.as_str() {
            "prompt" => self.prompts += 1,
            "tool" => {
                self.tools += 1;
                self.edits += usize::from(is_edit(event));
            }
            "failure" => self.failures += 1,
            "receipt" => self.commits += usize::from(commit_sha(event).is_some()),
            _ => self.events += 1,
        }
    }

    fn merge(&mut self, other: &Counts) {
        self.records += other.records;
        self.prompts += other.prompts;
        self.tools += other.tools;
        self.edits += other.edits;
        self.failures += other.failures;
        self.commits += other.commits;
        self.events += other.events;
    }
}

/// The commit a receipt records: `Recorded commit <sha>` gives `<sha>`.
pub(crate) fn commit_sha(event: &Event) -> Option<&str> {
    (event.kind == "receipt")
        .then(|| event.text.strip_prefix("Recorded commit "))
        .flatten()
        .and_then(|rest| rest.split_whitespace().next())
}

/// One session's share of a day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SessionDay {
    pub(crate) agent: String,
    pub(crate) session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent: Option<String>,
    pub(crate) project: String,
    pub(crate) counts: Counts,
    pub(crate) first: String,
    pub(crate) last: String,
    /// Commit SHAs as recorded, oldest first.
    pub(crate) commits: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stuck: Option<Run>,
}

/// One project's share of a day: its sessions, most recent first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ProjectDay {
    pub(crate) project: String,
    pub(crate) counts: Counts,
    pub(crate) first: String,
    pub(crate) last: String,
    pub(crate) sessions: Vec<SessionDay>,
}

/// A file edited by more than one session on a day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Contended {
    pub(crate) path: String,
    /// `agent session` of each editing session, in key order.
    pub(crate) sessions: Vec<String>,
    pub(crate) edits: usize,
}

/// A day of recorded work, for the summary and the MCP digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Digest {
    pub(crate) day: String,
    pub(crate) counts: Counts,
    /// Most recent project first; ties by name.
    pub(crate) projects: Vec<ProjectDay>,
    /// Stuck sessions of the day, most recent first.
    pub(crate) stuck: Vec<SessionDay>,
    pub(crate) contended: Vec<Contended>,
}

/// The digest of one UTC day (`YYYY-MM-DD`).
pub(crate) fn digest(events: &[Event], day: &str) -> Digest {
    let mut by_project: BTreeMap<&str, Vec<SessionDay>> = BTreeMap::new();
    for ((agent, session), records) in sessions(events.iter().filter(|e| e.at.starts_with(day))) {
        let mut counts = Counts::default();
        records.iter().for_each(|e| counts.add(e));
        let newest = records[records.len() - 1];
        by_project
            .entry(newest.project.as_str())
            .or_default()
            .push(SessionDay {
                agent: agent.to_owned(),
                session: session.to_owned(),
                parent: newest.parent.clone(),
                project: newest.project.clone(),
                counts,
                first: records[0].at.clone(),
                last: newest.at.clone(),
                commits: records
                    .iter()
                    .filter_map(|e| commit_sha(e))
                    .map(str::to_owned)
                    .collect(),
                stuck: stuck_run(&records),
            });
    }
    let mut counts = Counts::default();
    let mut stuck = Vec::new();
    let mut projects: Vec<ProjectDay> = by_project
        .into_iter()
        .map(|(project, mut sessions)| {
            sessions.sort_by(|a, b| {
                b.last
                    .cmp(&a.last)
                    .then_with(|| (&a.agent, &a.session).cmp(&(&b.agent, &b.session)))
            });
            let mut total = Counts::default();
            sessions.iter().for_each(|s| total.merge(&s.counts));
            counts.merge(&total);
            stuck.extend(sessions.iter().filter(|s| s.stuck.is_some()).cloned());
            ProjectDay {
                project: project.to_owned(),
                counts: total,
                first: sessions
                    .iter()
                    .map(|s| &s.first)
                    .min()
                    .cloned()
                    .unwrap_or_default(),
                last: sessions
                    .iter()
                    .map(|s| &s.last)
                    .max()
                    .cloned()
                    .unwrap_or_default(),
                sessions,
            }
        })
        .collect();
    projects.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.project.cmp(&b.project)));
    stuck.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.session.cmp(&b.session)));
    Digest {
        day: day.to_owned(),
        counts,
        projects,
        stuck,
        contended: contended(events, day),
    }
}

/// Files edited by more than one session on a UTC day, most sessions first,
/// then by path.
pub(crate) fn contended(events: &[Event], day: &str) -> Vec<Contended> {
    let mut by_path: BTreeMap<String, (BTreeSet<String>, usize)> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|e| e.at.starts_with(day) && is_edit(e))
    {
        let Some(path) = whole_path(event) else {
            continue;
        };
        let entry = by_path.entry(path).or_default();
        entry.0.insert(format!("{} {}", event.agent, event.session));
        entry.1 += 1;
    }
    let mut found: Vec<Contended> = by_path
        .into_iter()
        .filter(|(_, (sessions, _))| sessions.len() > 1)
        .map(|(path, (sessions, edits))| Contended {
            path,
            sessions: sessions.into_iter().collect(),
            edits,
        })
        .collect();
    found.sort_by(|a, b| {
        b.sessions
            .len()
            .cmp(&a.sessions.len())
            .then_with(|| a.path.cmp(&b.path))
    });
    found
}

/// A session that edited a given file inside a window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Editor {
    pub(crate) agent: String,
    pub(crate) session: String,
    pub(crate) project: String,
    /// The matching path as recorded (made whole where the record allows).
    pub(crate) path: String,
    pub(crate) edits: usize,
    pub(crate) first: String,
    pub(crate) last: String,
}

/// Sessions with edit or write records for `query` (see [`names`]) whose
/// timestamps fall in `since..=until`, most recent first.
pub(crate) fn editors(
    events: &[Event],
    query: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Vec<Editor> {
    let (since, until) = (stamp(since), stamp(until));
    let mut by_session: BTreeMap<(&str, &str, String), Editor> = BTreeMap::new();
    for event in events {
        if event.at < since || event.at > until || !is_edit(event) {
            continue;
        }
        let Some(path) = whole_path(event).filter(|p| names(p, query)) else {
            continue;
        };
        let editor = by_session
            .entry((&event.agent, &event.session, path.clone()))
            .or_insert_with(|| Editor {
                agent: event.agent.clone(),
                session: event.session.clone(),
                project: event.project.clone(),
                path,
                edits: 0,
                first: event.at.clone(),
                last: event.at.clone(),
            });
        editor.edits += 1;
        if event.at < editor.first {
            editor.first = event.at.clone();
        }
        if event.at > editor.last {
            editor.last = event.at.clone();
            editor.project = event.project.clone();
        }
    }
    let mut found: Vec<Editor> = by_session.into_values().collect();
    found.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.session.cmp(&b.session)));
    found
}

/// What a session's newest record says about it now. The order is the
/// precedence: a stuck session is not also counted as erroring or active.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    /// A stuck run whose last record is under [`RECENT_SECS`] old.
    Stuck,
    /// The newest record is a failure under [`RECENT_SECS`] old.
    Error,
    /// The newest record is under [`LIVE_SECS`] old.
    Active,
    Idle,
}

/// One session as of `now`, for recent-session listings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Recent {
    pub(crate) agent: String,
    pub(crate) session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent: Option<String>,
    pub(crate) project: String,
    pub(crate) state: State,
    pub(crate) records: usize,
    pub(crate) last: String,
    /// The newest record's kind and text.
    pub(crate) latest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stuck: Option<Run>,
}

/// Every session with a record at or after `since`, most recent first,
/// classified as of `now`.
pub(crate) fn recent(events: &[Event], now: DateTime<Utc>, since: DateTime<Utc>) -> Vec<Recent> {
    let since = stamp(since);
    let now = now.timestamp_millis();
    let mut found: Vec<Recent> = sessions(events.iter())
        .into_iter()
        .filter(|(_, records)| records[records.len() - 1].at >= since)
        .map(|((agent, session), records)| {
            let newest = records[records.len() - 1];
            let age = millis(&newest.at).map_or(i64::MAX, |at| (now - at) / 1000);
            let stuck = stuck_run(&records);
            let state = if stuck.is_some() && age < RECENT_SECS {
                State::Stuck
            } else if newest.kind == "failure" && age < RECENT_SECS {
                State::Error
            } else if age < LIVE_SECS {
                State::Active
            } else {
                State::Idle
            };
            Recent {
                agent: agent.to_owned(),
                session: session.to_owned(),
                parent: newest.parent.clone(),
                project: newest.project.clone(),
                state,
                records: records.len(),
                last: newest.at.clone(),
                latest: format!("{} {}", newest.kind, newest.text),
                stuck,
            }
        })
        .collect();
    found.sort_by(|a, b| {
        b.last
            .cmp(&a.last)
            .then_with(|| (&a.agent, &a.session).cmp(&(&b.agent, &b.session)))
    });
    found
}

/// Session counts by state, for a status line or a metrics scrape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pulse {
    pub(crate) active: usize,
    pub(crate) errors: usize,
    pub(crate) stuck: usize,
}

/// How many sessions are active, erroring or stuck as of `now`. Each session
/// is counted once, in the first state of [`State`] that applies.
pub(crate) fn pulse(events: &[Event], now: DateTime<Utc>) -> Pulse {
    let since = now - chrono::Duration::seconds(RECENT_SECS);
    let mut pulse = Pulse::default();
    for session in recent(events, now, since) {
        match session.state {
            State::Stuck => pulse.stuck += 1,
            State::Error => pulse.errors += 1,
            State::Active => pulse.active += 1,
            State::Idle => {}
        }
    }
    pulse
}

/// Record counts by kind for one day, keyed by kind name in a stable order.
pub(crate) fn kinds(events: &[Event], day: &str) -> BTreeMap<String, usize> {
    let mut kinds: HashMap<&str, usize> = HashMap::new();
    for event in events.iter().filter(|e| e.at.starts_with(day)) {
        *kinds.entry(event.kind.as_str()).or_default() += 1;
    }
    kinds.into_iter().map(|(k, n)| (k.to_owned(), n)).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A record at `at` (seconds into 2026-11-18 UTC) with a key unique to it.
    pub(crate) fn record(session: &str, secs: i64, kind: &str, text: &str) -> Event {
        let at = DateTime::from_timestamp(1_794_960_000 + secs, 0).unwrap();
        Event {
            key: format!("claude::{session}:{secs}:{kind}"),
            at: stamp(at),
            agent: "claude".into(),
            session: session.into(),
            project: "/work/app".into(),
            kind: kind.into(),
            text: text.into(),
            source: "test".into(),
            parent: None,
            path: None,
        }
    }

    fn edit(session: &str, secs: i64, path: &str) -> Event {
        Event {
            path: Some(path.into()),
            ..record(session, secs, "tool", &format!("Edit {path}"))
        }
    }

    fn noon() -> DateTime<Utc> {
        DateTime::from_timestamp(1_794_960_000 + 12 * 3600, 0).unwrap()
    }

    #[test]
    fn the_fixture_clock_starts_at_midnight_on_the_eighteenth() {
        assert_eq!(record("s", 0, "tool", "x").at, "2026-11-18T00:00:00.000Z");
    }

    fn refs(events: &[Event]) -> Vec<&Event> {
        events.iter().collect()
    }

    #[test]
    fn three_adjacent_failures_of_one_command_make_a_session_stuck() {
        let mut events = vec![record("s", 0, "prompt", "fix it")];
        for i in 0..3 {
            events.push(record("s", 10 + i * 10, "tool", "Bash cargo test"));
            events.push(record(
                "s",
                15 + i * 10,
                "failure",
                "Tool reported an error",
            ));
        }
        assert_eq!(
            stuck_run(&refs(&events)),
            Some(Run {
                command: "Bash cargo test".into(),
                runs: 3
            })
        );
        events.truncate(5);
        assert_eq!(stuck_run(&refs(&events)), None, "two runs are not enough");
    }

    #[test]
    fn a_success_or_a_different_command_ends_the_run() {
        let mut events = vec![];
        for i in 0..3 {
            events.push(record("s", i * 10, "tool", "Bash cargo test"));
            events.push(record("s", i * 10 + 5, "failure", "x"));
        }
        let mut passed = events.clone();
        passed.push(record("s", 100, "tool", "Bash cargo test"));
        assert_eq!(
            stuck_run(&refs(&passed)),
            None,
            "the newest call did not fail"
        );
        let mut other = events.clone();
        other.insert(2, record("s", 6, "tool", "Bash cargo build"));
        other.insert(3, record("s", 7, "failure", "x"));
        assert_eq!(
            stuck_run(&refs(&other)),
            None,
            "only two repeats run to the end"
        );
    }

    #[test]
    fn a_keyed_tool_error_pairs_with_its_own_call_even_among_parallel_calls() {
        // Claude keys a failing result as `<call key>:error`. Two calls go out
        // together each turn; only the test run fails, and its error arrives
        // after the other call, so adjacency alone would pair it wrongly.
        let mut events = vec![];
        for turn in 0..3 {
            let t = turn * 100;
            let mut call = record("s", t, "tool", "Bash cargo test");
            call.key = format!("claude::s:call-{turn}");
            let mut read = record("s", t + 1, "tool", "Read src/lib.rs");
            read.key = format!("claude::s:read-{turn}");
            let mut error = record("s", t + 5, "failure", "Tool reported an error");
            error.key = format!("claude::s:call-{turn}:error");
            events.extend([call, read, error]);
        }
        assert_eq!(
            stuck_run(&refs(&events)),
            None,
            "the read is the most recent call, and it succeeded"
        );
        // Without the reads, the keyed errors alone make the run.
        events.retain(|e| !e.text.starts_with("Read"));
        assert_eq!(stuck_run(&refs(&events)).unwrap().runs, 3);
        // A keyed error never pairs by adjacency with a different call.
        for e in events.iter_mut().filter(|e| e.kind == "failure") {
            e.key = format!("{}-elsewhere:error", e.key);
        }
        assert_eq!(stuck_run(&refs(&events)), None);
    }

    #[test]
    fn prompts_between_repeats_do_not_break_a_run_and_the_demo_has_one_stuck_session() {
        let mut events = vec![];
        for i in 0..4 {
            events.push(record("s", i * 10, "tool", "Bash make"));
            events.push(record("s", i * 10 + 1, "failure", "x"));
            events.push(record("s", i * 10 + 2, "prompt", "try again"));
        }
        assert_eq!(stuck_run(&refs(&events)).unwrap().runs, 4);

        let demo = crate::activity_data::demo_events();
        let day = days(&demo)[0].to_owned();
        let stuck: Vec<_> = digest(&demo, &day)
            .stuck
            .into_iter()
            .map(|s| s.session)
            .collect();
        assert_eq!(stuck, ["77e385bc-demo"]);
    }

    #[test]
    fn relative_queries_match_whole_trailing_components() {
        assert!(names("/x/src/a.rs", "src/a.rs"));
        assert!(names("/x/src/a.rs", "./a.rs"));
        assert!(names("/x/src/a.rs", "/x/src/a.rs"));
        assert!(!names("/x/mysrc/a.rs", "src/a.rs"));
        assert!(!names("/x/src/a.rs", "/src/a.rs"), "absolute means exact");
        assert!(!names("/x/src/a.rs", ""));
    }

    #[test]
    fn a_relative_patch_path_and_an_absolute_edit_are_the_same_file() {
        let mut patch = edit("codex-1", 10, "src/main.rs");
        patch.agent = "codex".into();
        patch.text = "apply_patch src/main.rs".into();
        let claude = edit("claude-1", 20, "/work/app/src/main.rs");
        let alone = edit("claude-1", 30, "/work/app/README.md");
        let mut todo = record("claude-2", 40, "tool", "TodoWrite plan");
        todo.path = None;
        let events = vec![patch, claude, alone, todo];
        let found = contended(&events, "2026-11-18");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "/work/app/src/main.rs");
        assert_eq!(found[0].sessions, ["claude claude-1", "codex codex-1"]);
        assert!(contended(&events, "2026-11-17").is_empty(), "another day");
    }

    #[test]
    fn editors_are_found_inside_the_window_most_recent_first() {
        let events = vec![
            edit("early", 60, "/work/app/src/lib.rs"),
            edit("late", 3600, "/work/app/src/lib.rs"),
            edit("late", 3700, "/work/app/src/lib.rs"),
            edit("other", 3800, "/work/app/src/main.rs"),
            Event {
                path: Some("/work/app/src/lib.rs".into()),
                ..record("reader", 3900, "tool", "Read /work/app/src/lib.rs")
            },
        ];
        let start = DateTime::from_timestamp(1_794_960_000, 0).unwrap();
        let all = editors(&events, "src/lib.rs", start, noon());
        let sessions: Vec<_> = all.iter().map(|e| (e.session.as_str(), e.edits)).collect();
        assert_eq!(sessions, [("late", 2), ("early", 1)], "reads are not edits");
        let window = editors(
            &events,
            "lib.rs",
            start + chrono::Duration::minutes(30),
            noon(),
        );
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].first, "2026-11-18T01:00:00.000Z");
    }

    #[test]
    fn the_digest_groups_projects_and_sessions_in_a_deterministic_order() {
        let mut other = record("b", 50, "receipt", "Recorded commit 0123abcd");
        other.project = "/work/zzz".into();
        let mut events = vec![
            record("a", 10, "prompt", "go"),
            edit("a", 20, "/work/app/x.rs"),
            record("a", 30, "failure", "x"),
            other,
            record("c", 5, "tool", "Bash ls"),
            record("a", -60, "prompt", "yesterday"),
        ];
        let forward = digest(&events, "2026-11-18");
        events.reverse();
        assert_eq!(
            digest(&events, "2026-11-18"),
            forward,
            "input order does not matter"
        );
        let projects: Vec<_> = forward
            .projects
            .iter()
            .map(|p| p.project.as_str())
            .collect();
        assert_eq!(projects, ["/work/zzz", "/work/app"]);
        let app = &forward.projects[1];
        assert_eq!(app.sessions[0].session, "a");
        assert_eq!(app.sessions[0].counts.edits, 1);
        assert_eq!(app.sessions[0].counts.failures, 1);
        assert_eq!(app.first, "2026-11-18T00:00:05.000Z");
        assert_eq!(
            forward.counts.records, 5,
            "yesterday's prompt is not counted"
        );
        assert_eq!(forward.counts.commits, 1);
        assert_eq!(forward.projects[0].sessions[0].commits, ["0123abcd"]);
        assert_eq!(days(&events), ["2026-11-18", "2026-11-17"]);
    }

    #[test]
    fn each_session_is_counted_once_with_stuck_before_error_before_active() {
        let now = noon();
        let base = 12 * 3600;
        let mut events = vec![
            record("live", base - 60, "tool", "Bash ls"),
            record("broken", base - 120, "failure", "x"),
            record("old-error", base - 3600, "failure", "x"),
            record("idle", base - 1200, "prompt", "hello"),
        ];
        for i in 0..3 {
            events.push(record("stuck", base - 300 + i * 20, "tool", "Bash make"));
            events.push(record("stuck", base - 290 + i * 20, "failure", "x"));
        }
        assert_eq!(
            pulse(&events, now),
            Pulse {
                active: 1,
                errors: 1,
                stuck: 1
            }
        );
        let since = now - chrono::Duration::hours(2);
        let listed: Vec<_> = recent(&events, now, since)
            .into_iter()
            .map(|r| (r.session, r.state))
            .collect();
        assert_eq!(
            listed,
            [
                ("live".to_owned(), State::Active),
                ("broken".to_owned(), State::Error),
                ("stuck".to_owned(), State::Stuck),
                ("idle".to_owned(), State::Idle),
                ("old-error".to_owned(), State::Idle),
            ]
        );
        assert_eq!(kinds(&events, "2026-11-18")["failure"], 5);
    }
}
