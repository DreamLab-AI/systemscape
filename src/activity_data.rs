//! Bounded, read-only local activity history. Records are evidence of logged activity,
//! not an assertion that an agent is live or that a task succeeded.
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};
use walkdir::WalkDir;

const READ_LIMIT: usize = 1024 * 1024;
const FILE_LIMIT: usize = 256;
const EVENT_LIMIT: usize = 5000;
const WALK_LIMIT: usize = 32768;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Event {
    pub key: String,
    pub at: String,
    pub agent: String,
    pub session: String,
    pub project: String,
    pub kind: String,
    pub text: String,
    pub source: String,
}

/// Strip terminal controls, directional formatting and invisible text carriers.
pub fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c,
        '\u{00ad}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' |
        '\u{2060}'..='\u{206f}' | '\u{feff}' | '\u{e0000}'..='\u{e007f}')
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}
fn string(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
// Archive identities are production u32 values; transcripts also use string IDs.
fn identifier(v: &Value) -> Option<String> {
    v.as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| v.as_u64().map(|n| n.to_string()))
}
fn fallback<'a>(a: &'a str, b: &'a str) -> &'a str {
    if a.is_empty() {
        b
    } else {
        a
    }
}
fn stamp(v: &Value) -> Option<String> {
    let dt = if let Some(s) = v.as_str() {
        DateTime::parse_from_rfc3339(s).ok()?.with_timezone(&Utc)
    } else {
        let n = v.as_f64()?;
        if !n.is_finite() {
            return None;
        }
        let ms = if n.abs() > 1e11 { n } else { n * 1000.0 };
        if ms < i64::MIN as f64 || ms >= i64::MAX as f64 {
            return None;
        }
        DateTime::from_timestamp_millis(ms as i64)?
    };
    Some(dt.to_rfc3339_opts(SecondsFormat::Millis, true))
}
fn digest(s: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}
#[derive(Clone)]
struct Source {
    path: PathBuf,
    family: &'static str,
    profile: String,
    modified: SystemTime,
}
#[derive(Default)]
struct Context {
    session: String,
    project: String,
}
struct State {
    identity: (u64, u64),
    offset: u64,
    checkpoint: Vec<u8>,
    prefix: Vec<u8>,
    context: Context,
    drop_line: bool,
    pending: bool,
}
#[cfg(unix)]
fn identity(m: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}
#[cfg(not(unix))]
fn identity(m: &fs::Metadata) -> (u64, u64) {
    (
        m.created()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64),
        0,
    )
}
fn normalise(row: &Value, source: &Source, ctx: &mut Context) -> Vec<Event> {
    if !row.is_object() {
        return vec![];
    }
    let payload = &row["payload"];
    if source.family == "codex" && row["type"] == "session_meta" {
        ctx.session = string(&payload["id"]).to_owned();
        ctx.project = string(&payload["cwd"]).to_owned();
    }
    if let Some(cwd) = row["cwd"].as_str() {
        ctx.project = cwd.to_owned();
    }
    let Some(at) = stamp(&row["timestamp"]) else {
        return vec![];
    };
    let stem = source
        .path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let mut session = fallback(string(&row["sessionId"]), fallback(&ctx.session, &stem)).to_owned();
    let mut project = fallback(&ctx.project, "?").to_owned();
    let mut agent = format!(
        "{}{}",
        source.family,
        if source.profile.is_empty() {
            String::new()
        } else {
            format!(":{}", source.profile)
        }
    );
    let id = fallback(string(&row["uuid"]), &digest(&row.to_string())).to_owned();
    let mut found: Vec<(String, &str, String)> = vec![];
    match source.family {
        "archive" => {
            let meta = &row["metadata"];
            let actor = identifier(&row["source_agent_id"]);
            agent = format!("archive:{}", actor.as_deref().unwrap_or("unknown"));
            session = identifier(&row["handoff_id"])
                .or_else(|| identifier(&meta["session_id"]))
                .unwrap_or_else(|| {
                    if actor.is_some() {
                        agent.clone()
                    } else {
                        // Missing identities must not merge unrelated archive files into one session.
                        format!("unattributed:{}", digest(&source.path.to_string_lossy()))
                    }
                });
            project = fallback(
                string(&meta["project"]),
                fallback(string(&meta["cwd"]), "shared events"),
            )
            .to_owned();
            let failure = !string(&row["failure_mode"]).is_empty()
                || row["outcome"] == "failure"
                || meta["outcome"] == "failure";
            let text = fallback(
                string(&meta["action"]),
                fallback(
                    string(&meta["summary"]),
                    fallback(
                        string(&meta["kind"]),
                        fallback(string(&row["type"]), "recorded event"),
                    ),
                ),
            );
            // Include outcome/time, so a failure does not hide behind a prior step ID.
            found.push((
                format!("{}:{}:{}", string(&meta["step_id"]), at, id),
                if failure { "failure" } else { "event" },
                text.to_owned(),
            ));
        }
        "claude" => {
            let content = &row["message"]["content"];
            let blocks = content.as_array().map(Vec::as_slice).unwrap_or(&[]);
            let texts: Vec<&str> = if let Some(s) = content.as_str() {
                vec![s]
            } else {
                blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect()
            };
            if row["type"] == "user"
                && row["isMeta"] != true
                && !texts.is_empty()
                && !blocks.iter().any(|b| b["type"] == "tool_result")
            {
                found.push((
                    format!("prompt:{}", fallback(string(&row["promptId"]), &id)),
                    "prompt",
                    texts.join(" "),
                ));
            }
            for (i, b) in blocks.iter().enumerate() {
                if b["type"] == "tool_use" {
                    let key = fallback(string(&b["id"]), &format!("{id}:{i}")).to_owned();
                    found.push((
                        key,
                        "tool",
                        format!(
                            "{} {}",
                            fallback(string(&b["name"]), "tool"),
                            fallback(
                                string(&b["input"]["file_path"]),
                                string(&b["input"]["description"])
                            )
                        ),
                    ));
                } else if b["type"] == "tool_result" && b["is_error"] == true {
                    found.push((
                        format!(
                            "{}:error",
                            fallback(string(&b["tool_use_id"]), &format!("{id}:{i}"))
                        ),
                        "failure",
                        "Tool reported an error".into(),
                    ));
                }
            }
            if let Some(sha) = row["toolUseResult"]["gitOperation"]["commit"]["sha"].as_str() {
                found.push((
                    format!("commit:{sha}"),
                    "receipt",
                    format!("Recorded commit {sha}"),
                ));
            }
        }
        "codex" => match (string(&row["type"]), string(&payload["type"])) {
            ("event_msg", "user_message") => {
                if let Some(s) = payload["message"].as_str() {
                    found.push((id, "prompt", s.to_owned()));
                }
            }
            ("event_msg", kind @ ("task_complete" | "task_started" | "turn_aborted")) => found
                .push((
                    id,
                    if kind == "turn_aborted" {
                        "failure"
                    } else {
                        "event"
                    },
                    format!("Recorded {}", kind.replace('_', " ")),
                )),
            ("response_item", "function_call" | "custom_tool_call") => found.push((
                fallback(string(&payload["call_id"]), &id).to_owned(),
                "tool",
                fallback(string(&payload["name"]), "tool").to_owned(),
            )),
            _ => {}
        },
        _ => {}
    }
    found
        .into_iter()
        .map(|(key, kind, text)| Event {
            key: format!("{}:{}:{}:{}", source.family, source.profile, session, key),
            at: at.clone(),
            agent: clean(&agent),
            session: clean(&session),
            project: clean(&project),
            kind: kind.into(),
            text: clean(&text),
            source: clean(&source.path.to_string_lossy()),
        })
        .collect()
}

/// Incremental local collector; no network access or model calls.
pub struct Collector {
    home: PathBuf,
    workspace: PathBuf,
    archive: PathBuf,
    files: Vec<Source>,
    states: HashMap<PathBuf, State>,
    events: HashMap<String, Event>,
    environment_roots: Vec<(&'static str, PathBuf)>,
    next_scan: Instant,
    cursor: usize,
    omitted: usize,
    errors: u64,
    limited: bool,
}
impl Collector {
    pub fn new(home: PathBuf, workspace: PathBuf, archive: PathBuf) -> Self {
        Self {
            home,
            workspace,
            archive,
            files: vec![],
            states: HashMap::new(),
            events: HashMap::new(),
            environment_roots: vec![],
            next_scan: Instant::now(),
            cursor: 0,
            omitted: 0,
            errors: 0,
            limited: false,
        }
    }
    /// Include explicit CLI profile environment roots in addition to the supplied home.
    pub fn with_env_roots(mut self) -> Self {
        self.environment_roots = [("CLAUDE_CONFIG_DIR", "claude"), ("CODEX_HOME", "codex")]
            .into_iter()
            .filter_map(|(var, family)| std::env::var_os(var).map(|p| (family, PathBuf::from(p))))
            .collect();
        self
    }
    fn discover(&mut self) {
        let mut roots = vec![
            ("claude", String::new(), self.home.join(".claude/projects")),
            ("codex", String::new(), self.home.join(".codex/sessions")),
        ];
        for (family, root) in &self.environment_roots {
            roots.push((
                *family,
                String::new(),
                root.join(if *family == "claude" {
                    "projects"
                } else {
                    "sessions"
                }),
            ));
        }
        if let Ok(profiles) = fs::read_dir(self.workspace.join("profiles")) {
            for profile in profiles.take(256).flatten() {
                let name = profile.file_name().to_string_lossy().into_owned();
                for (family, sub) in [("claude", ".claude/projects"), ("codex", ".codex/sessions")]
                {
                    roots.push((family, name.clone(), profile.path().join(sub)));
                }
            }
        }
        roots.push(("archive", String::new(), self.archive.clone()));
        let mut seen = HashSet::new();
        let mut candidates = vec![];
        let mut walked = 0;
        for (family, profile, root) in roots {
            if !root.exists() {
                continue;
            }
            // Resolve root symlinks (profile configs often are links), never recurse through child links.
            let root = fs::canonicalize(&root).unwrap_or(root);
            for entry in WalkDir::new(root)
                .max_depth(if family == "archive" { 1 } else { 12 })
                .follow_links(false)
            {
                walked += 1;
                if walked > WALK_LIMIT {
                    self.limited = true;
                    break;
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => {
                        self.errors += 1;
                        continue;
                    }
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy();
                let accepted = if family == "archive" {
                    name == "events.jsonl"
                        || name
                            .strip_prefix("events.jsonl.")
                            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                } else if family == "claude" {
                    name.ends_with(".jsonl")
                        && (entry.depth() == 2
                            || (entry.depth() == 4
                                && entry
                                    .path()
                                    .parent()
                                    .and_then(|p| p.file_name())
                                    .is_some_and(|p| p == "subagents")))
                } else {
                    name.ends_with(".jsonl")
                };
                if !accepted {
                    continue;
                }
                let path = entry.path().to_path_buf();
                if !seen.insert(path.clone()) {
                    continue;
                }
                match entry.metadata() {
                    Ok(m) => candidates.push(Source {
                        path,
                        family,
                        profile: profile.clone(),
                        modified: m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    }),
                    Err(_) => self.errors += 1,
                }
            }
            if walked > WALK_LIMIT {
                break;
            }
        }
        candidates.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.path.cmp(&b.path)));
        self.omitted = candidates.len().saturating_sub(FILE_LIMIT);
        candidates.truncate(FILE_LIMIT);
        let live: HashSet<_> = candidates.iter().map(|s| &s.path).collect();
        self.states.retain(|p, _| live.contains(p));
        self.files = candidates;
        self.next_scan = Instant::now() + Duration::from_secs(30);
    }
    pub fn poll(&mut self) {
        if Instant::now() >= self.next_scan {
            self.discover();
        }
        let mut budget = 8 * READ_LIMIT;
        let count = self.files.len();
        for _ in 0..count {
            if budget < READ_LIMIT {
                break;
            }
            let source = self.files[self.cursor % count].clone();
            self.cursor = (self.cursor + 1) % count;
            match self.read_source(&source) {
                Ok(used) => budget -= used,
                Err(_) => {
                    budget -= READ_LIMIT;
                    self.errors += 1;
                }
            }
        }
        if self.events.len() > EVENT_LIMIT {
            let mut keys: Vec<_> = self
                .events
                .values()
                .map(|e| (e.at.clone(), e.key.clone()))
                .collect();
            keys.sort();
            for (_, key) in keys.iter().take(keys.len() - EVENT_LIMIT) {
                self.events.remove(key);
            }
            self.limited = true;
        }
    }
    fn read_source(&mut self, source: &Source) -> std::io::Result<usize> {
        let mut file = File::open(&source.path)?;
        let meta = file.metadata()?;
        let mut used = 0;
        let mut reset = self
            .states
            .get(&source.path)
            .is_none_or(|s| s.identity != identity(&meta) || meta.len() < s.offset);
        if let Some(state) = self.states.get(&source.path).filter(|_| !reset) {
            file.seek(SeekFrom::Start(0))?;
            let mut prefix = vec![0; state.prefix.len()];
            file.read_exact(&mut prefix)?;
            used += prefix.len();
            reset = prefix != state.prefix;
            if !state.checkpoint.is_empty() {
                file.seek(SeekFrom::Start(
                    state.offset - state.checkpoint.len() as u64,
                ))?;
                let mut check = vec![0; state.checkpoint.len()];
                file.read_exact(&mut check)?;
                used += check.len();
                reset |= check != state.checkpoint;
            }
        }
        if reset {
            let mut context = Context::default();
            file.seek(SeekFrom::Start(0))?;
            let mut prefix = vec![0; (meta.len() as usize).min(128)];
            file.read_exact(&mut prefix)?;
            used += prefix.len();
            // Recover Codex session metadata even when only the recent tail is loaded.
            if source.family == "codex" {
                file.seek(SeekFrom::Start(0))?;
                let mut head = vec![0; (meta.len() as usize).min(65536)];
                let n = file.read(&mut head)?;
                used += n;
                if let Some(end) = head[..n].iter().position(|b| *b == b'\n') {
                    if let Ok(row) = serde_json::from_slice(&head[..end]) {
                        normalise(&row, source, &mut context);
                    }
                }
            }
            let offset = meta.len().saturating_sub((READ_LIMIT - used - 64) as u64);
            self.limited |= offset > 0;
            self.states.insert(
                source.path.clone(),
                State {
                    identity: identity(&meta),
                    offset,
                    checkpoint: vec![],
                    prefix,
                    context,
                    drop_line: offset > 0,
                    pending: true,
                },
            );
        }
        let state = self
            .states
            .get_mut(&source.path)
            .expect("state initialized");
        file.seek(SeekFrom::Start(state.offset))?;
        let capacity = READ_LIMIT - used - 64;
        let mut raw = vec![0; capacity.min(meta.len().saturating_sub(state.offset) as usize)];
        let n = file.read(&mut raw)?;
        raw.truncate(n);
        used += n;
        let end = raw.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if end > 0 {
            for line in raw[..end].split_inclusive(|b| *b == b'\n') {
                if state.drop_line {
                    state.drop_line = false;
                    continue;
                }
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match serde_json::from_slice::<Value>(line) {
                    Ok(row) if row.is_object() => {
                        for event in normalise(&row, source, &mut state.context) {
                            self.events.entry(event.key.clone()).or_insert(event);
                        }
                    }
                    _ => self.errors += 1,
                }
            }
            state.offset += end as u64;
        } else if n == capacity {
            state.offset += n as u64;
            state.drop_line = true;
            self.limited = true;
        }
        state.pending = state.offset < meta.len();
        let checkpoint_len = (state.offset as usize).min(64);
        state.checkpoint.resize(checkpoint_len, 0);
        file.seek(SeekFrom::Start(state.offset - checkpoint_len as u64))?;
        file.read_exact(&mut state.checkpoint)?;
        used += checkpoint_len;
        Ok(used)
    }
    /// Newest first, with deterministic tie ordering.
    pub fn rows(&self) -> Vec<Event> {
        let mut rows: Vec<_> = self.events.values().cloned().collect();
        rows.sort_by(|a, b| b.at.cmp(&a.at).then(a.key.cmp(&b.key)));
        rows
    }
    pub fn coverage(&self) -> String {
        let families: BTreeSet<_> = self.files.iter().map(|s| s.family).collect();
        let pending = self
            .files
            .iter()
            .filter(|source| self.states.get(&source.path).is_none_or(|s| s.pending))
            .count();
        format!(
            "Sources: {} | {} files | {} records | {} files pending | {} files omitted | {} read/parse errors{}",
            if families.is_empty() {
                "none found".to_owned()
            } else {
                families.into_iter().collect::<Vec<_>>().join(", ")
            },
            self.files.len(),
            self.events.len(),
            pending,
            self.omitted,
            self.errors,
            if self.limited {
                " | history limited"
            } else {
                ""
            }
        )
    }
}

/// Synthetic fixtures for a display demo; never mixed with observed records.
pub fn demo_events() -> Vec<Event> {
    let now = Utc::now();
    (0..24)
        .map(|i| Event {
            key: format!("demo:{i}"),
            at: (now - chrono::Duration::minutes(i * 7))
                .to_rfc3339_opts(SecondsFormat::Millis, true),
            agent: ["claude:builder", "codex:reviewer", "archive:tester"][(i % 3) as usize].into(),
            session: format!("demo-session-{}", i % 4),
            project: ["/workspace/agentbox", "/workspace/systemscape"][(i % 2) as usize].into(),
            kind: ["prompt", "tool", "event", "failure", "receipt"][(i % 5) as usize].into(),
            text: [
                "Inspect agent activity history",
                "Read renderer source",
                "Recorded task started",
                "Tool reported an error",
                "Recorded commit (demo)",
            ][(i % 5) as usize]
                .into(),
            source: "synthetic demo".into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "systemscape-collector-{}-{}",
                std::process::id(),
                ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn file(&self, name: &str, data: &str) -> PathBuf {
            let p = self.0.join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, data).unwrap();
            p
        }
        fn collector(&self) -> Collector {
            let mut c = Collector::new(self.0.clone(), self.0.clone(), self.0.join("archive"));
            c.environment_roots.clear();
            c
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn prompt(id: &str) -> String {
        format!("{{\"timestamp\":\"2026-09-12T12:00:00+02:00\",\"type\":\"user\",\"uuid\":\"{id}\",\"sessionId\":\"s\",\"message\":{{\"content\":\"hello\"}}}}\n")
    }
    #[test]
    fn append_partial_replay_and_utc() {
        let f = Fixture::new();
        let a = prompt("a");
        let b = prompt("b");
        let p = f.file(".claude/projects/p/s.jsonl", &format!("{a}{}", &b[..40]));
        let mut c = f.collector();
        c.poll();
        assert_eq!(c.rows().len(), 1);
        assert_eq!(c.rows()[0].at, "2026-09-12T10:00:00.000Z");
        let mut file = fs::OpenOptions::new().append(true).open(p).unwrap();
        file.write_all(&b.as_bytes()[40..]).unwrap();
        file.write_all(a.as_bytes()).unwrap();
        c.poll();
        c.poll();
        assert_eq!(c.rows().len(), 2);
    }
    #[test]
    fn rotation_and_same_inode_rewrite() {
        let f = Fixture::new();
        let p = f.file(".claude/projects/p/s.jsonl", &prompt("a"));
        let mut c = f.collector();
        c.poll();
        fs::write(&p, prompt("b")).unwrap();
        c.poll();
        assert_eq!(c.rows().len(), 2);
        let moved = p.with_extension("old");
        fs::rename(&p, moved).unwrap();
        fs::write(&p, prompt("c")).unwrap();
        c.poll();
        assert_eq!(c.rows().len(), 3);
    }
    #[test]
    fn profile_discovery_metadata_and_bad_types() {
        let f = Fixture::new();
        f.file("profiles/test/.codex/sessions/2026/09/a.jsonl", concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-1\",\"cwd\":\"/project\"}}\n",
            "[]\n{broken}\n{\"timestamp\":{},\"payload\":false}\n",
            "{\"timestamp\":\"2026-09-12T10:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"build\"}}\n"));
        let mut c = f.collector();
        c.poll();
        let rows = c.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session, "session-1");
        assert_eq!(rows[0].project, "/project");
        assert_eq!(rows[0].agent, "codex:test");
        assert!(c.coverage().contains("2 read/parse errors"));
    }
    #[test]
    fn archive_replay_and_non_jsonl_exclusion() {
        let f = Fixture::new();
        let row = "{\"timestamp\":1789207200000,\"source_agent_id\":\"a\",\"metadata\":{\"step_id\":\"step\",\"action\":\"test\"},\"outcome\":\"failure\"}\n";
        f.file("archive/events.jsonl", row);
        f.file("archive/events.jsonl.1", row);
        f.file("archive/events.jsonl.gz", "invalid");
        let mut c = f.collector();
        c.poll();
        assert_eq!(c.rows().len(), 1);
        assert_eq!(c.rows()[0].kind, "failure");
        assert_eq!(c.files.len(), 2);
    }
    #[test]
    fn production_numeric_archive_identities_remain_distinct() {
        let f = Fixture::new();
        let data = [7u32, 42, u32::MAX]
            .into_iter()
            .map(|id| {
                serde_json::json!({
                    "timestamp": "2026-09-12T10:00:00Z", "source_agent_id": id,
                    "metadata": {"action":"execute", "outcome":"success"}
                })
                .to_string()
                    + "\n"
            })
            .collect::<String>();
        f.file("archive/events.jsonl", &data);
        let mut c = f.collector();
        c.poll();
        let rows = c.rows();
        assert_eq!(rows.len(), 3);
        let agents: BTreeSet<_> = rows.iter().map(|r| r.agent.as_str()).collect();
        assert_eq!(
            agents,
            BTreeSet::from(["archive:7", "archive:42", "archive:4294967295"])
        );
        assert_eq!(
            rows.iter()
                .map(|r| &r.session)
                .collect::<BTreeSet<_>>()
                .len(),
            3
        );
        let source = c.files[0].clone();
        let mut ctx = Context::default();
        let handoff = normalise(
            &serde_json::json!({"timestamp":0,"source_agent_id":7,"handoff_id":"handoff-a"}),
            &source,
            &mut ctx,
        );
        assert_eq!(handoff[0].session, "handoff-a");
        let unknown = normalise(
            &serde_json::json!({"timestamp":0,"source_agent_id":false}),
            &source,
            &mut ctx,
        );
        assert_eq!(unknown[0].agent, "archive:unknown");
        assert!(unknown[0].session.starts_with("unattributed:"));
    }
    #[test]
    fn cold_scan_discloses_pending_files_and_drains_on_later_polls() {
        let f = Fixture::new();
        let line = serde_json::json!({"ignored": "x".repeat(1100)}).to_string() + "\n";
        let data = line.repeat(1000);
        for i in 0..12 {
            f.file(&format!(".claude/projects/p/{i}.jsonl"), &data);
        }
        let mut c = f.collector();
        c.poll();
        assert!(c.states.len() < 12);
        assert!(!c.coverage().contains("0 files pending"));
        c.poll();
        c.poll();
        assert!(c.coverage().contains("0 files pending"));
    }
    #[test]
    fn bounded_cold_tail_preserves_codex_metadata() {
        let f = Fixture::new();
        let head = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"original-session\",\"cwd\":\"/original\"}}\n";
        let tail = "{\"timestamp\":\"2026-09-12T10:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"recent prompt\"}}\n";
        f.file(
            ".codex/sessions/2026/session.jsonl",
            &format!("{head}{}\n{tail}", "x".repeat(READ_LIMIT + 1024)),
        );
        let mut c = f.collector();
        c.discover();
        let source = c.files[0].clone();
        assert!(c.read_source(&source).unwrap() <= READ_LIMIT);
        assert_eq!(c.rows().len(), 1);
        assert_eq!(c.rows()[0].session, "original-session");
        assert_eq!(c.rows()[0].project, "/original");
        assert!(c.coverage().contains("history limited"));
    }
    #[test]
    fn claude_discovers_subagents_but_excludes_arbitrary_logs() {
        let f = Fixture::new();
        f.file(".claude/projects/p/s/subagents/agent.jsonl", &prompt("a"));
        f.file(".claude/projects/p/debug/other.jsonl", &prompt("b"));
        let mut c = f.collector();
        c.poll();
        assert_eq!(c.rows().len(), 1);
    }
    #[test]
    fn controls_and_tool_results_are_not_prompts() {
        assert_eq!(clean("a\u{1b}\u{202e}\u{e0001}b"), "ab");
        let f = Fixture::new();
        f.file(".claude/projects/p/s.jsonl", "{\"timestamp\":\"2026-09-12T10:00:00Z\",\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"result\"},{\"type\":\"tool_result\",\"is_error\":true,\"tool_use_id\":\"x\"}]}}\n");
        let mut c = f.collector();
        c.poll();
        assert_eq!(c.rows().len(), 1);
        assert_eq!(c.rows()[0].kind, "failure");
    }
}
