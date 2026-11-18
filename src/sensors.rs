//! Sensors beyond `/proc` and hwmon: which processes hold each card's VRAM,
//! the busiest containers' CPU and memory, and a llama.cpp server's metrics.
//!
//! Each arrives as named [`Reading`]s carried in the telemetry `Snapshot`
//! (and so on the probe wire) as an additive optional field. Container and
//! model-server polls run on their own threads and publish their latest
//! answer into a shared slot that the sensor thread merges into every
//! snapshot, so the frame loop never waits on docker or the network.
//!
//! Every outside answer is bounded: subprocesses get a deadline and an output
//! cap and are killed when they overrun; the HTTP client has connect, read and
//! overall deadlines and a body cap. Names found in any answer are untrusted
//! and pass through `activity_data::clean` before they reach the screen.
use crate::activity_data::clean;
use crate::pixels::char_width;
use crate::telemetry::Snapshot;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Readings kept from one snapshot; more is a broken or hostile source.
const MAX_READINGS: usize = 32;
/// Display columns of an extra channel's name, and of its unit.
const NAME_COLUMNS: usize = 12;
const UNIT_COLUMNS: usize = 4;
/// Processes listed per card, largest VRAM first.
const APPS_PER_CARD: usize = 3;
/// Containers drawn: the busiest by CPU, two ridges each.
const BUSIEST_CONTAINERS: usize = 3;
const CONTAINER_POLL: Duration = Duration::from_secs(10);
/// `docker stats --no-stream` samples for about two seconds before answering.
const DOCKER_DEADLINE: Duration = Duration::from_secs(8);
const LLAMA_POLL: Duration = Duration::from_secs(5);
/// Off screen the model server is asked a third as often, as nvidia-smi is
/// slowed, so its history keeps filling without a request every few seconds.
const LLAMA_BACKGROUND_POLL: Duration = Duration::from_secs(15);
const HTTP_DEADLINE: Duration = Duration::from_secs(4);
const HTTP_CONNECT: Duration = Duration::from_secs(2);
/// A metrics page is a few kilobytes; anything far larger is not one.
const HTTP_CAP: usize = 256 * 1024;

/// One named reading that is not one of the fixed telemetry kinds. Every
/// field is optional on the wire so probes and viewers of different builds
/// still agree; an unknown field from a newer build is ignored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Reading {
    /// Ridge label; also the channel's identity from poll to poll.
    pub(crate) name: String,
    /// Suffix after the value, such as `/s`, `%` or `G`.
    pub(crate) unit: String,
    /// No reading this poll is `None`: JSON has no NaN.
    pub(crate) value: Option<f64>,
    /// Full-scale value; absent means scale to the largest value in view.
    pub(crate) full: Option<f64>,
    /// Logarithmic height, for readings that span orders of magnitude.
    pub(crate) log: bool,
}

/// One process holding memory on one card.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct GpuApp {
    /// Card index, matching `Snapshot::gpus`.
    pub(crate) card: usize,
    pub(crate) pid: u32,
    /// Process name from `/proc/<pid>/comm`; `None` when the PID does not
    /// resolve here (another PID namespace, or the process has exited).
    pub(crate) name: Option<String>,
    pub(crate) mib: Option<f64>,
}

/// The extra sources a telemetry view was asked for on the command line.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Extras {
    /// `--containers`: per-container ridges from `docker stats`.
    pub(crate) containers: bool,
    /// `--llama-metrics URL`: a llama.cpp server's Prometheus endpoint.
    pub(crate) llama: Option<HttpUrl>,
}

// ------------------------------------------------------------ tidying

/// The longest prefix of `text` that fits in `columns` terminal cells.
pub(crate) fn fit(text: &str, columns: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|c| {
            used += char_width(*c);
            used <= columns
        })
        .collect()
}

/// Display columns of `text`.
pub(crate) fn columns(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Make readings from any source safe to draw and to key channels by:
/// cleaned and shortened names and units, no empty or repeated names,
/// finite values, a positive finite scale, and a bounded count.
pub(crate) fn tidy(snapshot: &mut Snapshot) {
    let mut seen = Vec::new();
    snapshot.extra.truncate(MAX_READINGS);
    snapshot.extra.retain_mut(|reading| {
        reading.name = fit(&clean(&reading.name), NAME_COLUMNS);
        reading.unit = fit(&clean(&reading.unit), UNIT_COLUMNS);
        reading.value = reading.value.filter(|v| v.is_finite());
        reading.full = reading.full.filter(|f| f.is_finite() && *f > 0.0);
        let fresh = !reading.name.is_empty() && !seen.contains(&reading.name);
        seen.push(reading.name.clone());
        fresh
    });
    snapshot.gpu_apps.truncate(APPS_PER_CARD * 16);
}

/// A reading's value with its unit: one decimal below ten, none above.
pub(crate) fn format_value(v: f64, unit: &str) -> String {
    if v.abs() < 10.0 && v.fract().abs() > 1e-9 {
        format!("{v:.1}{unit}")
    } else {
        format!("{v:.0}{unit}")
    }
}

// ------------------------------------------------------------ subprocesses

/// Why a bounded command gave no output.
#[derive(Debug, PartialEq)]
pub(crate) enum RunError {
    /// The program is not installed (or not on `PATH`).
    Missing,
    /// It ran and failed; the first line it printed on stderr, cleaned.
    Failed(String),
    TimedOut,
    TooLarge,
}

/// Read up to `cap + 1` bytes from a pipe on a thread of its own, so a
/// chatty child never blocks on a full pipe while its parent waits.
fn drain(mut pipe: impl Read + Send + 'static, cap: usize) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut pipe).take(cap as u64 + 1).read_to_end(&mut bytes);
        let _ = tx.send(bytes);
    });
    rx
}

/// Run `argv` (never a shell string) with a deadline and an output cap. A
/// child that overruns either is killed and reaped.
pub(crate) fn run_bounded(
    argv: &[&str],
    deadline: Duration,
    cap: usize,
) -> Result<String, RunError> {
    let (program, args) = argv.split_first().ok_or(RunError::Missing)?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => RunError::Missing,
            _ => RunError::Failed(clean(&error.to_string())),
        })?;
    let until = Instant::now() + deadline;
    let stdout = child.stdout.take().map(|pipe| drain(pipe, cap));
    let stderr = child.stderr.take().map(|pipe| drain(pipe, 4096));
    let kill = |child: &mut std::process::Child, why| {
        let _ = child.kill();
        let _ = child.wait();
        Err(why)
    };
    let out =
        match stdout.map(|rx| rx.recv_timeout(until.saturating_duration_since(Instant::now()))) {
            Some(Ok(bytes)) if bytes.len() > cap => return kill(&mut child, RunError::TooLarge),
            Some(Ok(bytes)) => bytes,
            _ => return kill(&mut child, RunError::TimedOut),
        };
    // Output has ended; the child normally exits a moment later.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            _ => return kill(&mut child, RunError::TimedOut),
        }
    };
    if status.success() {
        return Ok(String::from_utf8_lossy(&out).into_owned());
    }
    let said = stderr
        .and_then(|rx| rx.recv_timeout(Duration::from_millis(200)).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let reason = said
        .lines()
        .map(clean)
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| match status.code() {
            Some(code) => format!("exit status {code}"),
            None => "killed by a signal".into(),
        });
    Err(RunError::Failed(fit(&reason, 72)))
}

// ------------------------------------------------------------ GPU processes

/// Parse `nvidia-smi --query-compute-apps=pid,used_memory,gpu_uuid` CSV rows
/// (`--format=csv,noheader,nounits`). Memory reported as `[N/A]` stays `None`.
pub(crate) fn parse_compute_apps(csv: &str) -> Vec<(u32, Option<f64>, String)> {
    csv.lines()
        .filter_map(|line| {
            let mut fields = line.split(',').map(str::trim);
            let pid = fields.next()?.parse::<u32>().ok()?;
            let mib = fields.next()?.parse::<f64>().ok().filter(|m| m.is_finite());
            let uuid = fields.next()?.to_string();
            (!uuid.is_empty()).then_some((pid, mib, uuid))
        })
        .collect()
}

/// Parse `nvidia-smi --query-gpu=index,uuid` rows into uuids ordered by index.
pub(crate) fn parse_card_uuids(csv: &str) -> Vec<String> {
    let mut cards: Vec<(usize, String)> = csv
        .lines()
        .filter_map(|line| {
            let (index, uuid) = line.split_once(',')?;
            Some((index.trim().parse().ok()?, uuid.trim().to_string()))
        })
        .collect();
    cards.sort();
    cards.into_iter().map(|(_, uuid)| uuid).collect()
}

/// The name of a process from `/proc/<pid>/comm`, cleaned; `None` when the
/// PID does not resolve here.
pub(crate) fn process_name(pid: u32) -> Option<String> {
    let mut comm = String::new();
    std::fs::File::open(format!("/proc/{pid}/comm"))
        .ok()?
        .take(256)
        .read_to_string(&mut comm)
        .ok()?;
    let name = fit(&clean(&comm), 24);
    (!name.is_empty()).then_some(name)
}

/// Attribute processes to cards by uuid and keep the largest few per card.
/// A process on a card the uuid list does not know is left out.
pub(crate) fn attribute(
    apps: &[(u32, Option<f64>, String)],
    uuids: &[String],
    resolve: impl Fn(u32) -> Option<String>,
) -> Vec<GpuApp> {
    let mut found: Vec<GpuApp> = apps
        .iter()
        .filter_map(|(pid, mib, uuid)| {
            Some(GpuApp {
                card: uuids.iter().position(|u| u == uuid)?,
                pid: *pid,
                name: resolve(*pid),
                mib: *mib,
            })
        })
        .collect();
    found.sort_by(|a, b| {
        a.card
            .cmp(&b.card)
            .then(b.mib.unwrap_or(0.0).total_cmp(&a.mib.unwrap_or(0.0)))
    });
    let mut kept = Vec::new();
    for app in found {
        if kept.iter().filter(|k: &&GpuApp| k.card == app.card).count() < APPS_PER_CARD {
            kept.push(app);
        }
    }
    kept
}

/// Card uuids, asked once and again when a process names an unknown card.
static CARD_UUIDS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Ask nvidia-smi which processes hold memory on which card. Runs on the
/// sensor thread at the GPU cadence; a missing or slow tool yields nothing.
pub(crate) fn query_gpu_apps() -> Vec<GpuApp> {
    let nvidia = |query: &str| {
        run_bounded(
            &["nvidia-smi", query, "--format=csv,noheader,nounits"],
            Duration::from_millis(1500),
            64 * 1024,
        )
        .ok()
    };
    let Some(csv) = nvidia("--query-compute-apps=pid,used_memory,gpu_uuid") else {
        return Vec::new();
    };
    let apps = parse_compute_apps(&csv);
    if apps.is_empty() {
        return Vec::new();
    }
    let Ok(mut uuids) = CARD_UUIDS.lock() else {
        return Vec::new();
    };
    if apps.iter().any(|(_, _, uuid)| !uuids.contains(uuid)) {
        if let Some(csv) = nvidia("--query-gpu=index,uuid") {
            *uuids = parse_card_uuids(&csv);
        }
    }
    attribute(&apps, &uuids, process_name)
}

/// The largest processes on `card` that fit in `room` columns, as
/// `name 22G · pid 4123 1.5G`. Names are cleaned again: a remote probe sends them.
pub(crate) fn apps_text(apps: &[GpuApp], card: usize, room: usize) -> String {
    let mut text = String::new();
    for app in apps.iter().filter(|a| a.card == card) {
        let name = app
            .name
            .as_deref()
            .map(|n| fit(&clean(n), 24))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("pid {}", app.pid));
        let size = app
            .mib
            .map(|mib| format!(" {}", format_value(mib / 1024.0, "G")))
            .unwrap_or_default();
        let entry = format!("{}{name}{size}", if text.is_empty() { "" } else { " · " });
        if columns(&text) + columns(&entry) > room {
            break;
        }
        text.push_str(&entry);
    }
    text
}

// ------------------------------------------------------------ containers

/// One row of `docker stats --no-stream --format '{{json .}}'`.
#[derive(Debug, PartialEq)]
pub(crate) struct ContainerStat {
    pub(crate) name: String,
    pub(crate) cpu_pct: f64,
    pub(crate) mem_gib: Option<f64>,
    pub(crate) mem_limit_gib: Option<f64>,
}

/// `"484.58%"` → 484.58.
fn percent(text: &str) -> Option<f64> {
    text.trim()
        .trim_end_matches('%')
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

/// A docker size such as `9.125GiB`, `985.1MiB` or `12.3kB`, in GiB.
fn gib(text: &str) -> Option<f64> {
    let text = text.trim();
    let split = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let scale = match unit {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "KiB" => 1024.0,
        "MB" => 1e6,
        "MiB" => 1024.0 * 1024.0,
        "GB" => 1e9,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TB" => 1e12,
        "TiB" => 1024.0_f64.powi(4),
        _ => return None,
    };
    let bytes = number.trim().parse::<f64>().ok()? * scale;
    bytes
        .is_finite()
        .then_some(bytes / 1024.0 / 1024.0 / 1024.0)
}

/// Parse docker's JSON lines; a line that is not one is skipped.
pub(crate) fn parse_docker_stats(text: &str) -> Vec<ContainerStat> {
    text.lines()
        .filter_map(|line| {
            let row: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            let name = clean(row["Name"].as_str()?);
            let cpu_pct = percent(row["CPUPerc"].as_str()?)?;
            let (used, limit) = row["MemUsage"]
                .as_str()
                .and_then(|m| m.split_once('/'))
                .map_or((None, None), |(used, limit)| (gib(used), gib(limit)));
            (!name.is_empty()).then_some(ContainerStat {
                name,
                cpu_pct,
                mem_gib: used,
                mem_limit_gib: limit,
            })
        })
        .collect()
}

/// Ridges for the busiest containers by CPU. CPU is on a log scale whose
/// full height is every core busy (docker counts 100% per core, so one
/// container can read 484%); memory is on a log scale up to its limit.
pub(crate) fn container_readings(mut stats: Vec<ContainerStat>, cores: usize) -> Vec<Reading> {
    stats.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));
    stats
        .into_iter()
        .take(BUSIEST_CONTAINERS)
        .flat_map(|stat| {
            let short = fit(&stat.name, NAME_COLUMNS - 4);
            [
                Reading {
                    name: format!("{short} cpu"),
                    unit: "%".into(),
                    value: Some(stat.cpu_pct),
                    full: Some(100.0 * cores.max(1) as f64),
                    log: true,
                },
                Reading {
                    name: format!("{short} mem"),
                    unit: "G".into(),
                    value: stat.mem_gib,
                    full: stat.mem_limit_gib,
                    log: true,
                },
            ]
        })
        .collect()
}

fn poll_containers() -> Result<Vec<Reading>, String> {
    let text = run_bounded(
        &["docker", "stats", "--no-stream", "--format", "{{json .}}"],
        DOCKER_DEADLINE,
        1024 * 1024,
    )
    .map_err(|error| match error {
        RunError::Missing => "containers: docker not found".to_string(),
        RunError::TimedOut => "containers: docker stats did not answer".to_string(),
        RunError::TooLarge => "containers: docker stats answer too large".to_string(),
        RunError::Failed(why) => format!("containers: {}", fit(&why, 60)),
    })?;
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    Ok(container_readings(parse_docker_stats(&text), cores))
}

// ------------------------------------------------------------ llama.cpp

/// A plain `http://host[:port]/path` URL. Nothing else is accepted: no
/// TLS, no credentials, nothing that could smuggle a second request line.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HttpUrl {
    host: String,
    port: u16,
    path: String,
}

impl HttpUrl {
    /// Parse a URL; an empty path means `/metrics`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let rest = text
            .strip_prefix("http://")
            .ok_or("--llama-metrics needs a plain http:// URL (https is not supported)")?;
        let (authority, path) = rest.find('/').map_or((rest, "/metrics"), |at| {
            let (a, p) = rest.split_at(at);
            (a, if p == "/" { "/metrics" } else { p })
        });
        if !path.chars().all(|c| c.is_ascii_graphic()) {
            return Err("--llama-metrics URL path has spaces or control characters".into());
        }
        // `[v6 address]` keeps its colons inside the brackets; any other host has none.
        let (host, port, v6) = match authority.strip_prefix('[') {
            Some(rest) => {
                let (host, after) = rest
                    .split_once(']')
                    .ok_or("--llama-metrics URL has an unclosed [")?;
                (host, after.strip_prefix(':'), true)
            }
            None => match authority.split_once(':') {
                Some((host, port)) => (host, Some(port), false),
                None => (authority, None, false),
            },
        };
        let port = match port {
            None => 80,
            Some(port) => port
                .parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .ok_or("--llama-metrics URL has a bad port")?,
        };
        let valid = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-') || v6 && c == ':');
        if !valid {
            return Err("--llama-metrics URL needs a host name or address".into());
        }
        Ok(Self {
            host: host.to_string(),
            port,
            path: path.to_string(),
        })
    }
}

impl std::fmt::Display for HttpUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        write!(f, "http://{host}:{}{}", self.port, self.path)
    }
}

/// GET `url` over HTTP/1.0 (so the answer is never chunked), with a connect
/// timeout per address, an overall deadline and a body cap. Returns the body
/// of a 200 answer.
pub(crate) fn http_get(url: &HttpUrl, deadline: Duration, cap: usize) -> Result<String, String> {
    use std::net::{TcpStream, ToSocketAddrs};
    let until = Instant::now() + deadline;
    // The resolver has no timeout of its own, so it runs on a thread that is
    // left behind if it has not answered by the deadline.
    let (answer, resolved) = std::sync::mpsc::channel();
    let target = (url.host.clone(), url.port);
    std::thread::spawn(move || {
        let found = target.to_socket_addrs().map(|a| a.collect::<Vec<_>>());
        let _ = answer.send(found);
    });
    let addrs = resolved
        .recv_timeout(deadline)
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| format!("cannot resolve {}", url.host))?;
    let mut stream = None;
    for addr in addrs {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        if let Ok(s) = TcpStream::connect_timeout(&addr, left.min(HTTP_CONNECT)) {
            stream = Some(s);
            break;
        }
    }
    let mut stream = stream.ok_or("cannot connect")?;
    let host = if url.host.contains(':') {
        format!("[{}]", url.host)
    } else {
        url.host.clone()
    };
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {host}:{}\r\nUser-Agent: systemscape/{}\r\nAccept: text/plain\r\nConnection: close\r\n\r\n",
        url.path,
        url.port,
        env!("CARGO_PKG_VERSION")
    );
    stream
        .set_write_timeout(Some(HTTP_CONNECT))
        .and_then(|()| stream.write_all(request.as_bytes()))
        .map_err(|_| "request failed")?;
    let mut answer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("timed out".into());
        }
        // Each read waits at most until the overall deadline.
        stream
            .set_read_timeout(Some(left))
            .map_err(|_| "read failed")?;
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("timed out".into())
            }
            Err(_) => return Err("connection reset".into()),
        };
        answer.extend_from_slice(&chunk[..n]);
        if answer.len() > cap + 8192 {
            return Err(format!("answer over {} KiB", cap / 1024));
        }
        if let Some(body) = complete_body(&answer, n == 0)? {
            if body.len() > cap {
                return Err(format!("answer over {} KiB", cap / 1024));
            }
            return Ok(String::from_utf8_lossy(body).into_owned());
        }
    }
}

/// The body once the answer is complete: at `Content-Length` when given,
/// else at end of stream. `Ok(None)` means read more.
fn complete_body(answer: &[u8], ended: bool) -> Result<Option<&[u8]>, String> {
    let Some(split) = answer.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if ended {
            Err("no HTTP answer".into())
        } else {
            Ok(None)
        };
    };
    let head = String::from_utf8_lossy(&answer[..split]);
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default();
    let code = status.split_whitespace().nth(1).unwrap_or_default();
    if !status.starts_with("HTTP/") {
        return Err("no HTTP answer".into());
    }
    if code != "200" {
        return Err(format!("HTTP {}", fit(&clean(code), 3)));
    }
    let header = |name: &str| {
        lines.clone().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };
    if header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        return Err("chunked answer not supported".into());
    }
    let body = &answer[split + 4..];
    match header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        Some(length) if body.len() >= length => Ok(Some(&body[..length])),
        Some(length) if ended => Err(format!(
            "answer cut short at {} of {length} bytes",
            body.len()
        )),
        None if ended => Ok(Some(body)),
        _ => Ok(None),
    }
}

/// The llama.cpp metrics this view draws; the rest of the page is ignored.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct LlamaMetrics {
    pub(crate) tokens_predicted_total: Option<f64>,
    pub(crate) requests_processing: Option<f64>,
    pub(crate) requests_deferred: Option<f64>,
}

/// Parse Prometheus text by metric name. Comments, labelled series and
/// unknown metrics are skipped.
pub(crate) fn parse_llama(text: &str) -> LlamaMetrics {
    let mut metrics = LlamaMetrics::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(name), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Some(value) = value.parse::<f64>().ok().filter(|v| v.is_finite()) else {
            continue;
        };
        let slot = match name {
            "llamacpp:tokens_predicted_total" => &mut metrics.tokens_predicted_total,
            "llamacpp:requests_processing" => &mut metrics.requests_processing,
            "llamacpp:requests_deferred" => &mut metrics.requests_deferred,
            _ => continue,
        };
        *slot = Some(value);
    }
    metrics
}

/// Tokens per second from the predicted-token counter between two polls,
/// over wall-clock time, so idle time counts. A counter that went backwards
/// (the server restarted) gives no rate for that poll.
#[derive(Default)]
pub(crate) struct TokenRate {
    last: Option<(f64, Instant)>,
}

impl TokenRate {
    pub(crate) fn update(&mut self, total: Option<f64>, at: Instant) -> Option<f64> {
        let total = total?;
        let rate = self.last.and_then(|(before, then)| {
            let seconds = at.duration_since(then).as_secs_f64();
            (total >= before && seconds > 0.0).then(|| (total - before) / seconds)
        });
        self.last = Some((total, at));
        rate
    }
}

/// Model-server ridges, named so they sort together.
pub(crate) fn llama_readings(metrics: &LlamaMetrics, tokens_per_s: Option<f64>) -> Vec<Reading> {
    let reading = |name: &str, unit: &str, value| Reading {
        name: name.into(),
        unit: unit.into(),
        value,
        full: None,
        log: false,
    };
    vec![
        reading("LLM tok", "/s", tokens_per_s),
        reading("LLM busy", "", metrics.requests_processing),
        reading("LLM wait", "", metrics.requests_deferred),
    ]
}

// ------------------------------------------------------------ feeds

/// The latest answer of one background source.
#[derive(Default)]
struct Latest {
    readings: Vec<Reading>,
    at: Option<Instant>,
    note: Option<String>,
}

/// Where a background source leaves its answer, and how long it stays current.
struct Slot {
    latest: Mutex<Latest>,
    fresh_for: Duration,
}

/// The background sources a view runs. Dropping it stops their threads.
pub(crate) struct Feeds {
    slots: Vec<Arc<Slot>>,
    workers: Vec<std::thread::Thread>,
    stop: Arc<AtomicBool>,
    demo: bool,
}

/// What the sensor thread holds to merge the feeds into each snapshot.
#[derive(Clone)]
pub(crate) struct Merger {
    slots: Vec<Arc<Slot>>,
    demo: bool,
}

impl Feeds {
    /// Start the sources `extras` asks for. `on_screen` is the view's
    /// visibility: container polls stop while it is false.
    pub(crate) fn start(extras: &Extras, demo: bool, on_screen: Arc<AtomicBool>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mut feeds = Self {
            slots: Vec::new(),
            workers: Vec::new(),
            stop,
            demo,
        };
        if extras.containers {
            feeds.spawn(
                CONTAINER_POLL * 3,
                (CONTAINER_POLL, None),
                &on_screen,
                poll_containers,
            );
        }
        if let Some(url) = extras.llama.clone() {
            let mut rate = TokenRate::default();
            feeds.spawn(
                LLAMA_BACKGROUND_POLL * 3,
                (LLAMA_POLL, Some(LLAMA_BACKGROUND_POLL)),
                &on_screen,
                move || {
                    let body = http_get(&url, HTTP_DEADLINE, HTTP_CAP)
                        .map_err(|why| format!("llama: {why}"))?;
                    let metrics = parse_llama(&body);
                    let tokens = rate.update(metrics.tokens_predicted_total, Instant::now());
                    Ok(llama_readings(&metrics, tokens))
                },
            );
        }
        feeds
    }

    /// Run `poll` on its own thread every `pace.0` while on screen, every
    /// `pace.1` off screen (never, when `None`).
    fn spawn(
        &mut self,
        fresh_for: Duration,
        pace: (Duration, Option<Duration>),
        on_screen: &Arc<AtomicBool>,
        mut poll: impl FnMut() -> Result<Vec<Reading>, String> + Send + 'static,
    ) {
        let slot = Arc::new(Slot {
            latest: Mutex::new(Latest::default()),
            fresh_for,
        });
        let (worker_slot, on_screen, stop) = (slot.clone(), on_screen.clone(), self.stop.clone());
        let worker = std::thread::spawn(move || {
            let mut last: Option<Instant> = None;
            while !stop.load(Ordering::Relaxed) {
                let every = if on_screen.load(Ordering::Relaxed) {
                    Some(pace.0)
                } else {
                    pace.1
                };
                if every.is_some_and(|every| last.is_none_or(|at| at.elapsed() >= every)) {
                    last = Some(Instant::now());
                    let answer = poll();
                    if let Ok(mut latest) = worker_slot.latest.lock() {
                        latest.at = Some(Instant::now());
                        (latest.readings, latest.note) = match answer {
                            Ok(readings) => (readings, None),
                            Err(note) => (Vec::new(), Some(note)),
                        };
                    }
                }
                // Wakes once a second to see the view's visibility, or at once on `wake`.
                std::thread::park_timeout(Duration::from_secs(1));
            }
        });
        self.slots.push(slot);
        self.workers.push(worker.thread().clone());
    }

    /// A handle for the sensor thread.
    pub(crate) fn merger(&self) -> Merger {
        Merger {
            slots: self.slots.clone(),
            demo: self.demo,
        }
    }

    /// The view came back on screen: poll now rather than within a second.
    pub(crate) fn wake(&self) {
        for worker in &self.workers {
            worker.unpark();
        }
    }
}

impl Drop for Feeds {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake();
    }
}

impl Merger {
    /// Add every current answer, and any source's trouble note, to a
    /// snapshot. An answer older than its source's freshness window (the
    /// view was off screen, a poll hung) is left out, so it is never drawn
    /// as current.
    pub(crate) fn merge(&self, snapshot: &mut Snapshot) {
        for slot in &self.slots {
            let Ok(latest) = slot.latest.lock() else {
                continue;
            };
            if latest.at.is_some_and(|at| at.elapsed() <= slot.fresh_for) {
                snapshot.extra.extend(latest.readings.iter().cloned());
                snapshot.notes.extend(latest.note.clone());
            }
        }
        if self.demo {
            demo_live(snapshot);
        }
    }
}

// ------------------------------------------------------------ demo

/// Live values of the demo's synthetic channels. The token rate sits far
/// above its two-hour band, so the anomaly glow can be seen without hardware.
pub(crate) fn demo_live(snapshot: &mut Snapshot) {
    snapshot.extra.extend([
        Reading {
            name: "LLM tok".into(),
            unit: "/s".into(),
            value: Some(96.0),
            full: None,
            log: false,
        },
        Reading {
            name: "LLM busy".into(),
            unit: String::new(),
            value: Some(2.0),
            full: None,
            log: false,
        },
    ]);
    snapshot.gpu_apps = vec![
        GpuApp {
            card: 0,
            pid: 2211,
            name: Some("python3".into()),
            mib: Some(3200.0),
        },
        GpuApp {
            card: 1,
            pid: 3307,
            name: Some("llama-server".into()),
            mib: Some(31_400.0),
        },
        GpuApp {
            card: 1,
            pid: 48_213,
            name: None,
            mib: Some(1500.0),
        },
    ];
}

/// The demo's two-hour history of an extra channel at `t` (0 = two hours
/// ago, 1 = now); `None` for a channel the demo does not invent.
pub(crate) fn demo_history(name: &str, t: f64, jitter: f64) -> Option<f64> {
    let job = (-((t - 0.38) / 0.08).powi(2)).exp();
    match name {
        "LLM tok" => Some(24.0 + 8.0 * jitter + 10.0 * job),
        "LLM busy" => Some((jitter * 1.6 + job).round()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const METRICS: &str = include_str!("../tests/fixtures/llama-metrics.txt");

    #[test]
    fn llama_metrics_are_read_by_name_and_the_rest_ignored() {
        let m = parse_llama(METRICS);
        assert_eq!(m.tokens_predicted_total, Some(33697.0));
        assert_eq!(m.requests_processing, Some(0.0));
        assert_eq!(m.requests_deferred, Some(0.0));
        let odd = "# llamacpp:requests_processing 9\nllamacpp:requests_processing{slot=\"1\"} 4\n\
                   llamacpp:requests_deferred NaN\nllamacpp:requests_processing 3\ngarbage\n";
        let m = parse_llama(odd);
        assert_eq!(m.requests_processing, Some(3.0));
        assert_eq!(m.requests_deferred, None);
        assert_eq!(m.tokens_predicted_total, None);
    }

    #[test]
    fn token_rate_is_the_counter_change_over_wall_time_and_resets_quietly() {
        let mut rate = TokenRate::default();
        let t0 = Instant::now();
        assert_eq!(rate.update(Some(1000.0), t0), None, "one sample is no rate");
        let r = rate
            .update(Some(1100.0), t0 + Duration::from_secs(4))
            .unwrap();
        assert!((r - 25.0).abs() < 1e-9);
        assert_eq!(
            rate.update(Some(10.0), t0 + Duration::from_secs(8)),
            None,
            "a restarted server's counter went backwards"
        );
        let r = rate
            .update(Some(30.0), t0 + Duration::from_secs(10))
            .unwrap();
        assert!((r - 10.0).abs() < 1e-9);
        assert_eq!(rate.update(None, t0 + Duration::from_secs(12)), None);
    }

    #[test]
    fn only_plain_http_urls_with_a_host_are_accepted() {
        let url = HttpUrl::parse("http://192.0.2.10:8085/metrics").unwrap();
        assert_eq!(url.to_string(), "http://192.0.2.10:8085/metrics");
        assert_eq!(
            HttpUrl::parse("http://llama").unwrap().to_string(),
            "http://llama:80/metrics"
        );
        assert_eq!(
            HttpUrl::parse("http://[::1]:9000/").unwrap().to_string(),
            "http://[::1]:9000/metrics"
        );
        for bad in [
            "https://host/metrics",
            "ftp://host",
            "host:8080/metrics",
            "http://",
            "http://:80/metrics",
            "http://user@host/metrics",
            "http://host:0/",
            "http://host:99999/",
            "http://host/me trics",
            "http://host/a\r\nX: y",
        ] {
            assert!(HttpUrl::parse(bad).is_err(), "{bad} should be refused");
        }
    }

    /// Serve one connection with `answer` (or nothing, held open) and return
    /// the URL and the request the client sent.
    fn serve_once(answer: Option<Vec<u8>>) -> (HttpUrl, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") && conn.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0]);
            }
            let _ = tx.send(String::from_utf8_lossy(&request).into_owned());
            match answer {
                Some(bytes) => {
                    let _ = conn.write_all(&bytes);
                    // Keep the connection open, as a keep-alive server would.
                    std::thread::sleep(Duration::from_secs(3));
                }
                None => std::thread::sleep(Duration::from_secs(5)),
            }
        });
        let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/metrics")).unwrap();
        (url, rx)
    }

    #[test]
    fn the_client_reads_a_metrics_page_from_a_local_server() {
        let page = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nKeep-Alive: timeout=5\r\nContent-Length: {}\r\n\r\n{METRICS}",
            METRICS.len()
        );
        let (url, request) = serve_once(Some(page.into_bytes()));
        let started = Instant::now();
        let body = http_get(&url, Duration::from_secs(2), HTTP_CAP).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "stops at Content-Length without waiting for the server to close"
        );
        assert_eq!(parse_llama(&body).tokens_predicted_total, Some(33697.0));
        let request = request.recv().unwrap();
        assert!(
            request.starts_with("GET /metrics HTTP/1.0\r\n"),
            "{request}"
        );
        assert!(request.contains("Connection: close"));
    }

    #[test]
    fn the_client_refuses_large_slow_failed_and_chunked_answers() {
        let big = format!(
            "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            HTTP_CAP * 2,
            "x".repeat(HTTP_CAP * 2)
        );
        let (url, _) = serve_once(Some(big.into_bytes()));
        let error = http_get(&url, Duration::from_secs(2), HTTP_CAP).unwrap_err();
        assert!(error.contains("KiB"), "{error}");

        let (url, _) = serve_once(None);
        let started = Instant::now();
        let error = http_get(&url, Duration::from_millis(400), HTTP_CAP).unwrap_err();
        assert_eq!(error, "timed out");
        assert!(started.elapsed() < Duration::from_secs(2));

        let (url, _) = serve_once(Some(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        ));
        assert_eq!(
            http_get(&url, Duration::from_secs(2), HTTP_CAP).unwrap_err(),
            "HTTP 404"
        );

        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let (url, _) = serve_once(Some(chunked.to_vec()));
        assert!(http_get(&url, Duration::from_secs(2), HTTP_CAP)
            .unwrap_err()
            .contains("chunked"));

        // Nothing listening: a refused connection is an error, not a hang.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        assert_eq!(
            http_get(&url, Duration::from_secs(2), HTTP_CAP).unwrap_err(),
            "cannot connect"
        );
    }

    #[test]
    fn gpu_processes_are_attributed_per_card_largest_first() {
        let csv = "4101, 30120, GPU-bbb\n4102, 900, GPU-aaa\n4103, [N/A], GPU-bbb\n\
                   4104, 2000, GPU-bbb\n4105, 100, GPU-bbb\n4106, 50, GPU-unknown\nnot a row\n";
        let apps = parse_compute_apps(csv);
        assert_eq!(apps.len(), 6);
        assert_eq!(apps[2], (4103, None, "GPU-bbb".to_string()));
        let uuids = parse_card_uuids("1, GPU-bbb\n0, GPU-aaa\n");
        assert_eq!(uuids, ["GPU-aaa", "GPU-bbb"]);
        let named = attribute(&apps, &uuids, |pid| {
            (pid != 4104).then(|| format!("p{pid}"))
        });
        let card1: Vec<u32> = named
            .iter()
            .filter(|a| a.card == 1)
            .map(|a| a.pid)
            .collect();
        assert_eq!(
            card1,
            [4101, 4104, 4105],
            "largest three; unknown memory sorts last"
        );
        assert_eq!(named.iter().find(|a| a.pid == 4102).unwrap().card, 0);
        assert!(
            named.iter().all(|a| a.pid != 4106),
            "an unknown card is left out"
        );

        let text = apps_text(&named, 1, 80);
        assert_eq!(text, "p4101 29G · pid 4104 2.0G · p4105 0.1G");
        assert_eq!(
            apps_text(&named, 1, 12),
            "p4101 29G",
            "only whole entries that fit"
        );
        assert_eq!(apps_text(&named, 1, 3), "");
        let hostile = [GpuApp {
            card: 0,
            pid: 7,
            name: Some("\u{1b}[31mevil\u{202e}".into()),
            mib: None,
        }];
        assert_eq!(apps_text(&hostile, 0, 40), "[31mevil");
    }

    #[test]
    fn a_pid_resolves_to_its_name_or_stays_a_pid() {
        let own = process_name(std::process::id()).expect("this test's own process");
        assert!(!own.is_empty() && !own.contains('\n'));
        assert_eq!(process_name(u32::MAX), None);
    }

    #[test]
    fn docker_stats_become_ridges_for_the_busiest_containers() {
        let text = r#"{"BlockIO":"6.13GB / 2.48GB","CPUPerc":"484.58%","ID":"55f8","MemPerc":"3.56%","MemUsage":"9.125GiB / 256GiB","Name":"agentbox","NetIO":"625MB / 172MB","PIDs":"3932"}
{"CPUPerc":"0.31%","MemUsage":"985.1MiB / 24GiB","Name":"openjev"}
{"CPUPerc":"12.5%","MemUsage":"2.672GiB / 16GiB","Name":"a-very-long-container-name"}
{"CPUPerc":"--","MemUsage":"0B / 0B","Name":"starting"}
{"CPUPerc":"1.0%","MemUsage":"12.3kB / 1GB","Name":"tiny"}
not json
"#;
        let stats = parse_docker_stats(text);
        assert_eq!(stats.len(), 4, "a row without a CPU figure is skipped");
        assert!((stats[0].mem_gib.unwrap() - 9.125).abs() < 1e-9);
        assert!((stats[1].mem_gib.unwrap() - 0.962).abs() < 0.001);
        let readings = container_readings(stats, 56);
        let names: Vec<&str> = readings.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "agentbox cpu",
                "agentbox mem",
                "a-very-l cpu",
                "a-very-l mem",
                "tiny cpu",
                "tiny mem"
            ]
        );
        assert_eq!(readings[0].full, Some(5600.0));
        assert_eq!(readings[1].full, Some(256.0));
        assert!(readings.iter().all(|r| columns(&r.name) <= NAME_COLUMNS));
    }

    #[test]
    fn bounded_commands_report_missing_failing_slow_and_large_output() {
        assert_eq!(
            run_bounded(&["sh", "-c", "echo hello"], Duration::from_secs(5), 64),
            Ok("hello\n".into())
        );
        assert_eq!(
            run_bounded(&["/nonexistent/docker"], Duration::from_secs(1), 64),
            Err(RunError::Missing)
        );
        assert_eq!(
            run_bounded(
                &["sh", "-c", "echo '\x1b[2Jpermission denied' >&2; exit 1"],
                Duration::from_secs(5),
                64
            ),
            Err(RunError::Failed("[2Jpermission denied".into()))
        );
        let started = Instant::now();
        assert_eq!(
            run_bounded(&["sleep", "10"], Duration::from_millis(300), 64),
            Err(RunError::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            run_bounded(&["sh", "-c", "yes"], Duration::from_secs(5), 4096),
            Err(RunError::TooLarge)
        );
    }

    #[test]
    fn readings_from_any_source_are_tidied_before_they_key_a_channel() {
        let reading = |name: &str, value: f64| Reading {
            name: name.into(),
            unit: "\u{1b}units!".into(),
            value: Some(value),
            full: Some(-1.0),
            log: false,
        };
        let mut snapshot = Snapshot {
            extra: vec![
                reading("  LLM   tok ", 3.0),
                reading("LLM tok", 4.0),
                reading("\u{202e}", 1.0),
                reading("a name far too long for a label", f64::NAN),
            ],
            ..Snapshot::default()
        };
        snapshot
            .extra
            .extend((0..100).map(|i| reading(&format!("r{i}"), 1.0)));
        tidy(&mut snapshot);
        assert!(snapshot.extra.len() <= MAX_READINGS);
        assert_eq!(snapshot.extra[0].name, "LLM tok");
        assert_eq!(snapshot.extra[0].unit, "unit");
        assert_eq!(snapshot.extra[0].full, None);
        assert_eq!(
            snapshot.extra[1].name, "a name far t",
            "the repeat and the blank are gone"
        );
        assert_eq!(snapshot.extra[1].value, None);
    }

    #[test]
    fn values_carry_their_unit_with_sensible_precision() {
        assert_eq!(format_value(96.0, "/s"), "96/s");
        assert_eq!(format_value(3.25, "G"), "3.2G");
        assert_eq!(format_value(2.0, ""), "2");
        assert_eq!(format_value(484.58, "%"), "485%");
    }
}
