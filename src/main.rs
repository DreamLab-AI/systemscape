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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("systemscape {}\n\nUsage: systemscape [--demo]\n       systemscape --activity [--demo] [--text|--json|--snapshot]\n                              [--home PATH] [--workspace PATH] [--archive PATH]\n\nTelemetry: one labelled 3D ridge per channel, GPUs split per card; older left, now right.\n           Space holds/resumes flight, arrows steer, +/- zoom, 0 reset, f flat view, q quits.\nActivity: a city of recorded work. District = project, tower = session, floor = action.\n          Space follows live work or holds, j/k session, Enter or f detail view (j/k record,\n          Enter source), [/] day, arrows steer, +/- zoom, 0 reset, h strip, ? sources, q quits.\nBoth views pause while their tmux window is not on screen, and slow down while visible but unfocused.\n\n  --demo     synthetic fixture; activity demo never opens agent histories\n  --text     bounded activity tree, also the default when stdout is redirected\n  --json     bounded activity records and coverage\n  --snapshot one 120x38 ANSI activity frame (requires a terminal)\n  --help     show this help\n  --version  print version", env!("CARGO_PKG_VERSION"));
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
}
