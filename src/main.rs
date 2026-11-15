//! SystemScape — 3D system telemetry and agent work history in the terminal.
//!
//! `telemetry` draws one labelled ridge per channel over a two-hour window;
//! `activity` draws recorded agent work as a city of sessions. This module owns the terminal guard,
//! the row-diffing frame buffer and the helpers both views share.

pub(crate) use gemini_engine::{
    core::{ColChar, Colour, Modifier, Vec2D},
    mesh3d::{Face, Mesh3D, Transform3D, Vec3D},
    view3d::{DisplayMode, Viewport},
};
mod activity;
mod activity_data;
mod pixels;
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

/// Whether this pane is on screen at start-up. Terminals report focus only
/// when it changes, so a view launched in a background tmux window would
/// otherwise run at full rate until first visited. Outside tmux, or when tmux
/// will not report focus changes, assume focused.
fn starts_focused() -> bool {
    let Some(pane) = std::env::var_os("TMUX_PANE") else {
        return true;
    };
    let tmux = |args: &[&std::ffi::OsStr]| {
        std::process::Command::new("tmux")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let reports = tmux(&[
        "show-options".as_ref(),
        "-gv".as_ref(),
        "focus-events".as_ref(),
    ]);
    let active = tmux(&[
        "display-message".as_ref(),
        "-p".as_ref(),
        "-t".as_ref(),
        pane.as_ref(),
        "#{window_active}#{pane_active}".as_ref(),
    ]);
    !(reports.as_deref() == Some("on") && active.is_some_and(|state| state != "11"))
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("systemscape {}\n\nUsage: systemscape [--demo]\n       systemscape --activity [--demo] [--text|--json|--snapshot]\n                              [--home PATH] [--workspace PATH] [--archive PATH]\n\nTelemetry: one labelled 3D ridge per channel, GPUs split per card; older left, now right.\n           Space holds/resumes flight, arrows steer, +/- zoom, 0 reset, f flat view, q quits.\nActivity: a city of recorded work. District = project, tower = session, floor = action.\n          Space follows live work or holds, j/k session, Enter or f detail view (j/k record,\n          Enter source), [/] day, arrows steer, +/- zoom, 0 reset, h strip, ? sources, q quits.\nBoth views stop drawing while their terminal tab is not focused.\n\n  --demo     synthetic fixture; activity demo never opens agent histories\n  --text     bounded activity tree, also the default when stdout is redirected\n  --json     bounded activity records and coverage\n  --snapshot one 120x38 ANSI activity frame (requires a terminal)\n  --help     show this help\n  --version  print version", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("systemscape {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--activity") {
        if let Err(error) = activity::run(&args) {
            eprintln!("systemscape: {error}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(arg) = args.iter().skip(1).find(|a| a.as_str() != "--demo") {
        eprintln!("systemscape: unknown telemetry option {arg}; see --help");
        std::process::exit(2);
    }
    if let Err(error) = telemetry::run(args.iter().any(|a| a == "--demo")) {
        eprintln!("systemscape: {error}");
        std::process::exit(1);
    }
}
