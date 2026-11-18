//! Alert rules: thresholds on telemetry channels and on recorded activity.
//!
//! A rules file (`alerts.toml`, beside the hosts file) lists rules. Each rule
//! belongs to one view by its `when`: `channel` and `link-lost` rules are
//! evaluated by telemetry, `session-error`, `error-burst` and `quiet` by
//! activity. Both views read the whole file and refuse it whole if any rule is
//! wrong, so one file serves both.
//!
//! Evaluation is a pure state machine ([`Book`]): rules, readings and a time
//! go in, transitions come out, so it is tested without a terminal or a clock.
//! A transition becomes a notification through a [`Sink`], so tests capture
//! exactly what would be written to the terminal or passed to tmux.
//! Notification text is built only from rule names, host numbers and numbers,
//! never from record text or host names, because tmux expands `#{…}` in
//! messages and terminals split OSC 777 fields on `;`.
//!
//! ```toml
//! [[rule]]
//! name = "cpu-hot"
//! when = "channel"
//! channel = "CPU°"
//! above = 85
//! clear = 80
//! for = 60
//! notify = ["bell", "osc9"]
//! ```
use crate::activity_data::{clean, Event};
use crate::pixels::{PixelCanvas, Rgb};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Longest duration any rule may name: a week.
const MAX_SECONDS: f64 = 7.0 * 86_400.0;
/// How often a view hands its readings to the rules.
const EVERY: Duration = Duration::from_secs(1);
const BANNER_BG: Rgb = (92, 18, 30);
const BANNER_FG: Rgb = (255, 226, 226);
const LIST_BG: Rgb = (16, 21, 32);
const MUTED: Rgb = (126, 144, 164);
const RED: Rgb = (255, 95, 95);
const AMBER: Rgb = (240, 200, 90);
const GREEN: Rgb = (110, 220, 110);

// ---------------------------------------------------------------- rules

/// Which view evaluates a rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    Telemetry,
    Activity,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            View::Telemetry => "telemetry",
            View::Activity => "activity",
        }
    }
}

/// One way of telling the operator that a rule fired or cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Notifier {
    /// BEL, which tmux handles itself (its `bell-action`).
    Bell,
    /// OSC 9 desktop notification, where the terminal implements it.
    Osc9,
    /// OSC 777 `notify`, where the terminal implements it.
    Osc777,
    /// `tmux display-message` in this pane's client.
    TmuxMessage,
    /// A tmux window option naming the firing rules, for a status line.
    TmuxOption,
}

const NOTIFIERS: [(&str, Notifier); 5] = [
    ("bell", Notifier::Bell),
    ("osc9", Notifier::Osc9),
    ("osc777", Notifier::Osc777),
    ("tmux-message", Notifier::TmuxMessage),
    ("tmux-option", Notifier::TmuxOption),
];

impl Notifier {
    fn name(self) -> &'static str {
        NOTIFIERS
            .iter()
            .find(|(_, n)| *n == self)
            .map_or("?", |(name, _)| name)
    }

    /// Notifiers that interrupt: these are rate-limited. The tmux option is
    /// state, kept current on every transition.
    fn interrupts(self) -> bool {
        self != Notifier::TmuxOption
    }
}

/// What makes a rule fire, and what clears it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Trigger {
    /// A telemetry channel beyond `level` for `hold` seconds; clears at `clear`.
    Channel {
        label: String,
        host: Option<String>,
        above: bool,
        level: f64,
        clear: f64,
        hold: f64,
    },
    /// A remote host with no live link for `hold` seconds; clears when live.
    LinkLost { host: Option<String>, hold: f64 },
    /// A session whose newest record is a failure, quiet for `quiet` seconds,
    /// and no older than `within`.
    SessionError { quiet: f64, within: f64 },
    /// A session with more than `errors` failures in the last `within` seconds;
    /// clears when its window holds none.
    ErrorBurst { errors: u32, within: f64 },
    /// No record from any session for `after` seconds, once there was activity.
    Quiet { after: f64 },
}

impl Trigger {
    fn view(&self) -> View {
        match self {
            Trigger::Channel { .. } | Trigger::LinkLost { .. } => View::Telemetry,
            _ => View::Activity,
        }
    }

    /// Seconds the condition must hold before the rule fires. Activity rules
    /// are measured on record timestamps, so they need no hold of their own.
    fn hold(&self) -> f64 {
        match self {
            Trigger::Channel { hold, .. } | Trigger::LinkLost { hold, .. } => *hold,
            _ => 0.0,
        }
    }

    /// One line for the rule list.
    fn describe(&self) -> String {
        let on = |host: &Option<String>, all: &str| match host {
            Some(name) => format!(" on {}", clean(name)),
            None => all.to_string(),
        };
        match self {
            Trigger::Channel {
                label,
                host,
                above,
                level,
                clear,
                hold,
            } => format!(
                "{label} {} {}{}, clears at {}{}",
                if *above { ">" } else { "<" },
                num(*level),
                held_for(*hold),
                num(*clear),
                on(host, ""),
            ),
            Trigger::LinkLost { host, hold } => {
                format!(
                    "no link for {}{}",
                    span(*hold),
                    on(host, " on any remote host")
                )
            }
            Trigger::SessionError { quiet, within } => format!(
                "a session ended on an error (quiet {}, last {})",
                span(*quiet),
                span(*within)
            ),
            Trigger::ErrorBurst { errors, within } => {
                format!(
                    "a session with more than {errors} errors in {}",
                    span(*within)
                )
            }
            Trigger::Quiet { after } => format!("no record for {} after activity", span(*after)),
        }
    }
}

/// A validated rule.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Rule {
    pub name: String,
    pub trigger: Trigger,
    pub notify: Vec<Notifier>,
    /// Minimum seconds between interrupting notifications from this rule.
    pub rate_limit: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default, rename = "rule")]
    rules: Vec<RuleEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleEntry {
    name: String,
    when: String,
    channel: Option<String>,
    host: Option<String>,
    above: Option<f64>,
    below: Option<f64>,
    clear: Option<f64>,
    #[serde(rename = "for")]
    hold: Option<f64>,
    within: Option<f64>,
    errors: Option<u32>,
    #[serde(default)]
    notify: Vec<String>,
    rate_limit: Option<f64>,
}

/// `$XDG_CONFIG_HOME/systemscape/alerts.toml`, else `~/.config/systemscape/alerts.toml`.
pub(crate) fn default_path() -> Option<PathBuf> {
    crate::hosts::default_path().map(|hosts| hosts.with_file_name("alerts.toml"))
}

/// Parse and validate a whole rules file, for both views.
pub(crate) fn parse(text: &str) -> Result<Vec<Rule>, String> {
    let file: FileConfig = toml::from_str(text).map_err(|e| e.message().to_string())?;
    if file.rules.is_empty() {
        return Err("the file defines no [[rule]]".into());
    }
    let mut rules: Vec<Rule> = Vec::new();
    for entry in file.rules {
        check_name(&entry.name)?;
        if rules.iter().any(|r| r.name == entry.name) {
            return Err(format!("rule name {:?} is used twice", entry.name));
        }
        rules.push(validate(entry)?);
    }
    Ok(rules)
}

/// Rule names reach tmux and terminal parsers: plain characters only.
fn check_name(name: &str) -> Result<(), String> {
    let plain = |c: char| c.is_ascii_alphanumeric() || " _.-".contains(c);
    let ok = (1..=24).contains(&name.len())
        && name.chars().all(plain)
        && !name.starts_with(' ')
        && !name.ends_with(' ');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "rule name {name:?} must be 1 to 24 letters, digits, spaces, '-', '_' or '.', \
             because it is sent to tmux and the terminal"
        ))
    }
}

/// Map a channel name to the label the telemetry view shows, or say why not.
/// Case does not matter; anything that could mean two channels is refused.
pub(crate) fn channel_label(text: &str) -> Result<String, String> {
    let name = text.trim().to_uppercase();
    if ["CPU°", "DISK°", "PWR", "LOAD", "MEM", "IO", "NET"].contains(&name.as_str()) {
        return Ok(name);
    }
    let card = |prefix: &str, suffix: &str| -> Option<String> {
        let digits = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
        let ok = !digits.is_empty()
            && digits.len() <= 2
            && digits.chars().all(|c| c.is_ascii_digit())
            && (digits == "0" || !digits.starts_with('0'));
        ok.then(|| digits.to_string())
    };
    if card("GPU", "°").is_some() || card("GPU", "%").is_some() || card("VRAM", "").is_some() {
        return Ok(name);
    }
    let why = match name.as_str() {
        "CPU" => "CPU names no channel: CPU° is the temperature, LOAD the utilisation".into(),
        "DISK" | "NVME" => "the disk temperature channel is DISK°".into(),
        "GPU" | "GPU°" | "GPU%" | "VRAM" => {
            format!(
                "{name} needs a card number as the view shows it, such as GPU0°, GPU0% or VRAM0"
            )
        }
        _ => match card("GPU", "") {
            Some(n) => format!("GPU{n} could be GPU{n}° (temperature) or GPU{n}% (utilisation)"),
            None => format!(
                "unknown channel {name:?}; name a channel as the view labels it: \
                 CPU°, GPU0°, DISK°, PWR, LOAD, MEM, GPU0%, VRAM0, IO or NET"
            ),
        },
    };
    Err(why)
}

fn validate(entry: RuleEntry) -> Result<Rule, String> {
    let name = entry.name;
    let at = |e: String| format!("rule {name:?}: {e}");
    let kind = entry.when.as_str();
    let fields = [
        ("channel", entry.channel.is_some()),
        ("host", entry.host.is_some()),
        ("above", entry.above.is_some()),
        ("below", entry.below.is_some()),
        ("clear", entry.clear.is_some()),
        ("for", entry.hold.is_some()),
        ("within", entry.within.is_some()),
        ("errors", entry.errors.is_some()),
    ];
    let allowed: &[&str] = match kind {
        "channel" => &["channel", "host", "above", "below", "clear", "for"],
        "link-lost" => &["host", "for"],
        "session-error" => &["for", "within"],
        "error-burst" => &["errors", "within"],
        "quiet" => &["for"],
        other => {
            return Err(at(format!(
                "when = {other:?} is not a rule kind; use channel, link-lost, \
                 session-error, error-burst or quiet"
            )))
        }
    };
    if let Some((field, _)) = fields.iter().find(|(f, set)| *set && !allowed.contains(f)) {
        return Err(at(format!("{field} does not apply to when = {kind:?}")));
    }
    let seconds =
        |value: Option<f64>, default: Option<f64>, lo: f64, what: &str| match value.or(default) {
            None => Err(at(format!("when = {kind:?} needs {what} (seconds)"))),
            Some(v) if v.is_finite() && (lo..=MAX_SECONDS).contains(&v) => Ok(v),
            Some(_) => Err(at(format!(
                "{what} must be between {lo} and {MAX_SECONDS} seconds"
            ))),
        };
    let host = match entry.host {
        Some(h) if h.is_empty() => return Err(at("host must name a host".into())),
        other => other,
    };
    let trigger = match kind {
        "channel" => {
            let label = entry
                .channel
                .ok_or_else(|| at("when = \"channel\" needs channel, such as \"CPU°\"".into()))
                .and_then(|c| channel_label(&c).map_err(at))?;
            let (above, level) = match (entry.above, entry.below) {
                (Some(_), Some(_)) => return Err(at("set above or below, not both".into())),
                (None, None) => return Err(at("set above or below, the threshold".into())),
                (Some(v), None) => (true, v),
                (None, Some(v)) => (false, v),
            };
            let clear = entry.clear.ok_or_else(|| {
                at(
                    "set clear, the level at which a firing rule clears, so a value \
                    hovering at the threshold does not fire again and again"
                        .into(),
                )
            })?;
            if !level.is_finite() || !clear.is_finite() {
                return Err(at("thresholds must be numbers".into()));
            }
            if above && clear >= level {
                return Err(at(format!("clear ({clear}) must be below above ({level})")));
            }
            if !above && clear <= level {
                return Err(at(format!("clear ({clear}) must be above below ({level})")));
            }
            Trigger::Channel {
                label,
                host,
                above,
                level,
                clear,
                hold: seconds(entry.hold, Some(0.0), 0.0, "for")?,
            }
        }
        "link-lost" => Trigger::LinkLost {
            host,
            hold: seconds(entry.hold, None, 1.0, "for")?,
        },
        "session-error" => {
            let quiet = seconds(entry.hold, Some(60.0), 0.0, "for")?;
            let within = seconds(entry.within, Some(600.0), 1.0, "within")?;
            if within <= quiet {
                return Err(at(format!(
                    "within ({within}) must be longer than for ({quiet})"
                )));
            }
            Trigger::SessionError { quiet, within }
        }
        "error-burst" => {
            let errors = entry
                .errors
                .ok_or_else(|| at("when = \"error-burst\" needs errors, a count".into()))?;
            if !(1..=10_000).contains(&errors) {
                return Err(at("errors must be between 1 and 10000".into()));
            }
            Trigger::ErrorBurst {
                errors,
                within: seconds(entry.within, None, 1.0, "within")?,
            }
        }
        _ => Trigger::Quiet {
            after: seconds(entry.hold, None, 1.0, "for")?,
        },
    };
    let mut notify = Vec::new();
    for word in &entry.notify {
        let notifier = NOTIFIERS
            .iter()
            .find(|(n, _)| n == word)
            .map(|(_, n)| *n)
            .ok_or_else(|| {
                at(format!(
                    "notify {word:?} is not one of bell, osc9, osc777, tmux-message, tmux-option"
                ))
            })?;
        if notify.contains(&notifier) {
            return Err(at(format!("notify lists {word:?} twice")));
        }
        notify.push(notifier);
    }
    let rate_limit = seconds(entry.rate_limit, Some(60.0), 0.0, "rate_limit")?;
    Ok(Rule {
        name: name.clone(),
        trigger,
        notify,
        rate_limit,
    })
}

/// Check telemetry rules against the hosts on screen: `hosts` is each
/// source's name and whether it is remote, `federated` whether a hosts file
/// is in use.
fn bind(rules: &[Rule], hosts: &[(String, bool)], federated: bool) -> Result<(), String> {
    for rule in rules {
        let at = |e: String| format!("rule {:?}: {e}", rule.name);
        let (host, link) = match &rule.trigger {
            Trigger::Channel { host, .. } => (host, false),
            Trigger::LinkLost { host, .. } => (host, true),
            _ => continue,
        };
        if !federated && (host.is_some() || link) {
            return Err(at(
                "names a host or a link, which needs a hosts file with a [[host]]".into(),
            ));
        }
        match host {
            Some(name) => match hosts.iter().find(|(n, _)| n == name) {
                None => return Err(at(format!("host {name:?} is not in the hosts file"))),
                Some((_, false)) if link => {
                    return Err(at(format!(
                        "host {name:?} is this machine; only a remote host's link can be lost"
                    )))
                }
                Some(_) => {}
            },
            None if link && !hosts.iter().any(|(_, remote)| *remote) => {
                return Err(at("link-lost needs at least one remote [[host]]".into()))
            }
            None => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- state machine

/// Where one rule stands for one subject.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Phase {
    Ok,
    /// The trip condition has held continuously since `since`.
    Pending {
        since: f64,
    },
    Firing {
        since: f64,
    },
}

/// One look at a rule's condition: whether it trips, whether it clears, and
/// the number to report. Trip and clear are never both true; between them is
/// the hysteresis band, where a firing rule stays firing and a pending one
/// starts again.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Obs {
    pub trip: bool,
    pub clear: bool,
    pub value: f64,
}

/// Advance one rule for one subject. `None` is a missing reading (a stale
/// host, a card not reported yet): it neither fires nor clears, and a pending
/// hold starts again. Returns `Some(true)` when the rule fires and
/// `Some(false)` when it clears.
pub(crate) fn advance(phase: &mut Phase, obs: Option<Obs>, hold: f64, now: f64) -> Option<bool> {
    match (*phase, obs) {
        (Phase::Firing { .. }, Some(o)) if o.clear => {
            *phase = Phase::Ok;
            Some(false)
        }
        (Phase::Firing { .. }, _) => None,
        (_, Some(o)) if o.trip => {
            let since = match *phase {
                Phase::Pending { since } => since,
                _ => now,
            };
            if now - since >= hold {
                *phase = Phase::Firing { since: now };
                Some(true)
            } else {
                *phase = Phase::Pending { since };
                None
            }
        }
        _ => {
            *phase = Phase::Ok;
            None
        }
    }
}

/// What a rule is evaluated for.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Subject {
    /// The whole view (the `quiet` rule).
    Whole,
    /// A telemetry source, by its place in the host strip (0-based).
    Host(usize),
    /// An activity session, by agent and id. Never shown or sent.
    Session(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Track {
    phase: Phase,
    value: f64,
}

/// A rule firing or clearing for one subject.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Change {
    pub rule: usize,
    pub subject: Subject,
    pub fired: bool,
    pub value: f64,
}

/// One telemetry source as the rules see it.
pub(crate) struct HostReading {
    /// The name in the host strip (empty without a hosts file).
    pub name: String,
    /// `None` for this machine; for a remote, whether its link is live.
    pub remote_live: Option<bool>,
    /// Each channel's label and live value.
    pub readings: Vec<(String, Option<f64>)>,
}

/// Every rule of one view and where each stands. Only subjects that are not
/// `Ok` are kept, so the book stays as small as what is happening.
pub(crate) struct Book {
    rules: Vec<Rule>,
    tracks: BTreeMap<(usize, Subject), Track>,
    /// Per rule: a `quiet` rule arms once it has seen a recent record.
    armed: Vec<bool>,
}

impl Book {
    pub(crate) fn new(rules: Vec<Rule>) -> Self {
        let armed = vec![false; rules.len()];
        Self {
            rules,
            tracks: BTreeMap::new(),
            armed,
        }
    }

    fn step(
        &mut self,
        rule: usize,
        subject: Subject,
        obs: Option<Obs>,
        now: f64,
    ) -> Option<Change> {
        let hold = self.rules[rule].trigger.hold();
        let key = (rule, subject);
        let mut track = self.tracks.get(&key).copied().unwrap_or(Track {
            phase: Phase::Ok,
            value: f64::NAN,
        });
        if let Some(o) = obs {
            track.value = o.value;
        }
        let change = advance(&mut track.phase, obs, hold, now);
        if track.phase == Phase::Ok {
            self.tracks.remove(&key);
        } else {
            self.tracks.insert(key.clone(), track);
        }
        change.map(|fired| Change {
            rule,
            subject: key.1,
            fired,
            value: track.value,
        })
    }

    /// Evaluate telemetry rules against every source at time `now` (seconds).
    pub(crate) fn telemetry(&mut self, hosts: &[HostReading], now: f64) -> Vec<Change> {
        let mut changes = Vec::new();
        for rule in 0..self.rules.len() {
            let trigger = self.rules[rule].trigger.clone();
            for (index, host) in hosts.iter().enumerate() {
                let obs = match &trigger {
                    Trigger::Channel {
                        label,
                        above,
                        level,
                        clear,
                        ..
                    } => host
                        .readings
                        .iter()
                        .find(|(l, _)| l == label)
                        .and_then(|(_, v)| *v)
                        .map(|v| Obs {
                            trip: if *above { v > *level } else { v < *level },
                            clear: if *above { v <= *clear } else { v >= *clear },
                            value: v,
                        }),
                    Trigger::LinkLost { hold, .. } => match host.remote_live {
                        None => continue,
                        Some(live) => Some(Obs {
                            trip: !live,
                            clear: live,
                            value: *hold,
                        }),
                    },
                    _ => continue,
                };
                let wanted = match &trigger {
                    Trigger::Channel { host, .. } | Trigger::LinkLost { host, .. } => host,
                    _ => &None,
                };
                if wanted.as_ref().is_some_and(|name| *name != host.name) {
                    continue;
                }
                changes.extend(self.step(rule, Subject::Host(index), obs, now));
            }
        }
        changes
    }

    /// Evaluate activity rules against the records at Unix time `now`.
    pub(crate) fn activity(&mut self, events: &[Event], now: f64) -> Vec<Change> {
        let facts = session_facts(events);
        let newest = facts.values().map(|f| f.last).reduce(f64::max);
        let mut changes = Vec::new();
        for rule in 0..self.rules.len() {
            let observed: Vec<(String, Obs)> = match self.rules[rule].trigger {
                Trigger::SessionError { quiet, within } => facts
                    .iter()
                    .filter(|(_, f)| f.ended_on_error && (quiet..=within).contains(&(now - f.last)))
                    .map(|(key, _)| {
                        let obs = Obs {
                            trip: true,
                            clear: false,
                            value: 1.0,
                        };
                        (key.clone(), obs)
                    })
                    .collect(),
                Trigger::ErrorBurst { errors, within } => facts
                    .iter()
                    .filter_map(|(key, f)| {
                        // A little leeway for a clock that runs ahead of ours.
                        let count = f
                            .failures
                            .iter()
                            .filter(|&&t| now - t <= within && t - now <= 60.0)
                            .count();
                        let obs = Obs {
                            trip: count > errors as usize,
                            clear: false,
                            value: count as f64,
                        };
                        (count > 0).then(|| (key.clone(), obs))
                    })
                    .collect(),
                Trigger::Quiet { after } => {
                    let obs = newest.map(|last| {
                        let age = now - last;
                        if age < after {
                            self.armed[rule] = true;
                        }
                        Obs {
                            trip: self.armed[rule] && age >= after,
                            clear: age < after,
                            value: age,
                        }
                    });
                    changes.extend(self.step(rule, Subject::Whole, obs, now));
                    continue;
                }
                _ => continue,
            };
            // A tracked session no longer observed has nothing left to report.
            let gone: Vec<String> = self
                .tracks
                .keys()
                .filter_map(|(r, s)| match s {
                    Subject::Session(key) if *r == rule => Some(key.clone()),
                    _ => None,
                })
                .filter(|key| observed.iter().all(|(k, _)| k != key))
                .collect();
            let cleared = Obs {
                trip: false,
                clear: true,
                value: 0.0,
            };
            for (key, obs) in observed {
                changes.extend(self.step(rule, Subject::Session(key), Some(obs), now));
            }
            for key in gone {
                changes.extend(self.step(rule, Subject::Session(key), Some(cleared), now));
            }
        }
        changes
    }

    /// Subjects firing for a rule, with their tracks.
    fn firing(&self, rule: usize) -> impl Iterator<Item = (&Subject, &Track)> {
        self.tracks
            .iter()
            .filter(move |((r, _), t)| *r == rule && matches!(t.phase, Phase::Firing { .. }))
            .map(|((_, s), t)| (s, t))
    }

    pub(crate) fn any_firing(&self) -> bool {
        self.tracks
            .values()
            .any(|t| matches!(t.phase, Phase::Firing { .. }))
    }
}

/// What the activity rules need to know about one session.
#[derive(Default)]
struct SessionFacts {
    /// Unix seconds of the newest record.
    last: f64,
    ended_on_error: bool,
    /// Unix seconds of every failure.
    failures: Vec<f64>,
}

/// Group records by session. Records whose time does not parse are skipped.
fn session_facts(events: &[Event]) -> BTreeMap<String, SessionFacts> {
    let mut facts: BTreeMap<String, SessionFacts> = BTreeMap::new();
    for event in events {
        let Ok(at) = chrono::DateTime::parse_from_rfc3339(&event.at) else {
            continue;
        };
        let t = at.timestamp_millis() as f64 / 1000.0;
        let failure = event.kind == "failure";
        let session = facts
            .entry(format!("{}\u{1f}{}", event.agent, event.session))
            .or_insert(SessionFacts {
                last: f64::NEG_INFINITY,
                ..SessionFacts::default()
            });
        if t >= session.last {
            session.last = t;
            session.ended_on_error = failure;
        }
        if failure {
            session.failures.push(t);
        }
    }
    facts
}

// ---------------------------------------------------------------- text

/// A number for a message: whole when it is nearly whole, else one decimal.
fn num(v: f64) -> String {
    if !v.is_finite() {
        "?".into()
    } else if (v - v.round()).abs() < 0.05 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// A duration in seconds, minutes or hours, whichever reads best.
fn span(seconds: f64) -> String {
    if seconds < 120.0 {
        format!("{:.0} s", seconds.max(0.0))
    } else if seconds < 7200.0 {
        format!("{} min", num(seconds / 60.0))
    } else {
        format!("{} h", num(seconds / 3600.0))
    }
}

fn held_for(hold: f64) -> String {
    if hold > 0.0 {
        format!(" for {}", span(hold))
    } else {
        String::new()
    }
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// The notification for a change. Only the rule name (plain characters by
/// [`check_name`]), the host's number in the strip and numbers go in.
/// `firing` is how many subjects of the rule fire after the change.
fn message(rule: &Rule, change: &Change, firing: usize, host: Option<usize>) -> String {
    let on = host.map(|n| format!(" on host {n}")).unwrap_or_default();
    let verb = if change.fired { "fired" } else { "cleared" };
    let v = change.value;
    let detail = match (&rule.trigger, change.fired) {
        (
            Trigger::Channel {
                above, level, hold, ..
            },
            true,
        ) => format!(
            "{} {} {}{}",
            num(v),
            if *above { "above" } else { "below" },
            num(*level),
            held_for(*hold)
        ),
        (Trigger::Channel { .. }, false) => format!("now {}", num(v)),
        (Trigger::LinkLost { hold, .. }, true) => format!("no link for {}", span(*hold)),
        (Trigger::LinkLost { .. }, false) => "link back".into(),
        (Trigger::SessionError { .. }, _) => {
            format!("{} ended on an error", plural(firing, "session"))
        }
        (Trigger::ErrorBurst { errors, within }, true) => format!(
            "{} in {}, more than {errors}",
            plural(v as usize, "error"),
            span(*within)
        ),
        (Trigger::ErrorBurst { within, .. }, false) => format!("no error in {}", span(*within)),
        (Trigger::Quiet { .. }, true) => format!("no record for {}", span(v)),
        (Trigger::Quiet { .. }, false) => "records again".into(),
    };
    format!("systemscape: {} {verb}{on}: {detail}", rule.name)
}

// ---------------------------------------------------------------- notifications

/// Where notifications go. The live sink writes to the terminal and runs
/// tmux; tests record the calls instead.
pub(crate) trait Sink {
    /// Bytes for the terminal: a bell or an OSC sequence, already wrapped
    /// for tmux passthrough where that is needed.
    fn terminal(&mut self, bytes: &str);
    /// Run `tmux` with this argv. Never a shell string.
    fn tmux(&mut self, argv: &[String]);
}

/// The real terminal and tmux.
struct LiveSink;

impl Sink for LiveSink {
    fn terminal(&mut self, bytes: &str) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes.as_bytes());
        let _ = out.flush();
    }

    fn tmux(&mut self, argv: &[String]) {
        let _ = std::process::Command::new("tmux")
            .args(argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// tmux's `allow-passthrough` setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Passthrough {
    /// Wrapped sequences are dropped (tmux's default).
    Off,
    /// Passed through only from a pane that is on screen.
    On,
    /// Passed through from any pane.
    All,
}

/// Whether this process runs in tmux, and how tmux treats escape sequences.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Place {
    /// `$TMUX_PANE`, when inside tmux.
    pub pane: Option<String>,
    pub passthrough: Passthrough,
}

impl Place {
    /// Ask the environment, and tmux once, at start-up.
    fn detect() -> Self {
        let pane = std::env::var_os("TMUX")
            .and_then(|_| std::env::var("TMUX_PANE").ok())
            .filter(|p| !p.is_empty());
        let passthrough = match &pane {
            None => Passthrough::Off,
            Some(pane) => std::process::Command::new("tmux")
                .args(["display-message", "-p", "-t", pane, "#{allow-passthrough}"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map_or(Passthrough::Off, |out| {
                    match String::from_utf8_lossy(&out.stdout).trim() {
                        "all" => Passthrough::All,
                        "on" => Passthrough::On,
                        _ => Passthrough::Off,
                    }
                }),
        };
        Self { pane, passthrough }
    }

    /// An OSC sequence as it must be written here, or `None` when tmux would
    /// drop it.
    fn osc(&self, sequence: &str, visible: bool) -> Option<String> {
        match (&self.pane, self.passthrough) {
            (None, _) => Some(sequence.to_string()),
            (Some(_), Passthrough::All) => Some(tmux_wrap(sequence)),
            (Some(_), Passthrough::On) if visible => Some(tmux_wrap(sequence)),
            _ => None,
        }
    }

    /// Notes for the rule list on what cannot reach the operator here.
    fn notes(&self) -> Vec<&'static str> {
        match (&self.pane, self.passthrough) {
            (None, _) => vec!["not inside tmux: tmux-message and tmux-option do nothing"],
            (Some(_), Passthrough::Off) => {
                vec!["tmux allow-passthrough is off: osc9/osc777 go out as tmux messages instead"]
            }
            (Some(_), Passthrough::On) => vec![
                "tmux allow-passthrough is on: osc9/osc777 reach the terminal only while this \
                 pane is on screen, otherwise they go out as tmux messages",
            ],
            (Some(_), Passthrough::All) => Vec::new(),
        }
    }
}

/// tmux's passthrough wrapper: a DCS whose escapes are doubled.
pub(crate) fn tmux_wrap(sequence: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
}

/// Turns changes into notifications: rate limits, the fallback when tmux
/// would drop an OSC, and the tmux option kept in step with what fires.
pub(crate) struct Dispatch {
    place: Place,
    /// tmux window option holding the firing rules' names.
    option: &'static str,
    /// The option's current value, so tmux runs only when it changes.
    shown: String,
    /// Per rule: when it last interrupted, and how many it has held back.
    last: Vec<Option<f64>>,
    held: Vec<u32>,
}

impl Dispatch {
    pub(crate) fn new(place: Place, view: View, rules: usize) -> Self {
        Self {
            place,
            option: match view {
                View::Telemetry => "@systemscape_telemetry",
                View::Activity => "@systemscape_activity",
            },
            shown: String::new(),
            last: vec![None; rules],
            held: vec![0; rules],
        }
    }

    /// Send the interrupting notifications a rule asks for, unless it spoke
    /// less than `rate_limit` seconds ago.
    pub(crate) fn send(
        &mut self,
        rule_index: usize,
        rule: &Rule,
        text: &str,
        now: f64,
        visible: bool,
        sink: &mut dyn Sink,
    ) {
        if !rule.notify.iter().any(|n| n.interrupts()) {
            return;
        }
        if self.last[rule_index].is_some_and(|t| now - t < rule.rate_limit) {
            self.held[rule_index] = self.held[rule_index].saturating_add(1);
            return;
        }
        self.last[rule_index] = Some(now);
        let mut dropped = false;
        for notifier in &rule.notify {
            match notifier {
                Notifier::Bell => sink.terminal("\x07"),
                Notifier::Osc9 | Notifier::Osc777 => {
                    let sequence = if *notifier == Notifier::Osc9 {
                        format!("\x1b]9;{text}\x07")
                    } else {
                        format!("\x1b]777;notify;SystemScape;{text}\x07")
                    };
                    match self.place.osc(&sequence, visible) {
                        Some(bytes) => sink.terminal(&bytes),
                        None => dropped = true,
                    }
                }
                Notifier::TmuxMessage => self.display(text, sink),
                Notifier::TmuxOption => {}
            }
        }
        // tmux would have swallowed the OSC: say it in tmux instead, once.
        if dropped && !rule.notify.contains(&Notifier::TmuxMessage) {
            self.display(text, sink);
        }
    }

    fn display(&self, text: &str, sink: &mut dyn Sink) {
        if let Some(pane) = &self.place.pane {
            sink.tmux(&[
                "display-message".into(),
                "-t".into(),
                pane.clone(),
                text.into(),
            ]);
        }
    }

    /// Set the tmux option to the firing rules' names, or unset it when none
    /// fire. Runs tmux only when the value changes.
    pub(crate) fn sync_option(&mut self, value: String, sink: &mut dyn Sink) {
        let Some(pane) = &self.place.pane else { return };
        if value == self.shown {
            return;
        }
        let mut argv: Vec<String> = vec!["set-option".into(), "-w".into()];
        if value.is_empty() {
            argv.push("-u".into());
        }
        argv.extend(["-t".into(), pane.clone(), self.option.into()]);
        if !value.is_empty() {
            argv.push(value.clone());
        }
        sink.tmux(&argv);
        self.shown = value;
    }
}

// ---------------------------------------------------------------- runtime

/// One view's rules, their state, and what they show and send.
pub(crate) struct Alerts {
    book: Book,
    dispatch: Dispatch,
    sink: Box<dyn Sink>,
    /// Where the rules came from, for the rule list.
    origin: String,
    /// Rules in the file that the other view evaluates.
    others: usize,
    other_view: View,
    /// Host names, by strip position, and whether a hosts file is in use.
    hosts: Vec<String>,
    federated: bool,
    demo: bool,
    list: bool,
    clock: Instant,
    evaluated: Option<Instant>,
    /// The book's clock at the last evaluation: seconds since start for
    /// telemetry, Unix seconds for activity (records carry Unix times).
    now: f64,
}

/// What a view hands over to load its rules.
struct Load<'a> {
    view: View,
    explicit: Option<&'a Path>,
    demo: bool,
    hosts: &'a [(String, bool)],
    federated: bool,
}

impl Alerts {
    /// Telemetry's rules. `hosts` is each source's name and whether it is
    /// remote, in strip order; `federated` is whether a hosts file is in use.
    pub(crate) fn telemetry(
        explicit: Option<&Path>,
        demo: bool,
        hosts: &[(String, bool)],
        federated: bool,
    ) -> Result<Self, String> {
        Self::load(Load {
            view: View::Telemetry,
            explicit,
            demo,
            hosts,
            federated,
        })
    }

    /// Activity's rules.
    pub(crate) fn activity(explicit: Option<&Path>, demo: bool) -> Result<Self, String> {
        Self::load(Load {
            view: View::Activity,
            explicit,
            demo,
            hosts: &[],
            federated: false,
        })
    }

    fn load(load: Load<'_>) -> Result<Self, String> {
        let (rules, origin) = if load.demo {
            if load.explicit.is_some() {
                return Err(
                    "--demo uses built-in rules and cannot be combined with --alerts".into(),
                );
            }
            (demo_rules(), "built-in demo rules".to_string())
        } else {
            let path = match load.explicit {
                Some(path) => Some(path.to_path_buf()),
                None => default_path().filter(|p| p.exists()),
            };
            match path {
                None => (Vec::new(), "no rules file".to_string()),
                Some(path) => {
                    let text = std::fs::read_to_string(&path)
                        .map_err(|e| format!("cannot read alerts file {}: {e}", path.display()))?;
                    let rules =
                        parse(&text).map_err(|e| format!("alerts file {}: {e}", path.display()))?;
                    (rules, format!("rules from {}", path.display()))
                }
            }
        };
        let (mine, others): (Vec<Rule>, Vec<Rule>) = rules
            .into_iter()
            .partition(|r| r.trigger.view() == load.view);
        if load.view == View::Telemetry {
            bind(&mine, load.hosts, load.federated)
                .map_err(|e| format!("alerts, {origin}: {e}"))?;
        }
        let place = if mine.is_empty() {
            Place {
                pane: None,
                passthrough: Passthrough::Off,
            }
        } else {
            Place::detect()
        };
        Ok(Self::with(
            mine,
            others.len(),
            load.view,
            place,
            Box::new(LiveSink),
            origin,
            load.hosts.iter().map(|(n, _)| n.clone()).collect(),
            load.federated,
            load.demo,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn with(
        rules: Vec<Rule>,
        others: usize,
        view: View,
        place: Place,
        sink: Box<dyn Sink>,
        origin: String,
        hosts: Vec<String>,
        federated: bool,
        demo: bool,
    ) -> Self {
        let count = rules.len();
        Self {
            book: Book::new(rules),
            dispatch: Dispatch::new(place, view, count),
            sink,
            origin,
            others,
            other_view: match view {
                View::Telemetry => View::Activity,
                View::Activity => View::Telemetry,
            },
            hosts,
            federated,
            demo,
            list: false,
            clock: Instant::now(),
            evaluated: None,
            now: 0.0,
        }
    }

    /// Whether it is time to hand readings over: there are rules, and a
    /// second has passed since the last look.
    pub(crate) fn due(&self) -> bool {
        !self.book.rules.is_empty() && self.evaluated.is_none_or(|t| t.elapsed() >= EVERY)
    }

    /// Evaluate telemetry rules. `visible` is whether the view is on screen,
    /// which decides whether tmux passes an OSC through when set to `on`.
    /// Returns whether the banner may have changed.
    pub(crate) fn observe_telemetry(&mut self, mut hosts: Vec<HostReading>, visible: bool) -> bool {
        self.evaluated = Some(Instant::now());
        let now = self.clock.elapsed().as_secs_f64();
        if self.demo {
            if let Some(first) = hosts.first_mut() {
                first.readings.push(("DEMO".into(), Some(demo_signal(now))));
            }
        }
        let changes = self.book.telemetry(&hosts, now);
        self.now = now;
        self.deliver(changes, now, visible)
    }

    /// Evaluate activity rules against the current records. Called only
    /// while the view is on screen, just after its records were refreshed.
    pub(crate) fn observe_activity(&mut self, events: &[Event]) -> bool {
        self.evaluated = Some(Instant::now());
        let unix = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let changes = self.book.activity(events, unix);
        self.now = unix;
        self.deliver(changes, self.clock.elapsed().as_secs_f64(), true)
    }

    /// Notify each change; `now` is the monotonic clock rate limits run on.
    fn deliver(&mut self, changes: Vec<Change>, now: f64, visible: bool) -> bool {
        for change in &changes {
            let rule = &self.book.rules[change.rule];
            let firing = self.book.firing(change.rule).count();
            let host = match change.subject {
                Subject::Host(i) if self.federated => Some(i + 1),
                _ => None,
            };
            let text = message(rule, change, firing, host);
            self.dispatch
                .send(change.rule, rule, &text, now, visible, self.sink.as_mut());
        }
        let names: Vec<&str> = self
            .book
            .rules
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                r.notify.contains(&Notifier::TmuxOption) && self.book.firing(*i).next().is_some()
            })
            .map(|(_, r)| r.name.as_str())
            .collect();
        self.dispatch
            .sync_option(names.join(","), self.sink.as_mut());
        // A firing rule's value changes with every reading.
        !changes.is_empty() || self.book.any_firing()
    }

    /// The rule-list key.
    pub(crate) fn toggle_list(&mut self) {
        self.list = !self.list;
    }

    fn banner_due(&self, dims: (usize, usize)) -> bool {
        dims.1 >= 3 && self.book.any_firing()
    }

    /// The size the view should draw at: one row fewer while a banner shows.
    pub(crate) fn view_dims(&self, dims: (usize, usize)) -> (usize, usize) {
        if self.banner_due(dims) {
            (dims.0, dims.1 - 1)
        } else {
            dims
        }
    }

    /// Add the banner and, when asked for, the rule list to a drawn view.
    /// With no rule firing and the list closed, the rows come back untouched.
    pub(crate) fn dress(&self, mut rows: Vec<String>, dims: (usize, usize)) -> Vec<String> {
        if self.list && rows.len() >= 4 {
            let lines = self.list_lines();
            let height = (lines.len() + 2).min(rows.len() - 2);
            let mut canvas = PixelCanvas::new(dims.0, height, LIST_BG, LIST_BG);
            let mut shown = lines.into_iter().take(height.saturating_sub(1));
            for row in 0..height.saturating_sub(1) {
                if let Some((text, colour)) = shown.next() {
                    canvas.label(1, row as i64, &text, colour);
                }
            }
            canvas.label(1, height as i64 - 1, "! closes this list", MUTED);
            for (i, row) in canvas.rows().into_iter().enumerate() {
                rows[1 + i] = row;
            }
        }
        if self.banner_due(dims) {
            let mut canvas = PixelCanvas::new(dims.0, 1, BANNER_BG, BANNER_BG);
            canvas.label(0, 0, &self.banner_text(), BANNER_FG);
            rows.insert(0, canvas.rows().remove(0));
        }
        rows
    }

    /// One line naming every firing rule.
    fn banner_text(&self) -> String {
        let mut parts = Vec::new();
        for (index, rule) in self.book.rules.iter().enumerate() {
            let firing: Vec<(&Subject, &Track)> = self.book.firing(index).collect();
            let Some((subject, track)) = firing.first() else {
                continue;
            };
            let host = match subject {
                Subject::Host(i) if self.federated => self
                    .hosts
                    .get(*i)
                    .map(|n| format!("{} ", clean(n)))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            let many = firing.len() > 1;
            let detail = match &rule.trigger {
                Trigger::Channel { .. } if many => plural(firing.len(), "host"),
                Trigger::Channel {
                    label,
                    above,
                    level,
                    ..
                } => format!(
                    "{host}{label} {} {} {}",
                    num(track.value),
                    if *above { ">" } else { "<" },
                    num(*level)
                ),
                Trigger::LinkLost { .. } if many => {
                    format!("{} lost", plural(firing.len(), "link"))
                }
                Trigger::LinkLost { .. } => format!("{host}link lost"),
                Trigger::SessionError { .. } | Trigger::ErrorBurst { .. } => {
                    plural(firing.len(), "session")
                }
                Trigger::Quiet { .. } => format!("quiet {}", span(track.value)),
            };
            parts.push(format!("{} {detail}", rule.name));
        }
        format!(" ▲ ALERT  {}  · ! rules", parts.join(" · "))
    }

    /// The rule list: every rule, its state and its notifiers, then notes.
    fn list_lines(&self) -> Vec<(String, Rgb)> {
        let mut lines = vec![(
            format!(
                "ALERT RULES · {} · {} for this view",
                clean(&self.origin),
                self.book.rules.len()
            ),
            (63, 235, 255),
        )];
        if self.book.rules.is_empty() {
            lines.push((
                "No rules. Copy docs/alerts.example.toml to ~/.config/systemscape/alerts.toml \
                 or pass --alerts FILE."
                    .into(),
                MUTED,
            ));
        }
        let width = self
            .book
            .rules
            .iter()
            .map(|r| r.name.len())
            .max()
            .unwrap_or(0);
        for (index, rule) in self.book.rules.iter().enumerate() {
            let firing: Vec<&Track> = self.book.firing(index).map(|(_, t)| t).collect();
            let pending = self
                .book
                .tracks
                .iter()
                .filter_map(|((r, _), t)| match t.phase {
                    Phase::Pending { since } if *r == index => Some(since),
                    _ => None,
                })
                .reduce(f64::min);
            let (state, colour) = if let Some(first) = firing
                .iter()
                .filter_map(|t| match t.phase {
                    Phase::Firing { since } => Some(since),
                    _ => None,
                })
                .reduce(f64::min)
            {
                let count = if firing.len() > 1 {
                    format!(" ×{}", firing.len())
                } else {
                    String::new()
                };
                (format!("● firing{count} {}", span(self.now - first)), RED)
            } else if let Some(since) = pending {
                (format!("◐ pending {}", span(self.now - since)), AMBER)
            } else {
                ("○ ok".to_string(), GREEN)
            };
            let notify: Vec<&str> = rule.notify.iter().map(|n| n.name()).collect();
            let notify = if notify.is_empty() {
                "banner only".to_string()
            } else {
                notify.join(" ")
            };
            let held = match self.dispatch.held[index] {
                0 => String::new(),
                n => format!(" · {n} held back"),
            };
            lines.push((
                format!(
                    "{state:<18} {:<width$}  {} · {notify}{held}",
                    rule.name,
                    rule.trigger.describe()
                ),
                colour,
            ));
        }
        if self.others > 0 {
            lines.push((
                format!(
                    "{} in this rule set belong to the {} view",
                    plural(self.others, "more rule"),
                    self.other_view.name()
                ),
                MUTED,
            ));
        }
        if self.demo
            && self
                .book
                .rules
                .iter()
                .any(|r| r.trigger.view() == View::Telemetry)
        {
            lines.push((
                "demo: DEMO is a synthetic signal for the rules, not a ridge".into(),
                MUTED,
            ));
        }
        if !self.book.rules.is_empty() {
            for note in self.dispatch.place.notes() {
                lines.push((note.to_string(), MUTED));
            }
        }
        lines
    }
}

impl Drop for Alerts {
    /// Leave no stale alert in a tmux status line.
    fn drop(&mut self) {
        self.dispatch.sync_option(String::new(), self.sink.as_mut());
    }
}

/// The demo's synthetic reading: crosses 85 for about six seconds of every
/// 24, so a rule with a two-second hold fires and clears on a short cycle.
fn demo_signal(seconds: f64) -> f64 {
    70.0 + 22.0 * (seconds * std::f64::consts::TAU / 24.0).sin()
}

/// The rules `--demo` runs: a synthetic heat signal for telemetry, and rules
/// the activity fixture (a session that ended in repeated failures four
/// minutes ago) sets off. The real CPU rule shows a rule that stays ok.
fn demo_rules() -> Vec<Rule> {
    let rule = |name: &str, trigger, notify: &[Notifier]| Rule {
        name: name.into(),
        trigger,
        notify: notify.to_vec(),
        rate_limit: 30.0,
    };
    let tmux = [Notifier::TmuxOption, Notifier::TmuxMessage];
    vec![
        rule(
            "demo heat",
            Trigger::Channel {
                label: "DEMO".into(),
                host: None,
                above: true,
                level: 85.0,
                clear: 75.0,
                hold: 2.0,
            },
            &tmux,
        ),
        rule(
            "cpu hot",
            Trigger::Channel {
                label: "CPU°".into(),
                host: None,
                above: true,
                level: 95.0,
                clear: 85.0,
                hold: 60.0,
            },
            &[],
        ),
        rule(
            "demo failed",
            Trigger::SessionError {
                quiet: 60.0,
                within: 900.0,
            },
            &tmux,
        ),
        rule(
            "demo burst",
            Trigger::ErrorBurst {
                errors: 2,
                within: 900.0,
            },
            &[Notifier::TmuxOption],
        ),
        rule("demo quiet", Trigger::Quiet { after: 1800.0 }, &[]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// What a sink was asked to do.
    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Terminal(String),
        Tmux(Vec<String>),
    }

    /// Records calls; clones share one log, so a test keeps a handle after
    /// giving the sink away.
    #[derive(Clone, Default)]
    struct Recorder(Rc<RefCell<Vec<Call>>>);

    impl Recorder {
        fn take(&self) -> Vec<Call> {
            std::mem::take(&mut self.0.borrow_mut())
        }
    }

    impl Sink for Recorder {
        fn terminal(&mut self, bytes: &str) {
            self.0.borrow_mut().push(Call::Terminal(bytes.into()));
        }
        fn tmux(&mut self, argv: &[String]) {
            self.0.borrow_mut().push(Call::Tmux(argv.to_vec()));
        }
    }

    fn rule(text: &str) -> Result<Rule, String> {
        parse(&format!("[[rule]]\n{text}")).map(|mut rules| rules.remove(0))
    }

    fn channel_rule(extra: &str) -> Rule {
        rule(&format!(
            "name = \"hot\"\nwhen = \"channel\"\nchannel = \"CPU°\"\nabove = 85\nclear = 80\n{extra}"
        ))
        .unwrap()
    }

    fn host(name: &str, remote_live: Option<bool>, cpu: Option<f64>) -> HostReading {
        HostReading {
            name: name.into(),
            remote_live,
            readings: vec![("CPU°".into(), cpu), ("MEM".into(), Some(40.0))],
        }
    }

    fn tmux_place(passthrough: Passthrough) -> Place {
        Place {
            pane: Some("%3".into()),
            passthrough,
        }
    }

    fn outside() -> Place {
        Place {
            pane: None,
            passthrough: Passthrough::Off,
        }
    }

    fn alerts(rules: Vec<Rule>, view: View, place: Place, sink: &Recorder) -> Alerts {
        Alerts::with(
            rules,
            0,
            view,
            place,
            Box::new(sink.clone()),
            "test rules".into(),
            vec!["here".into(), "render-box".into()],
            true,
            false,
        )
    }

    fn event(session: &str, kind: &str, ago: f64, now: f64) -> Event {
        let at = chrono::DateTime::from_timestamp_millis(((now - ago) * 1000.0) as i64)
            .unwrap()
            .to_rfc3339();
        Event {
            key: format!("{session}:{ago}"),
            at,
            agent: "claude".into(),
            session: session.into(),
            project: "/workspace/p".into(),
            kind: kind.into(),
            text: "#{pane_id}; rm -rf / \x1b]9;pwned\x07".into(),
            source: "test".into(),
            parent: None,
            path: None,
        }
    }

    fn plain(rows: &[String]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                let (mut text, mut escape) = (String::new(), false);
                for c in row.chars() {
                    match (escape, c) {
                        (false, '\x1b') => escape = true,
                        (true, 'm') => escape = false,
                        (false, _) => text.push(c),
                        _ => {}
                    }
                }
                text
            })
            .collect()
    }

    /// The example file with its commented-out rules switched on: the rules
    /// that need a hosts file naming `render-box`.
    fn federated_example() -> String {
        let keys = [
            "name",
            "when",
            "channel",
            "host",
            "above",
            "clear",
            "for",
            "notify",
            "rate_limit",
        ];
        let setting = |rule: &str| {
            rule == "[[rule]]"
                || rule
                    .split_once(" = ")
                    .is_some_and(|(k, _)| keys.contains(&k))
        };
        include_str!("../docs/alerts.example.toml")
            .lines()
            .map(|line| match line.strip_prefix("# ") {
                Some(rule) if setting(rule) => rule,
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_example_file_works_as_copied_on_one_machine() {
        let rules = parse(include_str!("../docs/alerts.example.toml")).unwrap();
        let telemetry: Vec<Rule> = rules
            .into_iter()
            .filter(|r| r.trigger.view() == View::Telemetry)
            .collect();
        assert_eq!(bind(&telemetry, &[(String::new(), false)], false), Ok(()));
    }

    #[test]
    fn the_documented_example_file_parses_with_rules_for_both_views() {
        let rules = parse(&federated_example()).unwrap();
        assert!(rules.iter().any(|r| r.name == "host-lost"));
        assert!(rules.iter().any(|r| r.name == "render-box-power"));
        let views: Vec<View> = rules.iter().map(|r| r.trigger.view()).collect();
        assert!(views.contains(&View::Telemetry) && views.contains(&View::Activity));
        let kinds = [
            "channel",
            "link-lost",
            "session-error",
            "error-burst",
            "quiet",
        ];
        let text = include_str!("../docs/alerts.example.toml");
        for kind in kinds {
            assert!(
                text.contains(&format!("when = \"{kind}\"")),
                "{kind} undocumented"
            );
        }
        let hosts = [
            ("here".to_string(), false),
            ("render-box".to_string(), true),
        ];
        let telemetry: Vec<Rule> = rules
            .into_iter()
            .filter(|r| r.trigger.view() == View::Telemetry)
            .collect();
        assert_eq!(bind(&telemetry, &hosts, true), Ok(()));
    }

    #[test]
    fn anything_ambiguous_or_misspelt_is_refused_with_a_reason() {
        let base = "name = \"r\"\nwhen = \"channel\"\nchannel = \"CPU°\"\n";
        for (text, reason) in [
            ("name = \"r\"\nwhen = \"sometimes\"", "not a rule kind"),
            (
                "name = \"r\"\nwhen = \"quiet\"\nfor = 60\ncolour = \"red\"",
                "unknown field",
            ),
            (&format!("{base}above = 85"), "set clear"),
            (
                &format!("{base}above = 85\nbelow = 20\nclear = 80"),
                "not both",
            ),
            (&format!("{base}clear = 80"), "set above or below"),
            (
                &format!("{base}above = 85\nclear = 90"),
                "must be below above",
            ),
            (
                &format!("{base}below = 10\nclear = 5"),
                "must be above below",
            ),
            (
                &format!("{base}above = 85\nclear = 80\nfor = -1"),
                "between",
            ),
            (
                &format!("{base}above = 85\nclear = 80\nerrors = 3"),
                "does not apply",
            ),
            (
                &format!("{base}above = 85\nclear = 80\nnotify = [\"beep\"]"),
                "not one of",
            ),
            (
                &format!("{base}above = 85\nclear = 80\nnotify = [\"bell\", \"bell\"]"),
                "twice",
            ),
            (
                "name = \"r\"\nwhen = \"channel\"\nchannel = \"GPU0\"\nabove = 1\nclear = 0",
                "GPU0° (temperature) or GPU0%",
            ),
            (
                "name = \"r\"\nwhen = \"channel\"\nchannel = \"CPU\"\nabove = 1\nclear = 0",
                "CPU° is the temperature",
            ),
            ("name = \"r\"\nwhen = \"link-lost\"", "needs for"),
            (
                "name = \"r\"\nwhen = \"quiet\"\nhost = \"x\"\nfor = 5",
                "does not apply",
            ),
            (
                "name = \"r\"\nwhen = \"error-burst\"\nwithin = 60",
                "needs errors",
            ),
            (
                "name = \"r\"\nwhen = \"error-burst\"\nerrors = 0\nwithin = 60",
                "between 1",
            ),
            (
                "name = \"r\"\nwhen = \"session-error\"\nfor = 600\nwithin = 60",
                "longer than",
            ),
            (
                "name = \"#{pane}\"\nwhen = \"quiet\"\nfor = 5",
                "must be 1 to 24",
            ),
            (
                "name = \"a;b\"\nwhen = \"quiet\"\nfor = 5",
                "must be 1 to 24",
            ),
            ("name = \"\"\nwhen = \"quiet\"\nfor = 5", "must be 1 to 24"),
        ] {
            let error = rule(text).unwrap_err();
            assert!(
                error.contains(reason),
                "{text:?} gave {error:?}, wanted {reason:?}"
            );
        }
        let twice = "[[rule]]\nname = \"q\"\nwhen = \"quiet\"\nfor = 5\n".repeat(2);
        assert!(parse(&twice).unwrap_err().contains("used twice"));
        assert!(parse("").unwrap_err().contains("no [[rule]]"));
    }

    #[test]
    fn channels_are_named_as_the_view_labels_them() {
        for good in [
            "CPU°", "cpu°", "GPU0°", "GPU12%", "VRAM3", "PWR", "NET", "IO",
        ] {
            assert!(channel_label(good).is_ok(), "{good}");
        }
        assert_eq!(channel_label("vram1").unwrap(), "VRAM1");
        for bad in [
            "CPU", "GPU1", "GPU°", "VRAM", "VRAM01", "GPU123°", "TEMP", "DISK",
        ] {
            assert!(channel_label(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_names_and_links_are_bound_to_the_hosts_file() {
        let hosts = [("here".to_string(), false), ("box".to_string(), true)];
        let lost = |host: &str| {
            rule(&format!(
                "name = \"l\"\nwhen = \"link-lost\"\nfor = 5\n{host}"
            ))
            .unwrap()
        };
        assert_eq!(bind(&[lost("")], &hosts, true), Ok(()));
        assert_eq!(bind(&[lost("host = \"box\"")], &hosts, true), Ok(()));
        let refused = |rules: &[Rule], hosts: &[(String, bool)], federated, why: &str| {
            let error = bind(rules, hosts, federated).unwrap_err();
            assert!(error.contains(why), "{error}");
        };
        refused(&[lost("host = \"here\"")], &hosts, true, "is this machine");
        refused(
            &[lost("host = \"elsewhere\"")],
            &hosts,
            true,
            "not in the hosts file",
        );
        refused(
            &[lost("")],
            &[(String::new(), false)],
            false,
            "needs a hosts file",
        );
        refused(&[lost("")], &hosts[..1], true, "at least one remote");
        let pinned = channel_rule("host = \"box\"");
        refused(
            std::slice::from_ref(&pinned),
            &[(String::new(), false)],
            false,
            "needs a hosts file",
        );
        assert_eq!(bind(&[pinned], &hosts, true), Ok(()));
    }

    #[test]
    fn a_value_hovering_at_the_threshold_fires_once_and_clears_once() {
        let mut book = Book::new(vec![channel_rule("")]);
        let mut fired = 0;
        let mut cleared = 0;
        for (t, v) in [84.0, 86.0, 84.5, 85.5, 83.0, 86.0, 81.0, 79.5, 86.0]
            .into_iter()
            .enumerate()
        {
            for change in book.telemetry(&[host("", None, Some(v))], t as f64) {
                if change.fired {
                    fired += 1;
                } else {
                    cleared += 1;
                }
            }
        }
        // 86 fires; nothing above 80 clears; 79.5 clears; the last 86 fires again.
        assert_eq!((fired, cleared), (2, 1));
    }

    #[test]
    fn a_rule_fires_only_after_its_condition_has_held_for_its_duration() {
        let mut book = Book::new(vec![channel_rule("for = 60")]);
        let at = |book: &mut Book, t: f64, v: f64| book.telemetry(&[host("", None, Some(v))], t);
        assert!(at(&mut book, 0.0, 90.0).is_empty());
        assert!(at(&mut book, 59.0, 90.0).is_empty());
        let change = at(&mut book, 60.0, 91.0);
        assert_eq!(change.len(), 1);
        assert!(change[0].fired && change[0].value == 91.0);
        // A dip into the band restarts the hold.
        let mut book = Book::new(vec![channel_rule("for = 60")]);
        at(&mut book, 0.0, 90.0);
        at(&mut book, 30.0, 82.0);
        assert!(at(&mut book, 61.0, 90.0).is_empty());
        assert_eq!(at(&mut book, 121.0, 90.0).len(), 1);
    }

    #[test]
    fn missing_readings_neither_fire_nor_clear() {
        let hold = 10.0;
        let trip = Some(Obs {
            trip: true,
            clear: false,
            value: 90.0,
        });
        let mut phase = Phase::Ok;
        assert_eq!(advance(&mut phase, trip, hold, 0.0), None);
        assert_eq!(advance(&mut phase, None, hold, 5.0), None);
        assert_eq!(phase, Phase::Ok, "a gap restarts a pending hold");
        assert_eq!(advance(&mut phase, trip, hold, 6.0), None);
        assert_eq!(advance(&mut phase, trip, hold, 16.0), Some(true));
        assert_eq!(advance(&mut phase, None, hold, 100.0), None);
        assert!(
            matches!(phase, Phase::Firing { .. }),
            "a gap does not clear"
        );
    }

    #[test]
    fn each_host_has_its_own_state_and_a_pinned_rule_watches_only_its_host() {
        let mut book = Book::new(vec![channel_rule(""), channel_rule("host = \"box\"")]);
        book.rules[1].name = "box hot".into();
        let changes = book.telemetry(
            &[
                host("here", None, Some(90.0)),
                host("box", Some(true), Some(70.0)),
            ],
            0.0,
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(
            (changes[0].rule, &changes[0].subject),
            (0, &Subject::Host(0))
        );
        let changes = book.telemetry(
            &[
                host("here", None, Some(90.0)),
                host("box", Some(true), Some(95.0)),
            ],
            1.0,
        );
        let subjects: Vec<(usize, Subject)> = changes
            .iter()
            .map(|c| (c.rule, c.subject.clone()))
            .collect();
        assert_eq!(subjects, vec![(0, Subject::Host(1)), (1, Subject::Host(1))]);
    }

    #[test]
    fn a_dead_link_fires_after_its_time_even_while_reconnecting_and_clears_when_live() {
        let lost = rule("name = \"lost\"\nwhen = \"link-lost\"\nfor = 30").unwrap();
        let mut book = Book::new(vec![lost]);
        let at = |book: &mut Book, t: f64, live: bool| {
            book.telemetry(
                &[host("here", None, None), host("box", Some(live), None)],
                t,
            )
        };
        // Down, connecting, down again: none of it is live, so the clock runs on.
        assert!(at(&mut book, 0.0, false).is_empty());
        assert!(at(&mut book, 15.0, false).is_empty());
        let fired = at(&mut book, 30.0, false);
        assert_eq!(fired.len(), 1);
        assert_eq!(
            fired[0].subject,
            Subject::Host(1),
            "this machine is never lost"
        );
        let cleared = at(&mut book, 31.0, true);
        assert!(cleared.len() == 1 && !cleared[0].fired);
    }

    #[test]
    fn a_session_that_ended_on_an_error_fires_only_while_recent() {
        let now = 1_800_000_000.0;
        let failed = rule("name = \"failed\"\nwhen = \"session-error\"").unwrap();
        let mut book = Book::new(vec![failed]);
        let events = vec![
            event("a", "tool", 300.0, now),
            event("a", "failure", 240.0, now), // ended 4 minutes ago: fires
            event("old", "failure", 86_400.0, now), // a day ago: never fires
            event("b", "failure", 20.0, now),  // not quiet long enough yet
            event("c", "failure", 400.0, now),
            event("c", "tool", 100.0, now), // carried on after its error
        ];
        let changes = book.activity(&events, now);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].subject, Subject::Session("claude\u{1f}a".into()));
        // Session a records again: it clears.
        let mut more = events.clone();
        more.push(event("a", "prompt", 1.0, now));
        let changes = book.activity(&more, now);
        assert!(changes.len() == 1 && !changes[0].fired);
        assert!(book.tracks.is_empty(), "nothing is kept for quiet sessions");
    }

    #[test]
    fn an_error_burst_fires_above_its_count_and_clears_when_the_window_empties() {
        let now = 1_800_000_000.0;
        let burst =
            rule("name = \"burst\"\nwhen = \"error-burst\"\nerrors = 2\nwithin = 600").unwrap();
        let mut book = Book::new(vec![burst]);
        let mut events: Vec<Event> = [500.0, 400.0]
            .iter()
            .map(|ago| event("s", "failure", *ago, now))
            .collect();
        events.push(event("s", "failure", 5_000.0, now)); // outside the window
        assert!(
            book.activity(&events, now).is_empty(),
            "two is not more than two"
        );
        events.push(event("s", "failure", 10.0, now));
        let fired = book.activity(&events, now);
        assert!(fired.len() == 1 && fired[0].fired && fired[0].value == 3.0);
        // Two have aged out but one is still in the window: still firing.
        assert!(book.activity(&events, now + 300.0).is_empty());
        let cleared = book.activity(&events, now + 700.0);
        assert!(cleared.len() == 1 && !cleared[0].fired);
    }

    #[test]
    fn quiet_waits_for_activity_before_it_can_fire() {
        let now = 1_800_000_000.0;
        let quiet = rule("name = \"quiet\"\nwhen = \"quiet\"\nfor = 600").unwrap();
        let mut book = Book::new(vec![quiet]);
        let history = vec![event("s", "tool", 7200.0, now)];
        assert!(
            book.activity(&history, now).is_empty(),
            "opening the view on old history does not fire"
        );
        let mut events = history.clone();
        events.push(event("s", "tool", 10.0, now));
        assert!(book.activity(&events, now).is_empty());
        let fired = book.activity(&events, now + 600.0);
        assert!(fired.len() == 1 && fired[0].fired);
        events.push(event("s", "tool", 0.0, now + 650.0));
        let cleared = book.activity(&events, now + 650.0);
        assert!(cleared.len() == 1 && !cleared[0].fired);
    }

    #[test]
    fn notifications_carry_names_and_numbers_never_record_text() {
        let now = 1_800_000_000.0;
        let sink = Recorder::default();
        let failed = rule(
            "name = \"failed\"\nwhen = \"session-error\"\nnotify = [\"tmux-message\", \"osc777\"]",
        )
        .unwrap();
        let mut alerts = alerts(
            vec![failed],
            View::Activity,
            tmux_place(Passthrough::All),
            &sink,
        );
        let changes = alerts
            .book
            .activity(&[event("x", "failure", 120.0, now)], now);
        alerts.deliver(changes, 0.0, true);
        let calls = sink.take();
        assert_eq!(
            calls,
            vec![
                Call::Tmux(vec![
                    "display-message".into(),
                    "-t".into(),
                    "%3".into(),
                    "systemscape: failed fired: 1 session ended on an error".into()
                ]),
                Call::Terminal(tmux_wrap(
                    "\x1b]777;notify;SystemScape;systemscape: failed fired: 1 session ended on an error\x07"
                )),
            ]
        );
        // Every message any rule can produce is free of tmux formats, OSC
        // separators and controls.
        let rules = parse(&federated_example()).unwrap();
        for (index, rule) in rules.iter().enumerate() {
            for fired in [true, false] {
                let change = Change {
                    rule: index,
                    subject: Subject::Host(1),
                    fired,
                    value: 91.25,
                };
                let text = message(rule, &change, 2, Some(2));
                assert!(text.starts_with("systemscape: "));
                assert!(
                    !text.contains(['#', ';']) && !text.chars().any(char::is_control),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn osc_goes_direct_outside_tmux_and_wrapped_or_rerouted_inside() {
        let hot = channel_rule("notify = [\"bell\", \"osc9\"]\nrate_limit = 0");
        let text = "systemscape: hot fired: 90 above 85";
        let send = |place: Place, visible: bool| {
            let mut sink = Recorder::default();
            let mut dispatch = Dispatch::new(place, View::Telemetry, 1);
            dispatch.send(0, &hot, text, 0.0, visible, &mut sink);
            sink.take()
        };
        let osc = format!("\x1b]9;{text}\x07");
        let bell = Call::Terminal("\x07".into());
        assert_eq!(
            send(outside(), true),
            vec![bell.clone(), Call::Terminal(osc.clone())]
        );
        let wrapped = Call::Terminal(format!("\x1bPtmux;\x1b\x1b]9;{text}\x07\x1b\\"));
        assert_eq!(
            send(tmux_place(Passthrough::All), false),
            vec![bell.clone(), wrapped.clone()]
        );
        assert_eq!(
            send(tmux_place(Passthrough::On), true),
            vec![bell.clone(), wrapped]
        );
        let rerouted = Call::Tmux(vec![
            "display-message".into(),
            "-t".into(),
            "%3".into(),
            text.into(),
        ]);
        assert_eq!(
            send(tmux_place(Passthrough::On), false),
            vec![bell.clone(), rerouted.clone()],
            "tmux would drop it from a pane off screen"
        );
        assert_eq!(
            send(tmux_place(Passthrough::Off), true),
            vec![bell, rerouted]
        );
    }

    #[test]
    fn rate_limits_hold_back_interruptions_but_never_the_tmux_option() {
        let sink = Recorder::default();
        let hot = channel_rule("notify = [\"bell\", \"tmux-option\"]\nrate_limit = 60");
        let mut alerts = alerts(
            vec![hot],
            View::Telemetry,
            tmux_place(Passthrough::Off),
            &sink,
        );
        let run = |alerts: &mut Alerts, t: f64, v: f64| {
            let changes = alerts.book.telemetry(&[host("here", None, Some(v))], t);
            alerts.deliver(changes, t, true);
        };
        let set = |value: &str| {
            Call::Tmux(
                [
                    "set-option",
                    "-w",
                    "-t",
                    "%3",
                    "@systemscape_telemetry",
                    value,
                ]
                .map(String::from)
                .to_vec(),
            )
        };
        let unset = Call::Tmux(
            [
                "set-option",
                "-w",
                "-u",
                "-t",
                "%3",
                "@systemscape_telemetry",
            ]
            .map(String::from)
            .to_vec(),
        );
        let bell = Call::Terminal("\x07".into());
        run(&mut alerts, 0.0, 90.0);
        assert_eq!(sink.take(), vec![bell.clone(), set("hot")]);
        run(&mut alerts, 1.0, 90.0);
        assert_eq!(sink.take(), vec![], "nothing changed: tmux is not run");
        run(&mut alerts, 2.0, 70.0);
        assert_eq!(
            sink.take(),
            vec![unset.clone()],
            "the clear's bell is held back"
        );
        run(&mut alerts, 3.0, 90.0);
        assert_eq!(sink.take(), vec![set("hot")]);
        assert_eq!(alerts.dispatch.held[0], 2);
        run(&mut alerts, 70.0, 70.0);
        assert_eq!(sink.take(), vec![bell, unset]);
        run(&mut alerts, 71.0, 90.0);
        drop(alerts);
        let calls = sink.take();
        assert_eq!(
            calls.last(),
            Some(&Call::Tmux(
                [
                    "set-option",
                    "-w",
                    "-u",
                    "-t",
                    "%3",
                    "@systemscape_telemetry"
                ]
                .map(String::from)
                .to_vec()
            )),
            "leaving clears the status line"
        );
    }

    #[test]
    fn outside_tmux_the_tmux_notifiers_do_nothing() {
        let sink = Recorder::default();
        let hot = channel_rule("notify = [\"tmux-message\", \"tmux-option\"]");
        let mut alerts = alerts(vec![hot], View::Telemetry, outside(), &sink);
        let changes = alerts
            .book
            .telemetry(&[host("here", None, Some(90.0))], 0.0);
        alerts.deliver(changes, 0.0, true);
        drop(alerts);
        assert_eq!(sink.take(), vec![]);
    }

    #[test]
    fn with_nothing_firing_the_view_is_drawn_untouched() {
        let sink = Recorder::default();
        let alerts = alerts(vec![channel_rule("")], View::Telemetry, outside(), &sink);
        let rows = vec!["a".to_string(), "b".to_string()];
        assert_eq!(alerts.view_dims((80, 24)), (80, 24));
        assert_eq!(alerts.dress(rows.clone(), (80, 2)), rows);
    }

    #[test]
    fn the_banner_and_rule_list_fill_every_pane_size_exactly() {
        let sink = Recorder::default();
        let rules = parse(&federated_example())
            .unwrap()
            .into_iter()
            .filter(|r| r.trigger.view() == View::Telemetry)
            .collect();
        let mut alerts = alerts(rules, View::Telemetry, tmux_place(Passthrough::Off), &sink);
        let hosts = [
            host("here", None, Some(97.0)),
            host("render-box", Some(false), Some(91.0)),
        ];
        for t in [0.0, 60.0] {
            let changes = alerts.book.telemetry(&hosts, t);
            alerts.deliver(changes, t, true);
        }
        for list in [false, true] {
            alerts.list = list;
            for dims in [
                (1, 1),
                (8, 3),
                (20, 8),
                (47, 27),
                (80, 24),
                (120, 40),
                (200, 65),
            ] {
                let view = alerts.view_dims(dims);
                let rows = vec![" ".repeat(view.0); view.1];
                let dressed = alerts.dress(rows, dims);
                assert_eq!(dressed.len(), dims.1, "{dims:?}");
                for line in plain(&dressed) {
                    let width: usize = line.chars().map(crate::pixels::char_width).sum();
                    assert_eq!(width, dims.0, "{dims:?} list={list}: {line:?}");
                }
                let text = plain(&dressed).join("\n");
                if dims.1 >= 3 {
                    assert!(text.contains("ALERT"), "{dims:?}");
                }
                if dims == (200, 65) {
                    assert!(text.contains("cpu-hot 2 hosts"), "{text}");
                    assert!(text.contains("host-lost render-box link lost"), "{text}");
                    if list {
                        for name in ["cpu-hot", "vram-full", "render-box-power", "host-lost"] {
                            assert!(text.contains(name), "{name} missing from the list");
                        }
                        assert!(text.contains("● firing ×2"));
                        assert!(text.contains("allow-passthrough is off"));
                    }
                }
            }
        }
    }

    #[test]
    fn the_demo_fires_and_clears_on_its_own_and_the_fixture_sets_off_activity_rules() {
        let sink = Recorder::default();
        let rules: Vec<Rule> = demo_rules()
            .into_iter()
            .filter(|r| r.trigger.view() == View::Telemetry)
            .collect();
        let mut book = Book::new(rules);
        let (mut fired_at, mut cleared_at) = (None, None);
        for second in 0..30 {
            let t = second as f64;
            let mut first = host("", None, Some(50.0));
            first.readings.push(("DEMO".into(), Some(demo_signal(t))));
            for change in book.telemetry(&[first], t) {
                if change.fired {
                    fired_at.get_or_insert(t);
                } else {
                    cleared_at.get_or_insert(t);
                }
            }
        }
        let (fired, cleared) = (fired_at.unwrap(), cleared_at.unwrap());
        assert!(
            fired <= 8.0 && cleared > fired && cleared <= 20.0,
            "{fired} {cleared}"
        );
        let mut alerts = alerts(
            demo_rules()
                .into_iter()
                .filter(|r| r.trigger.view() == View::Activity)
                .collect(),
            View::Activity,
            outside(),
            &sink,
        );
        alerts.observe_activity(&crate::activity_data::demo_events());
        let banner = alerts.banner_text();
        assert!(banner.contains("demo failed 1 session"), "{banner}");
        assert!(banner.contains("demo burst"), "{banner}");
        assert!(!banner.contains("demo quiet"), "{banner}");
        let firing: Vec<String> = alerts
            .list_lines()
            .into_iter()
            .map(|(line, _)| line)
            .filter(|line| line.starts_with("● firing"))
            .collect();
        assert_eq!(firing.len(), 2);
        assert!(
            firing.iter().all(|line| line.starts_with("● firing 0 s")),
            "times are measured on the clock the records use: {firing:?}"
        );
    }
}
