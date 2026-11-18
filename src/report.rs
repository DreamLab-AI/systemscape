//! Non-interactive outputs built on `insight`: `--summary` (a day's digest as
//! text or JSON) and `--status` (one short line for a status bar), plus what
//! they share with the MCP server and the metrics endpoint: the history
//! source flags, one-poll sensor readings and opt-in commit verification.
use crate::activity_data::{clean, demo_events, Collector, Event};
use crate::insight::{self, Digest};
use crate::telemetry::{poll_sensors, Gpu, RateState, Snapshot};
use serde::Serialize;
use serde_json::Value;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Longest string an output keeps (paths are kept whole up to this).
pub(crate) const STRING_LIMIT: usize = 1024;

/// Remove what `activity_data::clean` removes (controls, directional
/// formatting, invisible carriers) without folding whitespace, and cap the length.
pub(crate) fn scrub(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c,
        '\u{00ad}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' |
        '\u{2060}'..='\u{206f}' | '\u{feff}' | '\u{e0000}'..='\u{e007f}')
        })
        .take(STRING_LIMIT)
        .collect()
}

/// Scrub every string in a result, keys included.
pub(crate) fn scrub_value(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(scrub(&s)),
        Value::Array(items) => Value::Array(items.into_iter().map(scrub_value).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (scrub(&k), scrub_value(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Coverage line for synthetic records, as the activity view prints it.
const DEMO_COVERAGE: &str = "Synthetic fixture · no agent files opened";

pub(crate) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// The history-source flags every activity output accepts, with the same
/// defaults as `--activity`: `$HOME`, `$WORKSPACE` (or the agentbox
/// workspace), `$AGENTBOX_EVENT_ARCHIVE_DIR` (or the workspace's event
/// archive), and the harness roots unless `--home` is given.
pub(crate) struct SourceFlags {
    pub(crate) demo: bool,
    home: Option<PathBuf>,
    workspace: Option<PathBuf>,
    archive: Option<PathBuf>,
}

impl SourceFlags {
    pub(crate) fn new() -> Self {
        Self {
            demo: false,
            home: None,
            workspace: None,
            archive: None,
        }
    }

    /// Take `arg` (and its value) if it is a source flag. Returns whether it was.
    pub(crate) fn take<'a>(
        &mut self,
        arg: &str,
        rest: &mut impl Iterator<Item = &'a String>,
    ) -> io::Result<bool> {
        let slot = match arg {
            "--demo" => {
                self.demo = true;
                return Ok(true);
            }
            "--home" => &mut self.home,
            "--workspace" => &mut self.workspace,
            "--archive" => &mut self.archive,
            _ => return Ok(false),
        };
        let value = rest
            .next()
            .ok_or_else(|| invalid(format!("{arg} requires a path")))?;
        *slot = Some(PathBuf::from(value));
        Ok(true)
    }

    pub(crate) fn open(self) -> Sources {
        if self.demo {
            return Sources { collector: None };
        }
        let explicit_home = self.home.is_some();
        let home = self
            .home
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()));
        let workspace = self.workspace.unwrap_or_else(|| {
            PathBuf::from(
                std::env::var_os("WORKSPACE").unwrap_or_else(|| "/home/devuser/workspace".into()),
            )
        });
        let archive = self
            .archive
            .or_else(|| std::env::var_os("AGENTBOX_EVENT_ARCHIVE_DIR").map(PathBuf::from))
            .unwrap_or_else(|| workspace.join(".agentbox/agent-events"));
        let mut collector = Collector::new(home, workspace, archive);
        if !explicit_home {
            collector = collector.with_env_roots();
        }
        Sources {
            collector: Some(collector),
        }
    }
}

/// Where records come from: the demo fixture, or a collector kept across
/// polls so a long-running server reads only what was appended.
pub(crate) struct Sources {
    collector: Option<Collector>,
}

impl Sources {
    pub(crate) fn demo(&self) -> bool {
        self.collector.is_none()
    }

    /// Poll until no tracked file has unread bytes, for at most `budget`:
    /// a report of a whole day should not stop at the first poll's byte
    /// limit. Each poll is still bounded, and a round that reads nothing
    /// new ends the wait, so files that can never drain do not hold it open.
    pub(crate) fn load_drained(&mut self, budget: Duration) -> (Vec<Event>, String) {
        let started = std::time::Instant::now();
        let mut loaded = self.load();
        let Some(collector) = &mut self.collector else {
            return loaded;
        };
        let mut pending = collector.pending();
        while pending > 0 && started.elapsed() < budget {
            collector.poll();
            let left = collector.pending();
            if left >= pending {
                break;
            }
            pending = left;
        }
        loaded = (collector.rows(), collector.coverage());
        loaded
    }

    /// One bounded poll; the records (newest first) and the coverage line.
    pub(crate) fn load(&mut self) -> (Vec<Event>, String) {
        match &mut self.collector {
            None => (demo_events(), DEMO_COVERAGE.to_owned()),
            Some(collector) => {
                collector.poll();
                (collector.rows(), collector.coverage())
            }
        }
    }
}

/// nvidia-smi is never asked more often than this, however often readings are.
const GPU_EVERY: Duration = Duration::from_secs(10);

/// Sensor state kept between readings: CPU busy and transfer rates need two
/// samples, and GPU cards are cached for [`GPU_EVERY`].
#[derive(Default)]
pub(crate) struct Sensors {
    prev_cpu: (u64, u64),
    rates: RateState,
    cards: Vec<Gpu>,
    gpu_at: Option<Instant>,
}

impl Sensors {
    /// One sensor poll. `gpu` allows an nvidia-smi query (bounded at 1.5 s
    /// by telemetry) when the cached cards are older than [`GPU_EVERY`].
    pub(crate) fn read(&mut self, gpu: bool) -> Reading {
        let due = gpu && self.gpu_at.is_none_or(|at| at.elapsed() >= GPU_EVERY);
        let mut snapshot = poll_sensors(&mut self.prev_cpu, &mut self.rates, due);
        if due {
            self.gpu_at = Some(Instant::now());
            self.cards = snapshot.gpus.clone();
        } else if gpu {
            snapshot.gpus = self.cards.clone();
        }
        Reading::of(&snapshot)
    }
}

/// One GPU card's readings.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct Card {
    pub(crate) temp_c: Option<f64>,
    pub(crate) util_pct: Option<f64>,
    pub(crate) used_gib: Option<f64>,
    pub(crate) total_gib: Option<f64>,
}

/// The readings of one poll, every value optional and finite.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct Reading {
    pub(crate) cpu_temp_c: Option<f64>,
    pub(crate) disk_temp_c: Option<f64>,
    pub(crate) chipset_temp_c: Option<f64>,
    pub(crate) nic_temp_c: Option<f64>,
    pub(crate) power_w: Option<f64>,
    /// Needs two polls: absent from the first.
    pub(crate) cpu_pct: Option<f64>,
    pub(crate) mem_pct: Option<f64>,
    /// Needs two polls: absent from the first.
    pub(crate) disk_mbps: Option<f64>,
    /// Needs two polls: absent from the first.
    pub(crate) net_mbps: Option<f64>,
    pub(crate) gpus: Vec<Card>,
}

impl Reading {
    /// Cards are read through their probe-line form (`hosts::Frame`), the
    /// stable wire contract, rather than reaching into telemetry's fields.
    fn of(snapshot: &Snapshot) -> Self {
        let finite = |v: Option<f64>| v.filter(|v| v.is_finite());
        let field = |card: &Value, key: &str| finite(card[key].as_f64());
        let cards = serde_json::to_value(&snapshot.gpus).unwrap_or_default();
        Self {
            cpu_temp_c: snapshot
                .pkg
                .iter()
                .copied()
                .chain(snapshot.core_max)
                .filter(|t| t.is_finite())
                .reduce(f64::max),
            disk_temp_c: finite(snapshot.nvme),
            chipset_temp_c: finite(snapshot.pch),
            nic_temp_c: finite(snapshot.nic),
            power_w: finite(snapshot.power_w),
            cpu_pct: finite(snapshot.cpu_pct),
            mem_pct: finite(snapshot.mem_pct),
            disk_mbps: finite(snapshot.disk_mbps),
            net_mbps: finite(snapshot.net_mbps),
            gpus: cards
                .as_array()
                .map(|cards| {
                    cards
                        .iter()
                        .map(|card| Card {
                            temp_c: field(card, "temp"),
                            util_pct: field(card, "util"),
                            used_gib: field(card, "used_gib"),
                            total_gib: field(card, "total_gib"),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// Clock time of a timestamp, `HH:MM`.
fn clock(at: &str) -> &str {
    at.get(11..16).unwrap_or("?")
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// `… 3 more sessions`: what a cut list left out.
fn more(n: usize, one: &str) -> String {
    format!("… {n} more {one}{}", if n == 1 { "" } else { "s" })
}

/// Longest list kept in each part of the text summary, so a normal day fits on a screen.
const SESSIONS_SHOWN: usize = 5;
const PROJECTS_SHOWN: usize = 6;
const CALLOUTS_SHOWN: usize = 5;
const COMMITS_SHOWN: usize = 4;

/// The digest as plain text: totals, then stuck sessions and contended files
/// called out, then each project and its most recent sessions.
pub(crate) fn summary_text(digest: &Digest, coverage: &str) -> Vec<String> {
    let c = &digest.counts;
    let sessions: usize = digest.projects.iter().map(|p| p.sessions.len()).sum();
    let mut out = vec![
        clean(coverage),
        format!(
            "Activity on {} (UTC): {} · {} · {} · {} · {} · {}",
            digest.day,
            plural(digest.projects.len(), "project"),
            plural(sessions, "session"),
            plural(c.records, "record"),
            plural(c.edits, "edit"),
            plural(c.failures, "error"),
            plural(c.commits, "commit"),
        ),
    ];
    for stuck in digest.stuck.iter().take(CALLOUTS_SHOWN) {
        let run = stuck.stuck.as_ref().expect("stuck sessions carry a run");
        out.push(format!(
            "STUCK      {} {} in {}: \"{}\" failed {} times running, last at {}",
            stuck.agent,
            stuck.session,
            short_project(&stuck.project),
            fit(&run.command, 60),
            run.runs,
            clock(&stuck.last)
        ));
    }
    for file in digest.contended.iter().take(CALLOUTS_SHOWN) {
        out.push(format!(
            "CONTENDED  {} edited by {}: {}",
            fit(&clean(&file.path), 70),
            plural(file.sessions.len(), "session"),
            fit(&file.sessions.join(", "), 80)
        ));
    }
    for (extra, total) in [
        (digest.stuck.len(), "stuck session"),
        (digest.contended.len(), "contended file"),
    ] {
        if extra > CALLOUTS_SHOWN {
            out.push(format!(
                "           {}",
                more(extra - CALLOUTS_SHOWN, total)
            ));
        }
    }
    for project in digest.projects.iter().take(PROJECTS_SHOWN) {
        let pc = &project.counts;
        out.push(format!(
            "■ {}  {}–{}  {} · {} · {} · {} · {}",
            fit(&project.project, 48),
            clock(&project.first),
            clock(&project.last),
            plural(project.sessions.len(), "session"),
            plural(pc.tools, "tool"),
            plural(pc.edits, "edit"),
            plural(pc.failures, "error"),
            plural(pc.commits, "commit"),
        ));
        for session in project.sessions.iter().take(SESSIONS_SHOWN) {
            let sc = &session.counts;
            out.push(format!(
                "  {:<14} {:<22} {}–{} {:>4}p {:>4}t {:>3}e {:>3}✗ {:>2}c{}",
                fit(&session.agent, 14),
                fit(&session.session, 22),
                clock(&session.first),
                clock(&session.last),
                sc.prompts,
                sc.tools,
                sc.edits,
                sc.failures,
                sc.commits,
                if session.stuck.is_some() {
                    "  STUCK"
                } else {
                    ""
                }
            ));
            if !session.commits.is_empty() {
                let mut listed: Vec<String> = session
                    .commits
                    .iter()
                    .take(COMMITS_SHOWN)
                    .map(|sha| fit(sha, 12))
                    .collect();
                if session.commits.len() > COMMITS_SHOWN {
                    listed.push(format!("+{}", session.commits.len() - COMMITS_SHOWN));
                }
                out.push(format!("    commits: {}", listed.join(", ")));
            }
        }
        if project.sessions.len() > SESSIONS_SHOWN {
            out.push(format!(
                "  {}",
                more(project.sessions.len() - SESSIONS_SHOWN, "session")
            ));
        }
    }
    if digest.projects.len() > PROJECTS_SHOWN {
        out.push(more(digest.projects.len() - PROJECTS_SHOWN, "project"));
    }
    out.push("columns: p prompts · t tool calls · e edits · ✗ errors · c commits".into());
    out
}

/// The last two components of a project path, which is how people name it.
fn short_project(project: &str) -> &str {
    let trimmed = project.trim_end_matches('/');
    match trimmed.rmatch_indices('/').nth(1) {
        Some((i, _)) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// Cut to `width` characters, marking the cut with an ellipsis.
pub(crate) fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// `systemscape [--activity] --summary [--day YYYY-MM-DD] [--json]`
/// with the usual source flags. Prints the coverage line first, as the other
/// one-shot outputs do (inside the object for `--json`).
pub fn summary(args: &[String]) -> io::Result<()> {
    let mut sources = SourceFlags::new();
    let (mut day, mut json) = (None, false);
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if sources.take(arg, &mut rest)? {
            continue;
        }
        match arg.as_str() {
            "--summary" | "--activity" => {}
            "--json" => json = true,
            "--day" => {
                let value = rest
                    .next()
                    .ok_or_else(|| invalid("--day needs YYYY-MM-DD"))?;
                chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                    .map_err(|_| invalid("--day needs YYYY-MM-DD"))?;
                day = Some(value.clone());
            }
            other => {
                return Err(invalid(format!(
                    "unknown summary option {other}; see --help"
                )))
            }
        }
    }
    let mut sources = sources.open();
    let (events, coverage) = sources.load_drained(Duration::from_secs(3));
    let day = day
        .or_else(|| insight::days(&events).first().map(|d| (*d).to_owned()))
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
    let digest = insight::digest(&events, &day);
    if json {
        let value = serde_json::json!({"coverage": coverage, "digest": digest});
        // Record paths are only stripped of controls by the collector.
        println!("{}", scrub_value(value));
    } else {
        for line in summary_text(&digest, &coverage) {
            println!("{line}");
        }
    }
    Ok(())
}

/// How to dress the status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    Plain,
    /// tmux `#[fg=…]` attributes for `status-right`.
    Tmux,
}

/// `3 active · 1 error · 1 stuck · 71°C 412W · GPU 64°C`. Only numbers are
/// printed, never record text, so nothing in a history can reach the status bar.
pub(crate) fn status_line(
    pulse: insight::Pulse,
    reading: Option<&Reading>,
    style: Style,
) -> String {
    let paint = |colour: &str, text: String| match style {
        Style::Plain => text,
        Style::Tmux => format!("#[fg={colour}]{text}#[default]"),
    };
    let mut parts = vec![paint(
        if pulse.active > 0 { "green" } else { "default" },
        format!("{} active", pulse.active),
    )];
    if pulse.errors > 0 {
        parts.push(paint("red", plural(pulse.errors, "error")));
    }
    if pulse.stuck > 0 {
        parts.push(paint("yellow", format!("{} stuck", pulse.stuck)));
    }
    if let Some(reading) = reading {
        let mut heat = vec![];
        if let Some(t) = reading.cpu_temp_c {
            heat.push(format!("{t:.0}°C"));
        }
        if let Some(w) = reading.power_w {
            heat.push(format!("{w:.0}W"));
        }
        if !heat.is_empty() {
            parts.push(heat.join(" "));
        }
        let hottest = reading
            .gpus
            .iter()
            .filter_map(|g| g.temp_c)
            .reduce(f64::max);
        if let Some(t) = hottest {
            parts.push(format!("GPU {t:.0}°C"));
        }
    }
    parts.join(" · ")
}

/// `systemscape --status [--tmux] [--gpu] [--no-sensors]` with the usual
/// source flags: one bounded collector poll and, unless `--no-sensors` or
/// `--demo`, one sensor poll (nvidia-smi only with `--gpu`).
pub fn status(args: &[String]) -> io::Result<()> {
    let mut sources = SourceFlags::new();
    let (mut style, mut gpu, mut sensors) = (Style::Plain, false, true);
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if sources.take(arg, &mut rest)? {
            continue;
        }
        match arg.as_str() {
            "--status" | "--activity" => {}
            "--tmux" => style = Style::Tmux,
            "--gpu" => gpu = true,
            "--no-sensors" => sensors = false,
            other => {
                return Err(invalid(format!(
                    "unknown status option {other}; see --help"
                )))
            }
        }
    }
    let mut sources = sources.open();
    let reading = (sensors && !sources.demo()).then(|| Sensors::default().read(gpu));
    let (events, _) = sources.load();
    let pulse = insight::pulse(&events, chrono::Utc::now());
    println!("{}", status_line(pulse, reading.as_ref(), style));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insight::tests::record;
    use crate::insight::Pulse;

    #[test]
    fn the_status_line_is_short_plain_by_default_and_styled_for_tmux() {
        let pulse = Pulse {
            active: 3,
            errors: 1,
            stuck: 1,
        };
        let reading = Reading {
            cpu_temp_c: Some(71.4),
            power_w: Some(412.2),
            gpus: vec![
                Card {
                    temp_c: Some(55.0),
                    ..Card::default()
                },
                Card {
                    temp_c: Some(64.0),
                    ..Card::default()
                },
            ],
            ..Reading::default()
        };
        assert_eq!(
            status_line(pulse, Some(&reading), Style::Plain),
            "3 active · 1 error · 1 stuck · 71°C 412W · GPU 64°C"
        );
        let quiet = status_line(Pulse::default(), None, Style::Plain);
        assert_eq!(quiet, "0 active");
        assert!(!quiet.contains('\x1b'));
        let tmux = status_line(pulse, None, Style::Tmux);
        assert_eq!(
            tmux,
            "#[fg=green]3 active#[default] · #[fg=red]1 error#[default] · #[fg=yellow]1 stuck#[default]"
        );
    }

    #[test]
    fn readings_take_the_hottest_cpu_sensor_and_drop_non_finite_values() {
        let snapshot: Snapshot = serde_json::from_value(serde_json::json!({
            "pkg": [60.0, 72.5], "core_max": 70.0, "power_w": 300.0,
            "gpus": [{"temp": 50.0, "util": 90.0, "used_gib": 2.0, "total_gib": 24.0}, {}]
        }))
        .unwrap();
        let mut snapshot = snapshot;
        snapshot.mem_pct = Some(f64::NAN);
        let reading = Reading::of(&snapshot);
        assert_eq!(reading.cpu_temp_c, Some(72.5));
        assert_eq!(reading.mem_pct, None);
        assert_eq!(
            reading.gpus.len(),
            2,
            "a card with no readings keeps its index"
        );
        assert_eq!(reading.gpus[0].util_pct, Some(90.0));
        assert_eq!(reading.gpus[1], Card::default());
    }

    #[test]
    fn the_summary_calls_out_stuck_sessions_and_contended_files_and_fits_a_screen() {
        let events = demo_events();
        let day = insight::days(&events)[0].to_owned();
        let digest = insight::digest(&events, &day);
        let lines = summary_text(&digest, DEMO_COVERAGE);
        assert_eq!(lines[0], DEMO_COVERAGE);
        assert!(lines[1].starts_with(&format!("Activity on {day} (UTC)")));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("STUCK      codex 77e385bc-demo")
                && l.contains("\"Bash cargo test --locked\" failed 3 times running")));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("CONTENDED  /workspace/")));
        assert!(lines.iter().any(|l| l.ends_with("STUCK")));
        assert!(lines.len() <= 50, "{} lines", lines.len());
        assert!(lines.iter().all(|l| l.chars().count() <= 160));
        assert_eq!(lines, summary_text(&digest, DEMO_COVERAGE), "deterministic");
    }

    #[test]
    fn a_long_day_is_cut_to_a_screen_with_counts_of_what_was_left_out() {
        let mut events = vec![];
        for p in 0..12 {
            for s in 0..9 {
                let mut e = record(&format!("s{p}-{s}"), p * 100 + s, "prompt", "go");
                e.project = format!("/work/p{p}");
                events.push(e);
            }
        }
        let digest = insight::digest(&events, "2026-11-18");
        let lines = summary_text(&digest, "Sources: test");
        assert!(lines.contains(&"… 6 more projects".to_owned()));
        assert!(lines.contains(&"  … 4 more sessions".to_owned()));
        assert!(lines.len() <= 55, "{} lines", lines.len());
    }

    #[test]
    fn source_flags_take_their_values_and_leave_the_rest() {
        let argv: Vec<String> = ["--home", "/h", "--tmux", "--demo", "--archive"]
            .map(String::from)
            .to_vec();
        let mut flags = SourceFlags::new();
        let mut rest = argv.iter();
        let mut left = vec![];
        let mut failed = false;
        while let Some(arg) = rest.next() {
            match flags.take(arg, &mut rest) {
                Ok(true) => {}
                Ok(false) => left.push(arg.as_str()),
                Err(_) => failed = true,
            }
        }
        assert_eq!(left, ["--tmux"]);
        assert!(flags.demo && failed, "--archive without a path is refused");
        assert_eq!(flags.home.as_deref(), Some(std::path::Path::new("/h")));
    }

    #[test]
    fn json_output_scrubs_directional_and_invisible_characters_from_paths() {
        // The collector strips controls from paths but keeps other text whole.
        let edit = |session: &str| Event {
            path: Some("/work/app/evil\u{202e}sr.rs\u{200b}".into()),
            ..record(session, 10, "tool", "Edit evil")
        };
        let digest = insight::digest(&[edit("a"), edit("b")], "2026-11-18");
        assert!(digest.contended[0].path.contains('\u{202e}'));
        let printed = scrub_value(serde_json::json!({"digest": digest})).to_string();
        assert!(printed.contains("/work/app/evilsr.rs"));
        assert!(!printed.contains('\u{202e}') && !printed.contains('\u{200b}'));
    }
}
