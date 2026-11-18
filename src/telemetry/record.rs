//! The recorded telemetry history: the one file SystemScape writes, and only
//! with `--record`.
//!
//! Each closed 150 s slot becomes one small versioned JSON line in
//! `$XDG_STATE_HOME/systemscape/telemetry.jsonl` (else
//! `~/.local/state/systemscape/telemetry.jsonl`). Slots are aligned to the
//! wall clock, not to the moment a view started, so a probe and a view, or two
//! runs on either side of a restart, describe the same slot by the same time.
//!
//! The file is never opened for writing in place. Every write reads the file
//! (bounded), drops rows older than `KEEP_DAYS`, writes the result to a fresh
//! owner-only temporary file beside it and renames that over the final path.
//! A rename replaces whatever is at the final path rather than following it,
//! and a symbolic link found there is refused outright. A lock on the
//! directory keeps a second recorder on the same machine from racing the first.
//!
//! The reader (`load`) tolerates a missing file, a stale one, a last line cut
//! off mid-write and lines from a newer version, which it skips and counts.
use super::Snapshot;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Version of a history line. A reader skips lines from a newer version.
pub(crate) const VERSION: u32 = 1;
/// Seconds in one recorded slot, the same span as a telemetry ridge slot.
pub(crate) const SLOT_SECS: i64 = 150;
/// Days of history kept; older rows are dropped on the next write.
pub(crate) const KEEP_DAYS: i64 = 14;
/// Rows kept at most: `KEEP_DAYS` of full slots.
const MAX_ROWS: usize = (KEEP_DAYS * 86_400 / SLOT_SECS) as usize;
/// Bytes read at most, from the end of the file. A full file is about 1 MiB.
const READ_CAP: u64 = 4 * 1024 * 1024;
const FILE_NAME: &str = "telemetry.jsonl";

/// One closed slot of this machine's readings.
///
/// `power_w` is the slot's **mean** whole-system power, so that power × time
/// is energy. The other readings are the slot's **peaks**, as on the ridges,
/// so a short spike is not averaged away. A reading the machine does not have
/// is left out of the line.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Row {
    pub(crate) v: u32,
    /// Unix seconds at which the slot opened, a multiple of `secs`.
    pub(crate) t: i64,
    /// Length of the slot in seconds.
    pub(crate) secs: i64,
    /// Sensor polls folded into the slot.
    pub(crate) n: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) power_w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cpu_pct: Option<f64>,
    /// Hottest card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) gpu_temp: Option<f64>,
    /// Busiest card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) gpu_util: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mem_pct: Option<f64>,
}

impl Row {
    /// Unix seconds at which the slot closed.
    pub(crate) fn end(&self) -> i64 {
        self.t + self.secs
    }

    /// Drop readings no sensor could produce, so a damaged or hostile line
    /// cannot paint the city or inflate an energy total.
    fn sane(mut self) -> Option<Self> {
        if !(1..=3600).contains(&self.secs) {
            return None;
        }
        let within =
            |v: Option<f64>, top: f64| v.filter(|v| v.is_finite() && (0.0..=top).contains(v));
        self.power_w = within(self.power_w, 100_000.0);
        self.cpu_pct = within(self.cpu_pct, 100.0);
        self.gpu_temp = within(self.gpu_temp, 200.0);
        self.gpu_util = within(self.gpu_util, 100.0);
        self.mem_pct = within(self.mem_pct, 100.0);
        Some(self)
    }
}

/// The two fields every version keeps, read before deciding whether a line
/// is one this build understands.
#[derive(Deserialize)]
struct Header {
    v: u32,
    t: i64,
}

/// A loaded history, oldest first, one row per slot.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct History {
    pub(crate) rows: Vec<Row>,
    /// Lines that did not parse, including a last line cut off mid-write.
    pub(crate) skipped: usize,
    /// Lines from a newer version, not read.
    pub(crate) newer: usize,
}

impl History {
    /// The row whose slot holds unix second `at`. A slot that was not
    /// recorded has no row: there is no nearest-neighbour fallback.
    pub(crate) fn at(&self, at: f64) -> Option<&Row> {
        let i = self.rows.partition_point(|r| (r.t as f64) <= at);
        let row = self.rows.get(i.checked_sub(1)?)?;
        (at < row.end() as f64).then_some(row)
    }
}

/// What reading the history file found.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Loaded {
    /// No file (nothing has been recorded here), or no place to look.
    Absent,
    /// A file that could not be read, with the reason.
    Failed(String),
    Read(History),
}

/// The directory the history lives in: `$XDG_STATE_HOME/systemscape`, else
/// `$HOME/.local/state/systemscape`. A relative `XDG_STATE_HOME` is ignored,
/// as the XDG specification asks.
fn state_dir(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let absolute = |v: Option<std::ffi::OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    absolute(xdg)
        .or_else(|| absolute(home).map(|h| h.join(".local/state")))
        .map(|base| base.join("systemscape"))
}

/// The history file's path for this user, if there is a place for it.
pub(crate) fn default_path() -> Option<PathBuf> {
    state_dir(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
        .map(|dir| dir.join(FILE_NAME))
}

/// Parse a history file's text. Rows come out sorted by time with one row per
/// slot (the later line wins); unreadable lines are counted, not fatal.
fn parse(text: &str) -> History {
    let mut history = History::default();
    let complete = text.ends_with('\n');
    let lines: Vec<&str> = text.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        // The last line of a file that does not end in a newline was cut off
        // mid-write: it may parse, but it may also be a prefix of a longer number.
        if i + 1 == lines.len() && !complete {
            history.skipped += 1;
            continue;
        }
        match serde_json::from_str::<Header>(line) {
            Ok(header) if header.v > VERSION => history.newer += 1,
            Ok(header) if header.v == VERSION => {
                match serde_json::from_str::<Row>(line).ok().and_then(Row::sane) {
                    Some(row) => history.rows.push(row),
                    None => history.skipped += 1,
                }
            }
            _ => history.skipped += 1,
        }
    }
    history.rows.sort_by_key(|r| r.t);
    // One row per slot: of two lines for one slot, the later one in the file wins.
    history.rows.reverse();
    history.rows.dedup_by_key(|r| r.t);
    history.rows.reverse();
    history
}

/// Read up to `READ_CAP` bytes from the end of a file that must be a regular
/// file, not a link. A read that starts mid-file drops the partial first line.
fn read_tail(path: &Path) -> std::io::Result<Option<String>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_file() {
        return Err(std::io::Error::other(
            "not a regular file (a link or other entry is never followed)",
        ));
    }
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(READ_CAP);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((len - start) as usize);
    file.take(READ_CAP).read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        text = text
            .split_once('\n')
            .map(|(_, rest)| rest.to_owned())
            .unwrap_or_default();
    }
    Ok(Some(text))
}

/// Load the history at `path`, tolerating a missing, stale, truncated or
/// newer-version file.
pub(crate) fn load(path: Option<&Path>) -> Loaded {
    let Some(path) = path else {
        return Loaded::Absent;
    };
    match read_tail(path) {
        Ok(None) => Loaded::Absent,
        Ok(Some(text)) => Loaded::Read(parse(&text)),
        Err(e) => Loaded::Failed(format!("{}: {e}", path.display())),
    }
}

/// The open slot's readings, folded from every sensor poll inside it.
struct Slot {
    t: i64,
    n: u32,
    power_sum: f64,
    power_n: u32,
    cpu: Option<f64>,
    gpu_temp: Option<f64>,
    gpu_util: Option<f64>,
    mem: Option<f64>,
    /// The first slot after start-up began part-way through: never written.
    partial: bool,
}

impl Slot {
    fn new(t: i64, partial: bool) -> Self {
        Self {
            t,
            n: 0,
            power_sum: 0.0,
            power_n: 0,
            cpu: None,
            gpu_temp: None,
            gpu_util: None,
            mem: None,
            partial,
        }
    }

    fn fold(&mut self, snapshot: &Snapshot) {
        let peak = |held: Option<f64>, v: Option<f64>| match (held, v.filter(|v| v.is_finite())) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.n += 1;
        if let Some(w) = snapshot.power_w.filter(|w| w.is_finite()) {
            self.power_sum += w;
            self.power_n += 1;
        }
        self.cpu = peak(self.cpu, snapshot.cpu_pct);
        self.mem = peak(self.mem, snapshot.mem_pct);
        for gpu in &snapshot.gpus {
            self.gpu_temp = peak(self.gpu_temp, gpu.temp);
            self.gpu_util = peak(self.gpu_util, gpu.util);
        }
    }

    fn row(&self) -> Row {
        let round = |v: Option<f64>| v.map(|v| (v * 10.0).round() / 10.0);
        Row {
            v: VERSION,
            t: self.t,
            secs: SLOT_SECS,
            n: self.n,
            power_w: round((self.power_n > 0).then(|| self.power_sum / f64::from(self.power_n))),
            cpu_pct: round(self.cpu),
            gpu_temp: round(self.gpu_temp),
            gpu_util: round(self.gpu_util),
            mem_pct: round(self.mem),
        }
    }
}

/// Appends each closed slot to the history file. Only built with `--record`.
pub(crate) struct Recorder {
    path: PathBuf,
    /// Held for the recorder's life: a second recorder finds the lock taken.
    _lock: Option<File>,
    slot: Option<Slot>,
    /// Why recording is not working, shown until a write succeeds.
    trouble: Option<String>,
    /// Whether a start-up problem has been returned once already.
    told: bool,
}

impl Recorder {
    /// Prepare to record at `path`: create its directory owner-only and take
    /// the directory lock. A failure here is not fatal: the recorder keeps
    /// the reason, shows it, and writes nothing, so a read-only container
    /// still shows telemetry.
    pub(crate) fn open(path: Option<PathBuf>) -> Self {
        let Some(path) = path else {
            return Self {
                path: PathBuf::new(),
                _lock: None,
                slot: None,
                trouble: Some("record: no HOME or XDG_STATE_HOME; nothing written".into()),
                told: false,
            };
        };
        let (lock, trouble) = match Self::lock(&path) {
            Ok(lock) => (Some(lock), None),
            Err(reason) => (None, Some(format!("record: {reason}; nothing written"))),
        };
        Self {
            path,
            _lock: lock,
            slot: None,
            trouble,
            told: false,
        }
    }

    fn lock(path: &Path) -> Result<File, String> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = path.parent().ok_or("no directory")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let handle = File::open(dir).map_err(|e| format!("cannot open {}: {e}", dir.display()))?;
        match handle.try_lock() {
            Ok(()) => Ok(handle),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err("another systemscape is already recording here".into())
            }
            Err(std::fs::TryLockError::Error(e)) => {
                Err(format!("cannot lock {}: {e}", dir.display()))
            }
        }
    }

    /// Fold one sensor poll taken at unix second `now`. When it opens a new
    /// slot, the slot before is written. A standing problem is added to the
    /// snapshot's notes for the status line; the return value is the problem
    /// when it first appears, for a probe to print once.
    pub(crate) fn fold(&mut self, snapshot: &mut Snapshot, now: f64) -> Option<String> {
        if self._lock.is_none() {
            snapshot.notes.extend(self.trouble.clone());
            let first = !std::mem::replace(&mut self.told, true);
            return self.trouble.clone().filter(|_| first);
        }
        let t = (now / SLOT_SECS as f64).floor() as i64 * SLOT_SECS;
        let closed = match &mut self.slot {
            Some(slot) if slot.t == t => None,
            Some(_) => self.slot.replace(Slot::new(t, false)),
            None => {
                self.slot = Some(Slot::new(t, true));
                None
            }
        };
        if let Some(slot) = &mut self.slot {
            slot.fold(snapshot);
        }
        let mut fresh = None;
        if let Some(closed) = closed.filter(|s| !s.partial && s.n > 0) {
            match write(&self.path, &closed.row()) {
                Ok(()) => self.trouble = None,
                Err(reason) => {
                    let note = format!("record: {reason}");
                    if self.trouble.as_ref() != Some(&note) {
                        fresh = Some(note.clone());
                    }
                    self.trouble = Some(note);
                }
            }
        }
        snapshot.notes.extend(self.trouble.clone());
        fresh
    }
}

/// Add `row` to the history at `path` by rewriting it: keep the rows of the
/// last `KEEP_DAYS` (and lines of a newer version from that span, untouched),
/// replace any row for the same slot, write a temporary file beside it with
/// owner-only permissions, and rename it into place.
fn write(path: &Path, row: &Row) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let old = read_tail(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let oldest = row.t - KEEP_DAYS * 86_400;
    let mut kept: Vec<(i64, String)> = old
        .as_deref()
        .unwrap_or("")
        .lines()
        .filter_map(|line| {
            let header = serde_json::from_str::<Header>(line).ok()?;
            // Damaged and same-slot lines go; newer versions' lines stay as they are.
            (header.t >= oldest && header.t != row.t).then(|| (header.t, line.to_owned()))
        })
        .collect();
    kept.push((
        row.t,
        serde_json::to_string(row).map_err(|e| e.to_string())?,
    ));
    kept.sort_by_key(|(t, _)| *t);
    let surplus = kept.len().saturating_sub(MAX_ROWS);
    let mut text = String::new();
    for (_, line) in &kept[surplus..] {
        text.push_str(line);
        text.push('\n');
    }
    let dir = path.parent().ok_or("no directory")?;
    let name = path.file_name().ok_or("no file name")?.to_string_lossy();
    let temp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let create = || {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true) // never opens an existing entry, link or not
            .mode(0o600)
            .open(&temp)
    };
    let mut file = match create() {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Left by an earlier run with the same process ID.
            let _ = fs::remove_file(&temp);
            create()
        }
        other => other,
    }
    .map_err(|e| format!("cannot create {}: {e}", temp.display()))?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temp, path));
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(format!("cannot write {}: {e}", path.display()));
    }
    // Make the rename durable; a failure here loses nothing already written.
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::Gpu;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("systemscape-record-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("systemscape").join(FILE_NAME)
    }

    fn reading(power: f64, cpu: f64, util: Option<f64>) -> Snapshot {
        Snapshot {
            power_w: Some(power),
            cpu_pct: Some(cpu),
            mem_pct: Some(40.0),
            gpus: vec![
                Gpu {
                    temp: Some(50.0),
                    util,
                    ..Gpu::default()
                },
                Gpu {
                    temp: Some(70.0),
                    util: util.map(|u| u / 2.0),
                    ..Gpu::default()
                },
            ],
            ..Snapshot::default()
        }
    }

    #[test]
    fn the_state_directory_follows_xdg_and_ignores_a_relative_one() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        assert_eq!(
            state_dir(os("/state"), os("/home/u")),
            Some(PathBuf::from("/state/systemscape"))
        );
        assert_eq!(
            state_dir(os("relative"), os("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/systemscape"))
        );
        assert_eq!(
            state_dir(None, os("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/systemscape"))
        );
        assert_eq!(state_dir(None, None), None);
        assert_eq!(state_dir(os(""), os("")), None);
    }

    #[test]
    fn a_slot_records_mean_power_and_peak_load_and_the_first_partial_slot_is_skipped() {
        let path = scratch("fold");
        let mut recorder = Recorder::open(Some(path.clone()));
        let t0 = 1_763_000_000.0 - 1_763_000_000.0 % 150.0;
        // Start-up lands mid-slot: that slot is never written.
        recorder.fold(&mut reading(100.0, 10.0, Some(5.0)), t0 + 70.0);
        recorder.fold(&mut reading(200.0, 90.0, Some(80.0)), t0 + 150.0);
        recorder.fold(&mut reading(400.0, 30.0, None), t0 + 152.0);
        assert!(
            load(Some(&path)) == Loaded::Absent,
            "nothing until a full slot closes"
        );
        recorder.fold(&mut reading(999.0, 1.0, Some(1.0)), t0 + 300.0);
        let Loaded::Read(history) = load(Some(&path)) else {
            panic!("history not written");
        };
        assert_eq!(history.rows.len(), 1);
        let row = &history.rows[0];
        assert_eq!((row.t, row.secs, row.n), (t0 as i64 + 150, 150, 2));
        assert_eq!(row.power_w, Some(300.0), "power is the slot mean");
        assert_eq!(row.cpu_pct, Some(90.0), "load is the slot peak");
        assert_eq!(row.gpu_util, Some(80.0), "the busiest card");
        assert_eq!(row.gpu_temp, Some(70.0), "the hottest card");
        assert_eq!(row.mem_pct, Some(40.0));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_file_is_owner_only_bounded_in_days_and_refuses_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch("bounds");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let day = 86_400;
        let row = |t: i64| Row {
            v: VERSION,
            t,
            secs: SLOT_SECS,
            n: 75,
            power_w: Some(300.0),
            ..Row::default()
        };
        let now = 2_000_000_000 - 2_000_000_000 % SLOT_SECS;
        // An old row, a newer version's line, a damaged line and a cut-off end.
        let old = serde_json::to_string(&row(now - (KEEP_DAYS + 1) * day)).unwrap();
        let recent = serde_json::to_string(&row(now - day)).unwrap();
        let newer = format!(
            "{{\"v\":{},\"t\":{},\"novel\":true}}",
            VERSION + 1,
            now - 600
        );
        fs::write(
            &path,
            format!("{old}\n{recent}\n{newer}\nnot json\n{{\"v\":1,\"t\":12"),
        )
        .unwrap();
        write(&path, &row(now)).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the history is readable by its owner only");
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains(&old),
            "rows older than the kept days are dropped"
        );
        assert!(text.contains(&recent) && text.contains(&newer), "{text}");
        assert!(!text.contains("not json") && text.ends_with('\n'));
        let Loaded::Read(history) = load(Some(&path)) else {
            panic!("history unreadable");
        };
        assert_eq!(
            history.rows.iter().map(|r| r.t).collect::<Vec<_>>(),
            [now - day, now]
        );
        assert_eq!((history.newer, history.skipped), (1, 0));
        let leftovers = fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary file is left behind");

        // A link at the final path is neither followed nor replaced.
        let target = path.with_file_name("elsewhere");
        fs::write(&target, "precious\n").unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(write(&path, &row(now + 150)).is_err());
        assert!(matches!(load(Some(&path)), Loaded::Failed(_)));
        assert_eq!(fs::read_to_string(&target).unwrap(), "precious\n");
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_reader_tolerates_absent_truncated_newer_and_insane_lines() {
        assert_eq!(
            load(Some(Path::new("/nonexistent/systemscape/x.jsonl"))),
            Loaded::Absent
        );
        assert_eq!(load(None), Loaded::Absent);
        let text = concat!(
            "{\"v\":1,\"t\":300,\"secs\":150,\"n\":3,\"cpu_pct\":50.0}\n",
            "{\"v\":1,\"t\":150,\"secs\":150,\"n\":3,\"cpu_pct\":20.0,\"power_w\":-5}\n",
            "{\"v\":1,\"t\":300,\"secs\":150,\"n\":4,\"cpu_pct\":55.0}\n",
            "{\"v\":1,\"t\":450,\"secs\":0,\"n\":4}\n",
            "{\"v\":9,\"t\":600,\"whatever\":[1,2]}\n",
            "{\"v\":1,\"t\":750,\"secs\":150,\"n\":3,\"cpu_pct\":9",
        );
        let history = parse(text);
        assert_eq!(
            history.rows.iter().map(|r| r.t).collect::<Vec<_>>(),
            [150, 300]
        );
        assert_eq!(
            history.rows[0].power_w, None,
            "an impossible reading is dropped"
        );
        assert_eq!(
            history.rows[1].cpu_pct,
            Some(55.0),
            "the later line for a slot wins"
        );
        assert_eq!((history.skipped, history.newer), (2, 1));
        assert_eq!(history.at(160.0).map(|r| r.t), Some(150));
        assert_eq!(history.at(449.9).map(|r| r.t), Some(300));
        assert_eq!(
            history.at(450.0),
            None,
            "past the last slot there is no reading"
        );
        assert_eq!(history.at(10.0), None);
        let gap = parse(
            "{\"v\":1,\"t\":0,\"secs\":150,\"n\":1}\n{\"v\":1,\"t\":600,\"secs\":150,\"n\":1}\n",
        );
        assert_eq!(gap.at(300.0), None, "a gap stays a gap");
    }

    #[test]
    fn a_second_recorder_in_the_same_place_writes_nothing_and_says_why() {
        let path = scratch("lock");
        let first = Recorder::open(Some(path.clone()));
        assert!(first.trouble.is_none());
        let mut second = Recorder::open(Some(path.clone()));
        let mut snapshot = reading(1.0, 1.0, None);
        second.fold(&mut snapshot, 0.0);
        assert!(
            snapshot
                .notes
                .iter()
                .any(|n| n.contains("already recording")),
            "{:?}",
            snapshot.notes
        );
        drop(first);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
