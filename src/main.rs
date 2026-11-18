//! SystemScape — 3D system telemetry and agent work history in the terminal.
//!
//! `telemetry` draws one labelled ridge per channel over a two-hour window,
//! for this machine and any listed in a hosts file (`hosts`);
//! `activity` draws recorded agent work as a city of sessions. This module owns the terminal guard,
//! the row-diffing frame buffer and the helpers both views share.

pub(crate) use gemini_engine::{
    core::{ColChar, Colour, Modifier, Vec2D},
    mesh3d::{Face, Mesh3D, Transform3D, Vec3D},
    view3d::{DisplayMode, Viewport},
};
mod activity;
mod activity_data;
mod hosts;
mod insight;
mod mcp;
mod metrics;
mod pixels;
mod report;
mod sensors;
mod telemetry;

/// Restore terminal state on normal exit, errors and unwinding.
struct Terminal;
impl Terminal {
    fn enter() -> std::io::Result<Self> {
        use crossterm::{
            cursor::Hide,
            event::EnableFocusChange,
            execute,
            terminal::{enable_raw_mode, EnterAlternateScreen},
        };
        enable_raw_mode()?;
        let guard = Self;
        // Focus reports let a view stop working while its tab is in the background.
        execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            Hide,
            EnableFocusChange
        )?;
        Ok(guard)
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        use crossterm::{
            cursor::Show,
            event::DisableFocusChange,
            execute,
            terminal::{disable_raw_mode, LeaveAlternateScreen},
        };
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            DisableFocusChange,
            Show,
            LeaveAlternateScreen
        );
    }
}

/// Whether a view is on screen, and whether it has the keyboard.
///
/// In tmux, "visible" means the pane's window is the current window of an
/// attached session: a pane beside the active one in a split is visible but
/// not focused. tmux is asked every two seconds; focus reports from the
/// terminal update the state at once in between. Outside tmux a view is
/// always treated as visible, and focus reports only change its pace.
pub(crate) struct Sight {
    pane: Option<std::ffi::OsString>,
    visible: bool,
    focused: bool,
    checked: std::time::Instant,
}

impl Sight {
    pub(crate) fn new() -> Self {
        let mut sight = Self {
            pane: std::env::var_os("TMUX_PANE"),
            visible: true,
            focused: true,
            checked: std::time::Instant::now(),
        };
        sight.ask_tmux();
        sight
    }

    /// On screen: keep data current and draw it.
    pub(crate) fn visible(&self) -> bool {
        self.visible
    }

    /// On screen and receiving keys: animate at full rate.
    pub(crate) fn focused(&self) -> bool {
        self.visible && self.focused
    }

    /// Re-ask tmux when the last answer is two seconds old. Returns true
    /// when the view has just come back on screen.
    pub(crate) fn tick(&mut self) -> bool {
        if self.pane.is_none() || self.checked.elapsed() < std::time::Duration::from_secs(2) {
            return false;
        }
        let was = self.visible;
        self.ask_tmux();
        !was && self.visible
    }

    /// Apply a focus report. Returns true when the view has just come back on screen.
    pub(crate) fn focus(&mut self, gained: bool) -> bool {
        let was = self.visible;
        self.focused = gained;
        if gained {
            self.visible = true; // only a visible pane can take focus
        }
        !was && self.visible
    }

    fn ask_tmux(&mut self) {
        self.checked = std::time::Instant::now();
        let Some(pane) = &self.pane else { return };
        let answer = std::process::Command::new("tmux")
            .arg("display-message")
            .arg("-p")
            .arg("-t")
            .arg(pane)
            .arg("#{window_active} #{pane_active} #{session_attached}")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
        // If tmux cannot answer, keep the last state rather than guess.
        if let Some((visible, focused)) = answer.as_deref().and_then(parse_tmux_sight) {
            self.visible = visible;
            self.focused = focused;
        }
    }
}

/// Parse `"<window_active> <pane_active> <session_attached>"`.
fn parse_tmux_sight(answer: &str) -> Option<(bool, bool)> {
    let mut fields = answer.split_whitespace();
    let (window, pane, clients) = (fields.next()?, fields.next()?, fields.next()?);
    let visible = window == "1" && clients.parse::<u32>().ok()? > 0;
    Some((visible, visible && pane == "1"))
}

fn term_dims() -> (usize, usize) {
    let (w, h) = terminal_size::terminal_size()
        .map(|(tw, th)| (i64::from(tw.0), i64::from(th.0)))
        .unwrap_or((110, 32));
    (w.clamp(1, 512) as usize, (h - 1).clamp(1, 180) as usize)
}

/// Front/back row buffers avoid erasing the terminal between completed frames.
#[derive(Default)]
struct FrameBuffer {
    front: Vec<String>,
    dims: (usize, usize),
}
impl FrameBuffer {
    /// Write only the rows that changed, inside one synchronized update.
    fn present_rows(&mut self, back: Vec<String>, dims: (usize, usize)) -> std::io::Result<()> {
        use std::io::Write;
        let resized = self.dims != dims;
        let mut output = String::from("\x1b[?2026h");
        if resized {
            output.push_str("\x1b[0m\x1b[2J");
        }
        for (y, row) in back.iter().enumerate() {
            if resized || self.front.get(y) != Some(row) {
                output.push_str(&format!("\x1b[{};1H\x1b[0m{row}", y + 1));
            }
        }
        output.push_str("\x1b[0m\x1b[?2026l");
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(output.as_bytes())?;
        stdout.flush()?;
        self.front = back;
        self.dims = dims;
        Ok(())
    }
}

/// What a telemetry command line asks for.
#[derive(Debug, PartialEq)]
enum TelemetryMode {
    View {
        demo: bool,
        hosts: Option<std::path::PathBuf>,
        extras: sensors::Extras,
    },
    Probe {
        interval: std::time::Duration,
        gpu_interval: std::time::Duration,
    },
}

/// Parse the telemetry options (everything except `--activity` runs).
fn telemetry_mode(args: &[String]) -> Result<TelemetryMode, String> {
    let (mut demo, mut probe, mut hosts) = (false, false, None);
    let (mut interval, mut gpu_interval) = (None, None);
    let mut extras = sensors::Extras::default();
    let mut rest = args.iter().skip(1);
    let seconds = |flag: &str, value: Option<&String>| -> Result<f64, String> {
        value
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && (0.5..=600.0).contains(v))
            .ok_or_else(|| format!("{flag} needs a number of seconds from 0.5 to 600"))
    };
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--demo" => demo = true,
            "--probe" => probe = true,
            "--hosts" => {
                let path = rest.next().ok_or("--hosts needs a file path")?;
                hosts = Some(std::path::PathBuf::from(path));
            }
            "--interval" => interval = Some(seconds(arg, rest.next())?),
            "--gpu-interval" => gpu_interval = Some(seconds(arg, rest.next())?),
            "--containers" => extras.containers = true,
            "--llama-metrics" => {
                let url = rest.next().ok_or("--llama-metrics needs an http:// URL")?;
                extras.llama = Some(sensors::HttpUrl::parse(url)?);
            }
            other => return Err(format!("unknown telemetry option {other}; see --help")),
        }
    }
    if probe {
        if demo || hosts.is_some() || extras != sensors::Extras::default() {
            return Err("--probe takes only --interval, --gpu-interval and --record".into());
        }
        let interval = interval.unwrap_or(2.0);
        let gpu_interval = gpu_interval.unwrap_or(10.0).max(interval);
        return Ok(TelemetryMode::Probe {
            interval: std::time::Duration::from_secs_f64(interval),
            gpu_interval: std::time::Duration::from_secs_f64(gpu_interval),
        });
    }
    if interval.is_some() || gpu_interval.is_some() {
        return Err(
            "--interval and --gpu-interval apply to --probe; set a host's in its hosts file".into(),
        );
    }
    if demo && hosts.is_some() {
        return Err("--demo is synthetic and cannot be combined with --hosts".into());
    }
    if demo && extras != sensors::Extras::default() {
        return Err("--demo is synthetic and runs neither docker nor network requests".into());
    }
    Ok(TelemetryMode::View {
        demo,
        hosts,
        extras,
    })
}

/// Modes other than telemetry: the first flag present on the command line
/// picks the mode, which then parses the whole command line itself. A new
/// mode is one line here.
type Mode = fn(&[String]) -> std::io::Result<()>;
const MODES: &[(&str, Mode)] = &[
    // Insight outputs come before `--activity`, which they also accept.
    ("--summary", report::summary),
    ("--status", report::status),
    ("--mcp", mcp::run),
    ("--metrics", metrics::run),
    ("--activity", activity::run),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("systemscape {}\n\nUsage: systemscape [--demo | --hosts FILE]\n       systemscape --probe [--interval SECS] [--gpu-interval SECS]\n       systemscape --activity [--demo] [--text|--json|--snapshot]\n                              [--home PATH] [--workspace PATH] [--archive PATH]\n\nTelemetry: one labelled 3D ridge per channel, GPUs split per card; older left, now right.\n           Space holds/resumes flight, arrows steer, +/- zoom, 0 reset, f flat view, q quits.\n           With a hosts file, other machines appear too: Tab or 1-9 picks the host shown.\nActivity: a city of recorded work. District = project, tower = session, floor = action.\n          Space follows live work or holds, j/k session, Enter or f detail view (j/k record,\n          Enter source), [/] day, arrows steer, +/- zoom, 0 reset, h strip, ? sources, q quits.\nBoth views pause while their tmux window is not on screen, and slow down while visible but unfocused.\n\n  --demo     synthetic fixture; activity demo never opens agent histories\n  --hosts    hosts file to watch (default $XDG_CONFIG_HOME/systemscape/hosts.toml, if present)\n  --probe    print this machine's readings as JSON lines, for a viewer elsewhere\n  --interval / --gpu-interval  probe sensor and nvidia-smi periods (default 2 and 10 s)\n  --text     bounded activity tree, also the default when stdout is redirected\n  --json     bounded activity records and coverage\n  --snapshot one 120x38 ANSI activity frame (requires a terminal)\n  --help     show this help\n  --version  print version", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("systemscape {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Some((_, run)) = MODES
        .iter()
        .find(|(flag, _)| args.iter().any(|a| a == flag))
    {
        if let Err(error) = run(&args) {
            eprintln!("systemscape: {error}");
            std::process::exit(1);
        }
        return;
    }
    let (args, join) = match telemetry::join::split_flags(&args) {
        Ok(split) => split,
        Err(error) => {
            eprintln!("systemscape: {error}");
            std::process::exit(2);
        }
    };
    let result = match telemetry_mode(&args) {
        Err(error) => {
            eprintln!("systemscape: {error}");
            std::process::exit(2);
        }
        Ok(TelemetryMode::Probe {
            interval,
            gpu_interval,
        }) => match telemetry::probe(interval, gpu_interval, join.record) {
            // The viewer went away: that is how a probe is meant to end.
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            other => other,
        },
        Ok(TelemetryMode::View {
            demo,
            hosts,
            extras,
        }) => telemetry::run(telemetry::Options {
            demo,
            hosts,
            extras,
            join,
        }),
    };
    if let Err(error) = result {
        eprintln!("systemscape: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tmux_sight_separates_visible_from_focused() {
        use super::parse_tmux_sight as parse;
        assert_eq!(parse("1 1 1\n"), Some((true, true)));
        assert_eq!(
            parse("1 0 2"),
            Some((true, false)),
            "split pane beside the active one"
        );
        assert_eq!(
            parse("0 1 1"),
            Some((false, false)),
            "another window is current"
        );
        assert_eq!(parse("1 1 0"), Some((false, false)), "no client attached");
        assert_eq!(parse("garbage"), None);
    }

    #[test]
    fn telemetry_flags_parse_and_conflicts_are_refused() {
        use super::{telemetry_mode as mode, TelemetryMode};
        use std::time::Duration;
        let argv = |line: &str| {
            std::iter::once("systemscape")
                .chain(line.split_whitespace())
                .map(String::from)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            mode(&argv("")),
            Ok(TelemetryMode::View {
                demo: false,
                hosts: None,
                extras: Default::default(),
            })
        );
        assert_eq!(
            mode(&argv("--hosts /etc/h.toml")),
            Ok(TelemetryMode::View {
                demo: false,
                hosts: Some("/etc/h.toml".into()),
                extras: Default::default(),
            })
        );
        assert_eq!(
            mode(&argv("--probe --interval 5")),
            Ok(TelemetryMode::Probe {
                interval: Duration::from_secs(5),
                gpu_interval: Duration::from_secs(10)
            })
        );
        assert_eq!(
            mode(&argv("--probe --interval 20 --gpu-interval 3")),
            Ok(TelemetryMode::Probe {
                interval: Duration::from_secs(20),
                gpu_interval: Duration::from_secs(20)
            }),
            "the GPU is never asked more often than the sensors"
        );
        for bad in [
            "--hosts",
            "--probe --demo",
            "--probe --hosts x",
            "--interval 2",
            "--demo --hosts x",
            "--probe --interval 0",
            "--probe --interval soon",
            "--bogus",
        ] {
            assert!(mode(&argv(bad)).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn sensor_flags_parse_and_senseless_combinations_are_refused() {
        use super::{sensors, telemetry_mode as mode, TelemetryMode};
        let argv = |line: &str| {
            std::iter::once("systemscape")
                .chain(line.split_whitespace())
                .map(String::from)
                .collect::<Vec<_>>()
        };
        let url = sensors::HttpUrl::parse("http://127.0.0.1:8085/metrics").unwrap();
        assert_eq!(
            mode(&argv(
                "--containers --llama-metrics http://127.0.0.1:8085/metrics --hosts h.toml"
            )),
            Ok(TelemetryMode::View {
                demo: false,
                hosts: Some("h.toml".into()),
                extras: sensors::Extras {
                    containers: true,
                    llama: Some(url),
                },
            })
        );
        for bad in [
            "--llama-metrics",
            "--llama-metrics https://host/metrics",
            "--llama-metrics host:8085",
            "--probe --containers",
            "--probe --llama-metrics http://host/metrics",
            "--demo --containers",
            "--demo --llama-metrics http://host/metrics",
        ] {
            assert!(mode(&argv(bad)).is_err(), "{bad} should be refused");
        }
    }
}
