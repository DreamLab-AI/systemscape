//! `systemscape --metrics ADDR`: an opt-in OpenMetrics endpoint for this
//! machine's sensor readings and the activity counts `--status` reports.
//!
//! It serves `GET /metrics` and nothing else, one connection at a time, with
//! request size and time limits. It binds loopback addresses only, unless
//! `--metrics-allow-remote` is also given. All work happens per scrape: one
//! bounded collector poll and one sensor poll (nvidia-smi only with `--gpu`,
//! at most every ten seconds). It runs headless until interrupted.
use crate::insight;
use crate::report::{invalid, Reading, Sensors, SourceFlags, Sources};
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// A request head longer than this is refused.
const REQUEST_LIMIT: usize = 8 * 1024;
/// A client has this long to send its request and to take the reply, so a
/// stalled client holds up the next scrape for about a second at most.
const IO_TIMEOUT: Duration = Duration::from_secs(1);
const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// The first scrape keeps polling for up to this long, so it covers the
/// whole retained history rather than one bounded poll's share.
const FIRST_READ: Duration = Duration::from_secs(3);

/// What one scrape reads.
pub(crate) struct Exporter {
    sources: Sources,
    /// Whether the history has been read to the end once.
    drained: bool,
    sensors: Sensors,
    gpu: bool,
}

/// One metric family: name, type, unit (empty for none), help and its samples.
struct Family<'a> {
    name: &'a str,
    unit: &'a str,
    help: &'a str,
    samples: Vec<(Vec<(&'a str, String)>, f64)>,
}

/// Escape a label value as OpenMetrics requires.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Render gauge families as OpenMetrics text, skipping empty families and
/// non-finite values, ending with `# EOF`.
fn render(families: &[Family]) -> String {
    let mut out = String::new();
    for family in families {
        let samples: Vec<_> = family
            .samples
            .iter()
            .filter(|(_, v)| v.is_finite())
            .collect();
        if samples.is_empty() {
            continue;
        }
        let _ = writeln!(out, "# TYPE {} gauge", family.name);
        if !family.unit.is_empty() {
            let _ = writeln!(out, "# UNIT {} {}", family.name, family.unit);
        }
        let _ = writeln!(out, "# HELP {} {}", family.name, family.help);
        for (labels, value) in samples {
            let labels = if labels.is_empty() {
                String::new()
            } else {
                let pairs: Vec<_> = labels
                    .iter()
                    .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
                    .collect();
                format!("{{{}}}", pairs.join(","))
            };
            let _ = writeln!(out, "{}{labels} {value}", family.name);
        }
    }
    out.push_str("# EOF\n");
    out
}

/// The families for one reading and one activity poll.
fn families(
    reading: Option<&Reading>,
    pulse: insight::Pulse,
    kinds: &[(String, usize)],
    contended: usize,
) -> Vec<Family<'static>> {
    let one = |v: Option<f64>| v.map(|v| (vec![], v)).into_iter().collect::<Vec<_>>();
    let mut out = vec![];
    if let Some(r) = reading {
        let temps = [
            ("cpu", r.cpu_temp_c),
            ("disk", r.disk_temp_c),
            ("chipset", r.chipset_temp_c),
            ("nic", r.nic_temp_c),
        ];
        let card = |f: fn(&crate::report::Card) -> Option<f64>, scale: f64| {
            r.gpus
                .iter()
                .enumerate()
                .filter_map(|(i, g)| f(g).map(|v| (vec![("gpu", i.to_string())], v * scale)))
                .collect::<Vec<_>>()
        };
        out.extend([
            Family {
                name: "systemscape_temperature_celsius",
                unit: "celsius",
                help: "Temperature by sensor; cpu is the hottest package or core.",
                samples: temps
                    .iter()
                    .filter_map(|(s, v)| v.map(|v| (vec![("sensor", (*s).to_owned())], v)))
                    .collect(),
            },
            Family {
                name: "systemscape_power_watts",
                unit: "watts",
                help: "Power meter reading.",
                samples: one(r.power_w),
            },
            Family {
                name: "systemscape_cpu_busy_ratio",
                unit: "ratio",
                help: "CPU busy share since the previous scrape.",
                samples: one(r.cpu_pct.map(|p| p / 100.0)),
            },
            Family {
                name: "systemscape_memory_used_ratio",
                unit: "ratio",
                help: "Memory in use, from MemAvailable.",
                samples: one(r.mem_pct.map(|p| p / 100.0)),
            },
            Family {
                name: "systemscape_disk_bytes_per_second",
                unit: "",
                help: "Disk read and write rate since the previous scrape.",
                samples: one(r.disk_mbps.map(|m| m * 1e6)),
            },
            Family {
                name: "systemscape_network_bytes_per_second",
                unit: "",
                help: "Network receive and transmit rate since the previous scrape.",
                samples: one(r.net_mbps.map(|m| m * 1e6)),
            },
            Family {
                name: "systemscape_gpu_temperature_celsius",
                unit: "celsius",
                help: "GPU temperature by card.",
                samples: card(|g| g.temp_c, 1.0),
            },
            Family {
                name: "systemscape_gpu_busy_ratio",
                unit: "ratio",
                help: "GPU utilisation by card.",
                samples: card(|g| g.util_pct, 0.01),
            },
            Family {
                name: "systemscape_gpu_memory_used_bytes",
                unit: "bytes",
                help: "GPU memory in use by card.",
                samples: card(|g| g.used_gib, 1024.0 * 1024.0 * 1024.0),
            },
            Family {
                name: "systemscape_gpu_memory_total_bytes",
                unit: "bytes",
                help: "GPU memory size by card.",
                samples: card(|g| g.total_gib, 1024.0 * 1024.0 * 1024.0),
            },
        ]);
    }
    out.extend([
        Family {
            name: "systemscape_activity_sessions",
            unit: "",
            help: "Sessions by state now, each counted once: stuck, then error, then active.",
            samples: [
                ("active", pulse.active),
                ("error", pulse.errors),
                ("stuck", pulse.stuck),
            ]
            .into_iter()
            .map(|(state, n)| (vec![("state", state.to_owned())], n as f64))
            .collect(),
        },
        Family {
            name: "systemscape_activity_records",
            unit: "",
            help:
                "Retained records of the current UTC day by kind; a gauge, as history is bounded.",
            samples: kinds
                .iter()
                .map(|(kind, n)| (vec![("kind", kind.clone())], *n as f64))
                .collect(),
        },
        Family {
            name: "systemscape_activity_contended_files",
            unit: "",
            help: "Files edited by more than one session on the current UTC day.",
            samples: vec![(vec![], contended as f64)],
        },
    ]);
    out
}

/// The kinds the collector emits; anything else is counted as `event`.
const KINDS: [&str; 5] = ["prompt", "tool", "failure", "receipt", "event"];

impl Exporter {
    pub(crate) fn new(sources: Sources, gpu: bool) -> Self {
        Self {
            sources,
            sensors: Sensors::default(),
            gpu,
            drained: false,
        }
    }

    /// One scrape's body. The demo has no telemetry, only activity.
    pub(crate) fn scrape(&mut self) -> String {
        let reading = (!self.sources.demo()).then(|| self.sensors.read(self.gpu));
        // Drain on the first scrape; afterwards one bounded poll reads what was appended.
        let (events, _) = if std::mem::replace(&mut self.drained, true) {
            self.sources.load()
        } else {
            self.sources.load_drained(FIRST_READ)
        };
        let now = chrono::Utc::now();
        let today = now.format("%Y-%m-%d").to_string();
        let found = insight::kinds(&events, &today);
        // A fixed label set keeps record text out of label values.
        let mut kinds: Vec<(String, usize)> = KINDS.iter().map(|k| ((*k).to_owned(), 0)).collect();
        for (kind, n) in found {
            let slot = KINDS
                .iter()
                .position(|k| *k == kind)
                .unwrap_or(KINDS.len() - 1);
            kinds[slot].1 += n;
        }
        let contended = insight::contended(&events, &today).len();
        render(&families(
            reading.as_ref(),
            insight::pulse(&events, now),
            &kinds,
            contended,
        ))
    }
}

/// Resolve `addr` and refuse anything but loopback unless `remote` is set.
fn resolve(addr: &str, remote: bool) -> io::Result<Vec<SocketAddr>> {
    let addrs: Vec<SocketAddr> = addr
        .to_socket_addrs()
        .map_err(|e| invalid(format!("--metrics {addr}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(invalid(format!("--metrics {addr} resolves to no address")));
    }
    if !remote {
        if let Some(open) = addrs.iter().find(|a| !a.ip().is_loopback()) {
            return Err(invalid(format!(
                "--metrics refuses to listen on {open}, which is not loopback; add --metrics-allow-remote to serve the network"
            )));
        }
    }
    Ok(addrs)
}

fn bind(addr: &str, remote: bool) -> io::Result<TcpListener> {
    TcpListener::bind(&resolve(addr, remote)?[..])
}

/// Read a request head (up to the blank line) within the size and time
/// limits. `Err(status line)` when the request must be refused.
fn read_head(stream: &mut TcpStream) -> Result<String, &'static str> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && !head.windows(2).any(|w| w == b"\n\n") {
        if head.len() > REQUEST_LIMIT {
            return Err("431 Request Header Fields Too Large");
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return Err("408 Request Timeout");
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err("408 Request Timeout"),
        }
    }
    String::from_utf8(head).map_err(|_| "400 Bad Request")
}

/// Who may be served: the bound address, and whether any `Host` is accepted.
#[derive(Clone, Copy)]
struct Gate {
    bound: SocketAddr,
    remote: bool,
}

impl Gate {
    /// Does the request's `Host` name this server? Without
    /// `--metrics-allow-remote`, only a loopback name or the bound address
    /// (with or without a port) is accepted, which stops a web page from
    /// reading the endpoint through DNS rebinding.
    fn admits(&self, head: &str) -> bool {
        if self.remote {
            return true;
        }
        let host = head.lines().skip(1).find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("host")
                .then(|| value.trim().to_ascii_lowercase())
        });
        let Some(host) = host else { return false };
        // `[v6]:port`, `[v6]`, `name:port` or `name`; a port must be digits.
        let (name, port) = match host.strip_prefix('[') {
            Some(rest) => match rest.split_once(']') {
                Some((v6, tail)) => (format!("[{v6}]"), tail.strip_prefix(':')),
                None => return false,
            },
            None => match host.split_once(':') {
                Some((name, port)) => (name.to_owned(), Some(port)),
                None => (host.clone(), None),
            },
        };
        if port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
            return false;
        }
        let bound = match self.bound.ip() {
            std::net::IpAddr::V4(ip) => ip.to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        };
        ["localhost", "127.0.0.1", "[::1]", bound.as_str()].contains(&name.as_str())
    }
}

/// Pick the response for a request head: `(status, extra headers, body)`.
fn route(head: &str, gate: Gate, exporter: &mut Exporter) -> (&'static str, &'static str, String) {
    if !head.lines().next().unwrap_or("").trim().is_empty() && !gate.admits(head) {
        return (
            "421 Misdirected Request",
            "",
            "the Host header must name this loopback endpoint\n".into(),
        );
    }
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");
    match (method, path) {
        ("", _) | (_, "") => ("400 Bad Request", "", "bad request\n".into()),
        ("GET", "/metrics") => ("200 OK", CONTENT_TYPE, exporter.scrape()),
        ("GET", _) => ("404 Not Found", "", "only /metrics is served\n".into()),
        _ => ("405 Method Not Allowed", "", "only GET is served\n".into()),
    }
}

fn answer(stream: &mut TcpStream, gate: Gate, exporter: &mut Exporter) -> io::Result<()> {
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let (status, content_type, body) = match read_head(stream) {
        Ok(head) => route(&head, gate, exporter),
        Err(status) => (status, "", format!("{status}\n")),
    };
    let content_type = if content_type.is_empty() {
        "text/plain; charset=utf-8"
    } else {
        content_type
    };
    let allow = if status.starts_with("405") {
        "Allow: GET\r\n"
    } else {
        ""
    };
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{allow}Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(reply.as_bytes())?;
    stream.flush()
}

/// Answer connections one at a time; `limit` stops after that many (for tests).
fn serve(
    listener: &TcpListener,
    remote: bool,
    exporter: &mut Exporter,
    limit: Option<usize>,
) -> io::Result<()> {
    let gate = Gate {
        bound: listener.local_addr()?,
        remote,
    };
    for (served, stream) in listener.incoming().enumerate() {
        // A client that misbehaves only costs its own connection.
        if let Ok(mut stream) = stream {
            let _ = answer(&mut stream, gate, exporter);
        }
        if limit.is_some_and(|limit| served + 1 >= limit) {
            break;
        }
    }
    Ok(())
}

/// `systemscape --metrics ADDR [--metrics-allow-remote] [--gpu]` with the usual source flags.
pub fn run(args: &[String]) -> io::Result<()> {
    let mut sources = SourceFlags::new();
    let (mut addr, mut remote, mut gpu) = (None, false, false);
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if sources.take(arg, &mut rest)? {
            continue;
        }
        match arg.as_str() {
            "--metrics" => {
                let value = rest
                    .next()
                    .ok_or_else(|| invalid("--metrics needs an address such as 127.0.0.1:9464"))?;
                addr = Some(value.clone());
            }
            "--metrics-allow-remote" => remote = true,
            "--gpu" => gpu = true,
            "--activity" => {}
            other => {
                return Err(invalid(format!(
                    "unknown --metrics option {other}; see --help"
                )))
            }
        }
    }
    let addr = addr.ok_or_else(|| invalid("--metrics needs an address such as 127.0.0.1:9464"))?;
    let listener = bind(&addr, remote)?;
    eprintln!(
        "systemscape: serving OpenMetrics on http://{}/metrics until interrupted",
        listener.local_addr()?
    );
    serve(
        &listener,
        remote,
        &mut Exporter::new(sources.open(), gpu),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Shutdown;

    fn demo() -> Exporter {
        let mut flags = SourceFlags::new();
        flags.demo = true;
        Exporter::new(flags.open(), false)
    }

    /// Send raw bytes to a one-connection server on a free loopback port.
    fn exchange(request: &[u8]) -> String {
        exchange_as(request, false)
    }

    fn exchange_as(request: &[u8], remote: bool) -> String {
        let listener = bind("127.0.0.1:0", false).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || serve(&listener, remote, &mut demo(), Some(1)));
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        // The server may answer and close before an over-long request is all sent.
        let _ = client.write_all(request);
        let _ = client.shutdown(Shutdown::Write);
        let mut reply = String::new();
        let _ = client.read_to_string(&mut reply);
        server.join().unwrap().unwrap();
        reply
    }

    #[test]
    fn a_scrape_over_loopback_returns_openmetrics_ending_in_eof() {
        let reply = exchange(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let (head, body) = reply.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK"));
        assert!(head.contains(CONTENT_TYPE));
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
        assert!(body.ends_with("# EOF\n"));
        assert!(body.contains("# TYPE systemscape_activity_sessions gauge"));
        assert!(body.contains("systemscape_activity_sessions{state=\"stuck\"} 1"));
        assert!(body.contains("systemscape_activity_records{kind=\"tool\"}"));
        assert!(
            !body.contains("systemscape_temperature_celsius"),
            "the demo has no telemetry"
        );
    }

    #[test]
    fn other_paths_methods_and_oversized_requests_are_refused() {
        assert!(exchange(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").starts_with("HTTP/1.1 404"));
        assert!(
            exchange(b"GET /metrics/../etc HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .starts_with("HTTP/1.1 404")
        );
        let post = exchange(b"POST /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(post.starts_with("HTTP/1.1 405") && post.contains("Allow: GET"));
        assert!(exchange(b"\r\n\r\n").starts_with("HTTP/1.1 400"));
        let mut huge = b"GET /metrics HTTP/1.1\r\nX: ".to_vec();
        huge.extend(std::iter::repeat_n(b'a', REQUEST_LIMIT * 2));
        assert!(exchange(&huge).starts_with("HTTP/1.1 431"));
        assert!(
            exchange(b"GET /metrics?x=1 HTTP/1.0\nhost: LOCALHOST:9464\n\n")
                .starts_with("HTTP/1.1 200")
        );
    }

    #[test]
    fn only_loopback_host_names_are_served_unless_remote_is_allowed() {
        let get = |host: &str| format!("GET /metrics HTTP/1.1\r\nHost: {host}\r\n\r\n");
        for host in [
            "localhost",
            "127.0.0.1:9464",
            "[::1]",
            "[::1]:80",
            "LocalHost",
        ] {
            assert!(
                exchange(get(host).as_bytes()).starts_with("HTTP/1.1 200"),
                "{host}"
            );
        }
        for host in [
            "evil.example",
            "evil.example:9464",
            "localhost.evil.example",
            "localhost:",
            "localhost:x",
            "[::1",
        ] {
            assert!(
                exchange(get(host).as_bytes()).starts_with("HTTP/1.1 421"),
                "{host}"
            );
        }
        let no_host = exchange(b"GET /metrics HTTP/1.1\r\n\r\n");
        assert!(
            no_host.starts_with("HTTP/1.1 421"),
            "a missing Host is refused"
        );
        assert!(exchange_as(get("evil.example").as_bytes(), true).starts_with("HTTP/1.1 200"));
        let bound = Gate {
            bound: "192.0.2.7:9464".parse().unwrap(),
            remote: false,
        };
        assert!(bound.admits("GET / HTTP/1.1\r\nHost: 192.0.2.7:9464\r\n"));
        assert!(!bound.admits("GET / HTTP/1.1\r\nHost: 192.0.2.8\r\n"));
    }

    #[test]
    fn a_silent_client_holds_the_next_scrape_for_about_a_second() {
        let listener = bind("127.0.0.1:0", false).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || serve(&listener, false, &mut demo(), Some(2)));
        let started = Instant::now();
        let mut silent = TcpStream::connect(addr).unwrap();
        let mut scraper = TcpStream::connect(addr).unwrap();
        scraper
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        scraper
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reply = String::new();
        let _ = scraper.read_to_string(&mut reply);
        let waited = started.elapsed();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(waited < Duration::from_millis(2500), "{waited:?}");
        let mut refused = String::new();
        let _ = silent.read_to_string(&mut refused);
        assert!(refused.starts_with("HTTP/1.1 408"), "{refused}");
        server.join().unwrap().unwrap();
    }

    #[test]
    fn the_first_scrape_reads_the_whole_history() {
        let (dir, sources) = crate::mcp::tests::large_history(12);
        let mut exporter = Exporter::new(sources, false);
        let body = exporter.scrape();
        assert!(
            body.contains("systemscape_activity_records{kind=\"prompt\"} 12\n"),
            "{body}"
        );
        assert!(exporter.scrape().contains("{kind=\"prompt\"} 12\n"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_loopback_binds_without_the_second_flag() {
        assert!(bind("127.0.0.1:0", false).is_ok());
        let refused = bind("0.0.0.0:0", false).unwrap_err();
        assert!(refused.to_string().contains("--metrics-allow-remote"));
        assert!(bind("[::]:0", false).is_err());
        assert!(bind("not an address", false).is_err());
        assert!(
            resolve("0.0.0.0:0", true).is_ok(),
            "the second flag opens it"
        );
    }

    #[test]
    fn families_carry_units_escape_labels_and_skip_what_was_not_read() {
        let reading = Reading {
            cpu_temp_c: Some(71.5),
            power_w: Some(f64::NAN),
            gpus: vec![crate::report::Card {
                temp_c: Some(60.0),
                util_pct: Some(50.0),
                ..Default::default()
            }],
            ..Reading::default()
        };
        let text = render(&families(
            Some(&reading),
            insight::Pulse::default(),
            &[("we\"ird\\\n".to_owned(), 2)],
            0,
        ));
        assert!(text.contains("# UNIT systemscape_temperature_celsius celsius"));
        assert!(text.contains("systemscape_temperature_celsius{sensor=\"cpu\"} 71.5"));
        assert!(text.contains("systemscape_gpu_busy_ratio{gpu=\"0\"} 0.5"));
        assert!(
            !text.contains("systemscape_power_watts"),
            "NaN is not a reading"
        );
        assert!(!text.contains("systemscape_gpu_memory_used_bytes"));
        assert!(text.contains("kind=\"we\\\"ird\\\\\\n\"} 2"));
        assert_eq!(text.matches("# EOF").count(), 1);
    }
}
