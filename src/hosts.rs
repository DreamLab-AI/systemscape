//! Federated telemetry: other machines' sensors beside this one's.
//!
//! A hosts file lists the machines to watch. For each, SystemScape runs a
//! probe, normally `systemscape --probe` on that machine over the system
//! OpenSSH client, and reads one versioned JSON snapshot per line from its
//! standard output. SystemScape never handles a password or key itself:
//! authentication, host-key checking and encryption are OpenSSH's, configured
//! as usual in `~/.ssh/config` and the agent. A host can instead name a local
//! `command` that prints the same lines, which is how tests run without ssh.
//!
//! ```toml
//! [[host]]
//! name = "render-box"
//! ssh = "user@render-box.lan"
//! probe = "/home/user/.local/bin/systemscape"
//! ```
use crate::telemetry::Snapshot;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Wire version of a probe line. A different version is reported, not guessed at.
pub(crate) const PROBE_VERSION: u32 = 1;
/// A probe line longer than this is not a snapshot.
const MAX_LINE: u64 = 64 * 1024;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// One line of probe output.
#[derive(Serialize, Deserialize)]
pub(crate) struct Frame {
    pub v: u32,
    #[serde(default)]
    pub snapshot: Snapshot,
}

// ---------------------------------------------------------------- config

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    /// Show this machine as well as the listed hosts.
    #[serde(default = "yes")]
    local: bool,
    /// Name for this machine; defaults to its hostname.
    local_name: Option<String>,
    #[serde(default, rename = "host")]
    hosts: Vec<HostEntry>,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostEntry {
    name: String,
    /// OpenSSH destination, `user@host` or a `~/.ssh/config` alias.
    ssh: Option<String>,
    port: Option<u16>,
    identity_file: Option<String>,
    /// Extra `-o` options, each `Key=Value`.
    #[serde(default)]
    ssh_options: Vec<String>,
    /// Probe binary on the remote host.
    probe: Option<String>,
    /// Run this argv locally instead of ssh; it must print probe lines.
    command: Option<Vec<String>>,
    interval: Option<f64>,
    gpu_interval: Option<f64>,
}

/// A validated host: what to run, and how often it reports.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Host {
    pub name: String,
    pub argv: Vec<String>,
    pub interval: Duration,
}

/// Everything a hosts file asks for.
#[derive(Debug, PartialEq)]
pub(crate) struct Federation {
    /// This machine's name, or `None` when the file sets `local = false`.
    pub local: Option<String>,
    pub hosts: Vec<Host>,
}

/// `$XDG_CONFIG_HOME/systemscape/hosts.toml`, else `~/.config/systemscape/hosts.toml`.
pub(crate) fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("systemscape").join("hosts.toml"))
}

/// Load the hosts file. An explicit path must exist; a missing default file
/// means "this machine only", exactly as before hosts files existed.
pub(crate) fn load(explicit: Option<&Path>) -> Result<Option<Federation>, String> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => match default_path() {
            Some(path) if path.exists() => path,
            _ => return Ok(None),
        },
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read hosts file {}: {e}", path.display()))?;
    parse(&text, &local_hostname())
        .map(Some)
        .map_err(|e| format!("hosts file {}: {e}", path.display()))
}

fn local_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "local".into())
}

/// Parse and validate a hosts file. `hostname` names this machine by default.
pub(crate) fn parse(text: &str, hostname: &str) -> Result<Federation, String> {
    let file: FileConfig = toml::from_str(text).map_err(|e| e.message().to_string())?;
    let local = file
        .local
        .then(|| file.local_name.unwrap_or_else(|| hostname.to_string()));
    let mut names: Vec<String> = local.iter().cloned().collect();
    if let Some(name) = &local {
        check_name(name)?;
    }
    let mut hosts = Vec::new();
    for entry in file.hosts {
        check_name(&entry.name)?;
        if names.contains(&entry.name) {
            return Err(format!("host name {:?} is used twice", entry.name));
        }
        names.push(entry.name.clone());
        hosts.push(validate(entry)?);
    }
    if local.is_none() && hosts.is_empty() {
        return Err("local = false needs at least one [[host]]".into());
    }
    Ok(Federation { local, hosts })
}

fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty() && name.chars().count() <= 16 && !name.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "host name {name:?} must be 1 to 16 printable characters"
        ))
    }
}

/// Words that reach the remote shell. ssh joins the command into one string
/// for the remote user's login shell, so only plain path characters pass.
fn shell_safe(word: &str) -> bool {
    !word.is_empty()
        && !word.starts_with('-')
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./~+-".contains(c))
}

fn seconds(value: Option<f64>, default: f64, lo: f64, hi: f64, what: &str) -> Result<f64, String> {
    let v = value.unwrap_or(default);
    if v.is_finite() && (lo..=hi).contains(&v) {
        Ok(v)
    } else {
        Err(format!("{what} must be between {lo} and {hi} seconds"))
    }
}

fn validate(entry: HostEntry) -> Result<Host, String> {
    let name = entry.name;
    let at = |e: String| format!("host {name:?}: {e}");
    let interval = seconds(entry.interval, 2.0, 0.5, 60.0, "interval").map_err(at)?;
    let gpu_interval = seconds(entry.gpu_interval, 10.0, interval, 600.0, "gpu_interval")
        .map_err(|e| at(format!("{e}, and at least interval")))?;
    let argv = match (entry.ssh, entry.command) {
        (Some(_), Some(_)) => return Err(at("set ssh or command, not both".into())),
        (None, None) => return Err(at("set ssh (a destination) or command".into())),
        (None, Some(command)) => {
            let unused = entry.port.is_some()
                || entry.identity_file.is_some()
                || !entry.ssh_options.is_empty()
                || entry.probe.is_some();
            if unused {
                return Err(at(
                    "port, identity_file, ssh_options and probe apply only to ssh hosts".into(),
                ));
            }
            if command.first().is_none_or(|c| c.is_empty()) {
                return Err(at("command must name a program".into()));
            }
            command
        }
        (Some(destination), None) => {
            let options = SshOptions {
                port: entry.port,
                identity_file: entry.identity_file.map(|p| expand_home(&p)),
                extra: entry.ssh_options,
            };
            ssh_argv(
                &destination,
                &options,
                entry.probe.as_deref().unwrap_or("systemscape"),
                interval,
                gpu_interval,
            )
            .map_err(at)?
        }
    };
    Ok(Host {
        name,
        argv,
        interval: Duration::from_secs_f64(interval),
    })
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

#[derive(Default)]
struct SshOptions {
    port: Option<u16>,
    identity_file: Option<String>,
    extra: Vec<String>,
}

/// The ssh invocation for one host. It never prompts (`BatchMode`), keeps
/// stdin closed so it cannot read the terminal, gives up on a dead link within
/// about fifteen seconds, and leaves host-key checking at the user's setting.
fn ssh_argv(
    destination: &str,
    options: &SshOptions,
    probe: &str,
    interval: f64,
    gpu_interval: f64,
) -> Result<Vec<String>, String> {
    if destination.is_empty()
        || destination.starts_with('-')
        || destination
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("ssh destination {destination:?} is not valid"));
    }
    if !shell_safe(probe) {
        return Err(format!(
            "probe {probe:?} must be a plain path (letters, digits, _ . / ~ + -)"
        ));
    }
    let mut argv: Vec<String> = ["ssh", "-n", "-T"].map(String::from).to_vec();
    for option in [
        "BatchMode=yes",
        "ConnectTimeout=5",
        "ServerAliveInterval=5",
        "ServerAliveCountMax=2",
        "LogLevel=ERROR",
    ] {
        argv.extend(["-o".into(), option.into()]);
    }
    if let Some(port) = options.port {
        argv.extend(["-p".into(), port.to_string()]);
    }
    if let Some(identity) = &options.identity_file {
        argv.extend(["-i".into(), identity.clone()]);
        argv.extend(["-o".into(), "IdentitiesOnly=yes".into()]);
    }
    for option in &options.extra {
        let valid = option.contains('=')
            && !option.starts_with('-')
            && !option.chars().any(char::is_control);
        if !valid {
            return Err(format!("ssh option {option:?} must be Key=Value"));
        }
        argv.extend(["-o".into(), option.clone()]);
    }
    argv.extend([
        "--".into(),
        destination.into(),
        probe.into(),
        "--probe".into(),
        "--interval".into(),
        interval.to_string(),
        "--gpu-interval".into(),
        gpu_interval.to_string(),
    ]);
    Ok(argv)
}

// ---------------------------------------------------------------- link

/// What is known about a host's connection.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Link {
    Connecting,
    Live,
    /// Connected, but no snapshot for longer than three intervals.
    Stale,
    /// The probe is not running; the text says why.
    Down(String),
}

impl Link {
    pub(crate) fn glyph(&self) -> char {
        match self {
            Link::Live => '●',
            Link::Connecting => '◌',
            Link::Stale => '○',
            Link::Down(_) => '!',
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Link::Live => "live".into(),
            Link::Connecting => "connecting".into(),
            Link::Stale => "no reading for a while".into(),
            Link::Down(why) => why.clone(),
        }
    }
}

/// A message from a host's worker.
pub(crate) enum Report {
    Frame(Box<Snapshot>),
    Link(Link),
}

/// Keep a stderr line printable and short enough for the status line.
fn tidy(line: &str) -> String {
    let clean: String = line
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let clean = clean.trim();
    match clean.char_indices().nth(120) {
        Some((cut, _)) => format!("{}…", &clean[..cut]),
        None => clean.to_string(),
    }
}

/// Why a probe process ended, in words for the status line.
fn exit_reason(code: Option<i32>, via_ssh: bool, stderr: &str, frames: u64) -> String {
    let detail = if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    };
    match code {
        Some(127) => format!("probe not found (exit 127){detail}"),
        Some(255) if via_ssh => format!("ssh failed{detail}"),
        Some(0) if frames == 0 => format!("probe printed nothing{detail}"),
        Some(code) => format!("probe exited ({code}){detail}"),
        None => format!("probe killed{detail}"),
    }
}

/// Parse one probe line. `Ok(None)` is a line to skip; an error means the
/// probe speaks another protocol version and retrying soon will not help.
pub(crate) fn read_frame(line: &str) -> Result<Option<Snapshot>, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    #[derive(Deserialize)]
    struct Version {
        v: u32,
    }
    let Ok(Version { v }) = serde_json::from_str::<Version>(line) else {
        return Ok(None); // not a probe line: a login banner, say
    };
    if v != PROBE_VERSION {
        return Err(format!(
            "incompatible probe: protocol v{v}, this build reads v{PROBE_VERSION}"
        ));
    }
    // One malformed line is skipped; the next poll brings another.
    Ok(serde_json::from_str::<Frame>(line)
        .ok()
        .map(|frame| frame.snapshot))
}

/// A host's worker thread and its probe process. Dropping it kills the probe.
pub(crate) struct Remote {
    pub interval: Duration,
    pub reports: Receiver<Report>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    worker: Option<JoinHandle<()>>,
}

impl Remote {
    pub(crate) fn spawn(host: Host) -> Self {
        let (tx, reports) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let worker = {
            let (stop, child, argv) = (stop.clone(), child.clone(), host.argv.clone());
            std::thread::spawn(move || supervise(&argv, &tx, &stop, &child))
        };
        Self {
            interval: host.interval,
            reports,
            stop,
            child,
            worker: Some(worker),
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().ok().and_then(|mut c| c.take()) {
            reap(child);
        }
        // Wake the worker from its backoff and let it finish on its own:
        // joining could wait on a grandchild that still holds the pipe open.
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
        }
    }
}

/// Exit code of a probe whose output has ended. It usually exits a moment
/// later on its own, so it gets a second to do so before it is killed:
/// killing at once would hide its real code (127 for a missing binary).
fn finish(child: &mut Child) -> Option<i32> {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    child.wait().ok().and_then(|status| status.code())
}

/// Whether a stderr line says anything: shells such as fish underline the
/// failing command with a line of `^~~~^`, which is not the reason.
fn informative(line: &str) -> bool {
    line.chars().any(|c| !matches!(c, '^' | '~' | ' '))
}

fn reap(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Run the probe, forward its snapshots, and restart it with backoff when it
/// ends, until the `Remote` is dropped or nobody is listening.
fn supervise(argv: &[String], tx: &Sender<Report>, stop: &AtomicBool, slot: &Mutex<Option<Child>>) {
    let via_ssh = argv.first().is_some_and(|p| p == "ssh");
    let mut backoff = Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) {
        if tx.send(Report::Link(Link::Connecting)).is_err() {
            return;
        }
        let spawned = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                let why = format!("cannot start {}: {e}", argv[0]);
                if tx.send(Report::Link(Link::Down(why))).is_err() {
                    return;
                }
                std::thread::park_timeout(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        match slot.lock() {
            // Dropped while starting: do not leave the probe behind.
            Ok(mut held) if !stop.load(Ordering::SeqCst) => *held = Some(child),
            _ => return reap(child),
        }
        // Keep the first informative stderr line (error output leads with the
        // reason); never let stderr reach the terminal.
        let last_err = Arc::new(Mutex::new(String::new()));
        let err_reader = stderr.map(|stderr| {
            let last_err = last_err.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let line = tidy(&line);
                    if informative(&line) {
                        if let Ok(mut last) = last_err.lock() {
                            if last.is_empty() {
                                *last = line;
                            }
                        }
                    }
                }
            })
        });

        let (mut frames, mut fault) = (0u64, None);
        if let Some(stdout) = stdout {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.by_ref().take(MAX_LINE).read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if !line.ends_with('\n') && line.len() as u64 >= MAX_LINE {
                    fault = Some("probe line too long".to_string());
                    break;
                }
                match read_frame(&line) {
                    Ok(Some(snapshot)) => {
                        if frames == 0 {
                            backoff = Duration::from_secs(1);
                            if tx.send(Report::Link(Link::Live)).is_err() {
                                break;
                            }
                        }
                        frames += 1;
                        if tx.send(Report::Frame(Box::new(snapshot))).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(why) => {
                        // A probe of another version will not change on retry soon.
                        backoff = MAX_BACKOFF;
                        fault = Some(why);
                        break;
                    }
                }
            }
        }
        let code = slot
            .lock()
            .ok()
            .and_then(|mut held| held.take())
            .map(|mut child| finish(&mut child));
        if let Some(reader) = err_reader {
            let _ = reader.join();
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let stderr = last_err.lock().map(|s| s.clone()).unwrap_or_default();
        let why = fault.unwrap_or_else(|| exit_reason(code.flatten(), via_ssh, &stderr, frames));
        if tx.send(Report::Link(Link::Down(why))).is_err() {
            return;
        }
        std::thread::park_timeout(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_file_adds_one_ssh_host_beside_this_machine() {
        let fed = parse(
            "[[host]]\nname = \"lab\"\nssh = \"user@host.example\"\nprobe = \"~/bin/systemscape\"\n",
            "box",
        )
        .unwrap();
        assert_eq!(fed.local.as_deref(), Some("box"));
        assert_eq!(fed.hosts.len(), 1);
        let host = &fed.hosts[0];
        assert_eq!(host.interval, Duration::from_secs(2));
        assert_eq!(&host.argv[..3], ["ssh", "-n", "-T"]);
        let tail: Vec<&str> = host
            .argv
            .iter()
            .rev()
            .take(8)
            .rev()
            .map(String::as_str)
            .collect();
        assert_eq!(
            tail,
            [
                "--",
                "user@host.example",
                "~/bin/systemscape",
                "--probe",
                "--interval",
                "2",
                "--gpu-interval",
                "10"
            ]
        );
        for option in ["BatchMode=yes", "ConnectTimeout=5", "LogLevel=ERROR"] {
            assert!(host.argv.iter().any(|a| a == option), "missing {option}");
        }
        assert!(
            !host
                .argv
                .iter()
                .any(|a| a.contains("StrictHostKeyChecking")),
            "host-key checking stays the user's choice"
        );
    }

    #[test]
    fn ssh_settings_become_arguments_never_a_local_shell_string() {
        let fed = parse(
            r#"
            local = false
            [[host]]
            name = "gpu"
            ssh = "lab-gpu"
            port = 2222
            identity_file = "/keys/id_ed25519"
            ssh_options = ["ProxyJump=bastion"]
            interval = 5
            gpu_interval = 30
            "#,
            "box",
        )
        .unwrap();
        assert_eq!(fed.local, None);
        let argv = &fed.hosts[0].argv;
        let after = |flag: &str| {
            argv.iter()
                .position(|a| a == flag)
                .map(|i| argv[i + 1].as_str())
        };
        assert_eq!(after("-p"), Some("2222"));
        assert_eq!(after("-i"), Some("/keys/id_ed25519"));
        assert!(argv.iter().any(|a| a == "ProxyJump=bastion"));
        assert!(argv.iter().any(|a| a == "IdentitiesOnly=yes"));
        assert_eq!(after("--interval"), Some("5"));
        assert_eq!(after("--gpu-interval"), Some("30"));
        assert_eq!(fed.hosts[0].interval, Duration::from_secs(5));
    }

    #[test]
    fn a_command_host_runs_its_argv_as_given() {
        let fed = parse(
            "local_name = \"here\"\n[[host]]\nname = \"loop\"\ncommand = [\"systemscape\", \"--probe\"]\n",
            "box",
        )
        .unwrap();
        assert_eq!(fed.local.as_deref(), Some("here"));
        assert_eq!(fed.hosts[0].argv, ["systemscape", "--probe"]);
    }

    #[test]
    fn unsafe_or_ambiguous_files_are_refused_with_a_reason() {
        let refused = |text: &str, expect: &str| {
            let err = parse(text, "box").unwrap_err();
            assert!(err.contains(expect), "{text:?} gave {err:?}");
        };
        refused("[[host]]\nname = \"a\"\n", "set ssh");
        refused(
            "[[host]]\nname = \"a\"\nssh = \"h\"\ncommand = [\"x\"]\n",
            "not both",
        );
        refused(
            "[[host]]\nname = \"a\"\nssh = \"-oProxyCommand=x\"\n",
            "destination",
        );
        refused(
            "[[host]]\nname = \"a\"\nssh = \"h\"\nprobe = \"x; rm -rf ~\"\n",
            "plain path",
        );
        refused(
            "[[host]]\nname = \"a\"\nssh = \"h\"\nprobe = \"$(id)\"\n",
            "plain path",
        );
        refused(
            "[[host]]\nname = \"a\"\nssh = \"h\"\nssh_options = [\"-F/tmp/x\"]\n",
            "Key=Value",
        );
        refused("[[host]]\nname = \"box\"\nssh = \"h\"\n", "used twice");
        refused(
            "[[host]]\nname = \"a\"\nssh = \"h\"\n[[host]]\nname = \"a\"\nssh = \"g\"\n",
            "used twice",
        );
        refused(
            "[[host]]\nname = \"a\"\nssh = \"h\"\ninterval = 0.1\n",
            "interval",
        );
        refused(
            "[[host]]\nname = \"a\"\nssh = \"h\"\ninterval = 5\ngpu_interval = 2\n",
            "gpu_interval",
        );
        refused(
            "[[host]]\nname = \"a\"\ncommand = [\"x\"]\nport = 22\n",
            "only to ssh hosts",
        );
        refused("[[host]]\nname = \"a\"\ncommand = []\n", "name a program");
        refused(
            "[[host]]\nname = \"a\"\nssh = \"h\"\npasword = \"x\"\n",
            "unknown field",
        );
        refused("local = false\n", "at least one");
        refused(
            "[[host]]\nname = \"a-very-long-host-name\"\nssh = \"h\"\n",
            "1 to 16",
        );
    }

    #[test]
    fn the_documented_example_file_parses() {
        let fed = parse(include_str!("../docs/hosts.example.toml"), "box").unwrap();
        assert_eq!(fed.local.as_deref(), Some("box"));
        assert_eq!(fed.hosts.len(), 1);
        assert_eq!(fed.hosts[0].name, "render-box");
    }

    #[test]
    fn an_explicit_missing_file_is_an_error_but_no_default_file_is_not() {
        let missing = Path::new("/nonexistent/systemscape/hosts.toml");
        assert!(load(Some(missing)).unwrap_err().contains("cannot read"));
    }

    #[test]
    fn probe_lines_are_versioned_and_tolerate_noise() {
        let line = serde_json::to_string(&Frame {
            v: PROBE_VERSION,
            snapshot: Snapshot::default(),
        })
        .unwrap();
        assert!(matches!(read_frame(&line), Ok(Some(_))));
        assert!(matches!(read_frame("Welcome to HP\n"), Ok(None)));
        assert!(matches!(read_frame(""), Ok(None)));
        assert!(read_frame("{\"v\":2}")
            .unwrap_err()
            .contains("incompatible"));
        // Fields this build does not know, or that the probe left out, are fine.
        assert!(matches!(
            read_frame("{\"v\":1,\"snapshot\":{\"cpu_pct\":12.5,\"future\":1}}"),
            Ok(Some(s)) if s.cpu_pct == Some(12.5)
        ));
        assert!(matches!(read_frame("{\"v\":1}"), Ok(Some(_))));
        assert!(matches!(
            read_frame("{\"v\":1,\"snapshot\":{\"cpu_pct\":\"hot\"}}"),
            Ok(None)
        ));
    }

    #[test]
    fn exit_reasons_name_the_likely_fault() {
        assert!(
            exit_reason(Some(127), true, "fish: Unknown command", 0).starts_with("probe not found")
        );
        assert_eq!(
            exit_reason(Some(255), true, "Permission denied (publickey).", 0),
            "ssh failed: Permission denied (publickey)."
        );
        assert_eq!(exit_reason(Some(255), false, "", 3), "probe exited (255)");
        assert_eq!(exit_reason(Some(0), false, "", 0), "probe printed nothing");
        assert_eq!(tidy("\x1b[31mred\x1b[0m\r"), "[31mred [0m");
        assert!(!informative("^~~~~~~~~~^") && !informative(""));
        assert!(informative("fish: Unknown command: /x/systemscape"));
        assert_eq!(tidy(&"x".repeat(200)).chars().count(), 121);
    }

    fn host(script: &str) -> Host {
        Host {
            name: "t".into(),
            argv: vec!["sh".into(), "-c".into(), script.into()],
            interval: Duration::from_secs(1),
        }
    }

    fn next(remote: &Remote) -> Report {
        remote
            .reports
            .recv_timeout(Duration::from_secs(5))
            .expect("worker reported")
    }

    #[test]
    fn a_probe_process_feeds_snapshots_then_reports_why_it_ended() {
        let remote = Remote::spawn(host(
            "echo banner; echo '{\"v\":1,\"snapshot\":{\"mem_pct\":40}}'; \
             echo '{\"v\":1,\"snapshot\":{\"mem_pct\":41}}'; echo boom >&2; exit 3",
        ));
        assert!(matches!(next(&remote), Report::Link(Link::Connecting)));
        assert!(matches!(next(&remote), Report::Link(Link::Live)));
        assert!(matches!(next(&remote), Report::Frame(s) if s.mem_pct == Some(40.0)));
        assert!(matches!(next(&remote), Report::Frame(s) if s.mem_pct == Some(41.0)));
        match next(&remote) {
            Report::Link(Link::Down(why)) => assert_eq!(why, "probe exited (3): boom"),
            _ => panic!("expected the exit to be reported"),
        }
    }

    #[test]
    fn a_shell_reporting_a_missing_probe_keeps_its_exit_code_and_reason() {
        // What fish prints over ssh when the probe path does not exist.
        let remote = Remote::spawn(host(
            "echo 'fish: Unknown command: /x/systemscape' >&2; \
             echo '/x/systemscape --probe' >&2; echo '^~~~~~~~~~~~^' >&2; exit 127",
        ));
        assert!(matches!(next(&remote), Report::Link(Link::Connecting)));
        match next(&remote) {
            Report::Link(Link::Down(why)) => {
                assert_eq!(
                    why,
                    "probe not found (exit 127): fish: Unknown command: /x/systemscape"
                )
            }
            _ => panic!("expected the exit to be reported"),
        }
    }

    #[test]
    fn a_missing_program_or_wrong_version_is_reported_not_retried_blindly() {
        let missing = Remote::spawn(Host {
            argv: vec!["/nonexistent/systemscape".into(), "--probe".into()],
            ..host("")
        });
        assert!(matches!(next(&missing), Report::Link(Link::Connecting)));
        assert!(
            matches!(next(&missing), Report::Link(Link::Down(why)) if why.starts_with("cannot start"))
        );
        let old = Remote::spawn(host("echo '{\"v\":7}'; sleep 30"));
        assert!(matches!(next(&old), Report::Link(Link::Connecting)));
        assert!(matches!(next(&old), Report::Link(Link::Down(why)) if why.contains("protocol v7")));
    }

    #[test]
    fn dropping_a_remote_kills_its_probe() {
        let marker = std::env::temp_dir().join(format!("systemscape-drop-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let remote = Remote::spawn(host(&format!(
            "echo '{{\"v\":1}}'; sleep 3; touch {}",
            marker.display()
        )));
        assert!(matches!(next(&remote), Report::Link(Link::Connecting)));
        assert!(matches!(next(&remote), Report::Link(Link::Live)));
        let started = std::time::Instant::now();
        drop(remote);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "drop must not wait for the probe"
        );
        std::thread::sleep(Duration::from_millis(3500));
        assert!(!marker.exists(), "the probe outlived its Remote");
    }
}
