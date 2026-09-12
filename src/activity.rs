//! Interactive work landscape: time runs left to right; agent lanes recede in Z.
//! The selected record is white and its original source is always available.
use crate::activity_data::{Collector, Event};
use crate::{
    make_view, push_bar, Colour, DisplayMode, Mesh3D, Modifier, Transform3D, Vec2D, Vec3D, View,
    Viewport,
};
use crossterm::event::{self, Event as Input, KeyCode, KeyEventKind, KeyModifiers};
use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MAX_NODES: usize = 128;
const MAX_LANES: usize = 8;
const POLL: Duration = Duration::from_secs(2);

#[derive(Default)]
struct State {
    selected: usize,
    day: usize,
    lane_page: usize,
    record_page: usize,
    yaw: f64,
    pitch: f64,
    zoom: f64,
    tour: bool,
    tour_elapsed: f64,
    dwell: f64,
    focus: Vec3D,
    camera_x: f64,
    flat: bool,
    detail: bool,
}

struct Slice<'a> {
    records: Vec<&'a Event>,
    lanes: Vec<String>,
    day: String,
    days: usize,
    lane_pages: usize,
    total: usize,
    record_pages: usize,
}

fn slice<'a>(events: &'a [Event], state: &mut State) -> Slice<'a> {
    let days: Vec<_> = events
        .iter()
        .filter_map(|e| e.at.get(..10))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .rev()
        .collect();
    state.day = state.day.min(days.len().saturating_sub(1));
    let day = days.get(state.day).copied().unwrap_or("No history");
    let dated: Vec<_> = events.iter().filter(|e| e.at.starts_with(day)).collect();
    let all_lanes: Vec<_> = dated
        .iter()
        .map(|e| e.agent.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let lane_pages = all_lanes.len().div_ceil(MAX_LANES).max(1);
    state.lane_page = state.lane_page.min(lane_pages - 1);
    let lanes: Vec<_> = all_lanes
        .into_iter()
        .skip(state.lane_page * MAX_LANES)
        .take(MAX_LANES)
        .collect();
    let mut records: Vec<_> = dated
        .into_iter()
        .filter(|e| lanes.contains(&e.agent))
        .collect();
    records.sort_by(|a, b| a.at.cmp(&b.at).then(a.key.cmp(&b.key)));
    let total = records.len();
    let record_pages = total.div_ceil(MAX_NODES).max(1);
    state.record_page = state.record_page.min(record_pages - 1);
    let end = total.saturating_sub(state.record_page * MAX_NODES);
    let start = end.saturating_sub(MAX_NODES);
    records = records[start..end].to_vec();
    state.selected = state.selected.min(records.len().saturating_sub(1));
    Slice {
        records,
        lanes,
        day: day.into(),
        days: days.len(),
        lane_pages,
        total,
        record_pages,
    }
}

/// Move between every retained page; never silently stay on just the newest lane.
fn advance_selection(events: &[Event], state: &mut State) {
    let data = slice(events, state);
    if state.selected + 1 < data.records.len() {
        state.selected += 1;
    } else {
        state.selected = 0;
        if state.record_page + 1 < data.record_pages {
            state.record_page += 1;
        } else {
            state.record_page = 0;
            if state.lane_page + 1 < data.lane_pages {
                state.lane_page += 1;
            } else {
                state.lane_page = 0;
                state.day = (state.day + 1) % data.days.max(1);
            }
        }
    }
}

/// Slow dolly + sweep, easing towards the selected action instead of snapping.
fn tour_step(events: &[Event], state: &mut State, dt: f64) {
    if !state.tour || events.is_empty() {
        return;
    }
    let dt = dt.clamp(0.0, 1.0);
    state.tour_elapsed += dt;
    state.dwell += dt;
    if state.dwell >= 4.0 {
        state.dwell = 0.0;
        advance_selection(events, state);
    }
    let data = slice(events, state);
    let mut target = Vec3D::new(0.0, 0.5, 0.0);
    if let Some(record) = data.records.get(state.selected) {
        let first = data.records.first().map_or(0.0, |e| seconds(&e.at));
        let last = data.records.last().map_or(first, |e| seconds(&e.at));
        let x = if last > first {
            8.5 - 17.0 * (seconds(&record.at) - first) / (last - first)
        } else {
            0.0
        };
        let lane = data
            .lanes
            .iter()
            .position(|a| a == &record.agent)
            .unwrap_or(0);
        target = Vec3D::new(x * 0.3, 0.5, lane_z(lane, data.lanes.len()) * 0.3);
    }
    let ease = 1.0 - (-dt * 1.2).exp();
    state.focus = state.focus.lerp(target, ease);
    let phase = state.tour_elapsed / 18.0;
    state.yaw += ((-0.2 + phase.sin() * 0.42) - state.yaw) * ease;
    state.pitch += ((phase.mul_add(0.7, 0.0).sin() * 6.0) - state.pitch) * ease;
    state.zoom += ((46.0 + (phase * 0.6).sin() * 5.0) - state.zoom) * ease;
    state.camera_x += ((phase.cos() * 3.0) - state.camera_x) * ease;
}

fn colour(kind: &str) -> Colour {
    match kind {
        "prompt" => Colour::rgb(91, 205, 176),
        "failure" => Colour::rgb(238, 112, 78),
        "receipt" => Colour::rgb(233, 193, 101),
        "tool" => Colour::rgb(99, 147, 195),
        _ => Colour::rgb(157, 135, 198),
    }
}

fn node(mesh: &mut Mesh3D, pos: Vec3D, size: f64, colour: Colour) {
    let start = mesh.vertices.len();
    push_bar(mesh, pos.x, pos.z, size, size, size * 2.0, colour);
    for vertex in &mut mesh.vertices[start..] {
        vertex.y += pos.y;
    }
}

fn seconds(at: &str) -> f64 {
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|v| v.timestamp_millis() as f64 / 1000.0)
        .unwrap_or(0.0)
}

fn scene(data: &Slice<'_>, selected: usize) -> Mesh3D {
    let mut mesh = Mesh3D::new(Vec::new(), Vec::new());
    let rail = Colour::rgb(48, 65, 89);
    for (i, _) in data.lanes.iter().enumerate() {
        let z = lane_z(i, data.lanes.len());
        push_bar(&mut mesh, 0.0, z, 9.0, 0.045, 0.06, rail);
        node(&mut mesh, Vec3D::new(-9.0, 0.0, z), 0.17, rail);
    }
    let first = data.records.first().map_or(0.0, |e| seconds(&e.at));
    let last = data.records.last().map_or(first, |e| seconds(&e.at));
    let mut branches = std::collections::HashMap::new();
    for (i, e) in data.records.iter().enumerate() {
        let lane = data.lanes.iter().position(|a| a == &e.agent).unwrap_or(0);
        let z = lane_z(lane, data.lanes.len());
        let x = if last > first {
            8.5 - 17.0 * (seconds(&e.at) - first) / (last - first)
        } else {
            0.0
        };
        // Each session has its own prompt branch. Only temporal membership is
        // implied: the scene makes no claim about causal or git ancestry.
        let branch = branches
            .entry((&e.agent, &e.session))
            .or_insert((x, 0usize));
        if e.kind == "prompt" {
            *branch = (x, 0);
        } else {
            branch.1 += 1;
        }
        let y = if e.kind == "prompt" {
            1.5
        } else {
            0.4 + (branch.1 % 5) as f64 * 0.20
        };
        let col = if i == selected {
            Colour::rgb(255, 255, 255)
        } else {
            colour(&e.kind)
        };
        // Stem connects each record to its agent's time rail. Prompt stems are
        // taller; record height is a category, never an invented cost metric.
        push_bar(&mut mesh, x, z, 0.022, 0.022, y, col);
        node(
            &mut mesh,
            Vec3D::new(x, y, z),
            if i == selected { 0.20 } else { 0.12 },
            col,
        );
        if e.kind != "prompt" && x < branch.0 {
            let start = mesh.vertices.len();
            push_bar(
                &mut mesh,
                (x + branch.0) / 2.0,
                z,
                (branch.0 - x) / 2.0,
                0.018,
                0.035,
                rail,
            );
            for v in &mut mesh.vertices[start..] {
                v.y += 0.25;
            }
        }
    }
    mesh
}

fn lane_z(i: usize, count: usize) -> f64 {
    ((count.saturating_sub(1)) as f64 / 2.0 - i as f64) * 3.0
}

fn label(view: &mut View, x: i64, y: i64, text: &str, colour: Colour) {
    let safe: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(view.width.saturating_sub(x.max(0) as usize + 1))
        .collect();
    use gemini_engine::core::Canvas;
    for (i, c) in safe.chars().enumerate() {
        view.plot(
            Vec2D::new(x + i as i64, y),
            crate::ColChar::new(c, Modifier::Colour(colour)),
        );
    }
}

// Match gemini-engine's perspective transform for lane labels. Scene geometry
// remains rendered and clipped by gemini-engine itself.
fn project(viewport: &Viewport, rotation: Transform3D, point: Vec3D) -> Vec2D {
    let camera = viewport
        .camera_transform
        .mul_mat4(&rotation)
        .transform_point3(point);
    let p = Transform3D::perspective_infinite_rh(
        viewport.fov.to_radians(),
        1.0,
        viewport.clipping_distace,
    )
    .project_point3(camera);
    let size = viewport.canvas_centre.x.max(viewport.canvas_centre.y) as f64;
    Vec2D::new(
        (p.x * viewport.character_width_multiplier * size) as i64 + viewport.canvas_centre.x,
        (-p.y * size) as i64 + viewport.canvas_centre.y,
    )
}

fn render(
    events: &[Event],
    coverage: &str,
    state: &mut State,
    dims: (usize, usize),
    demo: bool,
) -> View {
    let data = slice(events, state);
    let mut view = make_view(dims);
    let muted = Colour::rgb(126, 144, 164);
    let bright = Colour::rgb(204, 219, 235);
    let footer = dims.1.saturating_sub(7) as i64;
    if state.flat || dims.0 < 80 || dims.1 < 25 {
        let offset = state
            .selected
            .saturating_sub((footer.max(5) as usize - 4) / 2);
        for (i, e) in data
            .records
            .iter()
            .enumerate()
            .skip(offset)
            .take(footer.saturating_sub(4) as usize)
        {
            label(
                &mut view,
                1,
                4 + (i - offset) as i64,
                &format!(
                    "{} {} | {} | {} | {}",
                    if i == state.selected { ">" } else { " " },
                    e.at.get(11..19).unwrap_or("?"),
                    e.agent,
                    e.kind,
                    e.text
                ),
                colour(&e.kind),
            );
        }
    } else {
        let mut viewport = Viewport::new(
            Transform3D::look_at_lh(
                state.focus + Vec3D::new(state.camera_x, 28.0 + state.pitch, state.zoom),
                state.focus + Vec3D::new(0.0, 0.5, 0.0),
                Vec3D::NEG_Y,
            ),
            60.0,
            Vec2D::new(dims.0 as i64 / 2, (footer + 4) / 2),
        );
        viewport.display_mode = DisplayMode::Solid;
        let rotation = Transform3D::from_rotation_y(state.yaw);
        viewport.objects = vec![scene(&data, state.selected).with_transform(rotation)];
        view.draw(&viewport);
        for (i, lane) in data.lanes.iter().enumerate() {
            let p = project(
                &viewport,
                rotation,
                Vec3D::new(9.0, 0.2, lane_z(i, data.lanes.len())),
            );
            if p.x >= 0 && p.y > 3 && p.y < footer {
                let name = format!("{} {}", i + 1, lane.chars().take(18).collect::<String>());
                label(
                    &mut view,
                    (p.x - name.chars().count() as i64 - 1).max(0),
                    p.y,
                    &name,
                    muted,
                );
            }
        }
    }
    label(
        &mut view,
        1,
        0,
        &format!(
            "SYSTEMSCAPE / WORK    {}    {}    {}",
            data.day,
            if demo {
                "DEMO · synthetic"
            } else {
                "local history"
            },
            if state.tour {
                "FLYING TOUR"
            } else {
                "MANUAL · Space resumes tour"
            }
        ),
        bright,
    );
    label(
        &mut view,
        1,
        1,
        &format!(
            "{} of {} records · page {}/{} · day {}/{} · lanes {}/{} · time → / agents ↗",
            data.records.len(),
            data.total,
            state.record_page + 1,
            data.record_pages,
            state.day + 1,
            data.days.max(1),
            state.lane_page + 1,
            data.lane_pages
        ),
        muted,
    );
    label(&mut view, 1, 2, coverage, muted);
    label(
        &mut view,
        1,
        3,
        "Green prompt · blue tool · rust error · gold commit receipt · white selected",
        muted,
    );
    // Clear footer cells after 3D projection so text remains readable at all angles.
    for y in footer..dims.1 as i64 {
        label(&mut view, 0, y, &" ".repeat(dims.0), muted);
    }
    if let Some(e) = data.records.get(state.selected) {
        label(
            &mut view,
            1,
            footer,
            &format!(
                "[{}/{}] {}  {}  {}",
                state.selected + 1,
                data.records.len(),
                e.at,
                e.agent,
                e.kind
            ),
            bright,
        );
        label(&mut view, 1, footer + 1, &e.text, bright);
        label(
            &mut view,
            1,
            footer + 2,
            &if state.detail {
                format!("Source: {}", e.source)
            } else {
                format!("Project: {}  Session: {}", e.project, e.session)
            },
            muted,
        );
    } else {
        label(
            &mut view,
            1,
            footer,
            "No recorded work found. Waiting for transcript or archive updates.",
            bright,
        );
    }
    label(&mut view, 1, footer + 3, "Recorded work ≠ live status. Archive/transcript records can overlap; totals are not billing.", muted);
    label(
        &mut view,
        1,
        footer + 4,
        "←→ rotate  ↑↓ tilt  +/- zoom  j/k select  [/] day  Tab lanes  Space tour",
        bright,
    );
    label(
        &mut view,
        1,
        footer + 5,
        "PgUp/PgDn records  Enter source  f flat/3D  0 reset camera  q quit",
        bright,
    );
    view
}

pub fn run(args: &[String]) -> io::Result<()> {
    let mut home_override = false;
    let mut home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let mut workspace = PathBuf::from(
        std::env::var_os("WORKSPACE").unwrap_or_else(|| "/home/devuser/workspace".into()),
    );
    let mut archive = std::env::var_os("AGENTBOX_EVENT_ARCHIVE_DIR").map(PathBuf::from);
    let (mut demo, mut text, mut json, mut snapshot) = (false, false, false, false);
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--activity" => (),
            "--demo" => demo = true,
            "--text" => text = true,
            "--json" => json = true,
            "--snapshot" => snapshot = true,
            "--home" | "--workspace" | "--archive" => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{arg} requires a path"),
                    )
                })?;
                match arg.as_str() {
                    "--home" => {
                        home = value.into();
                        home_override = true;
                    }
                    "--workspace" => workspace = value.into(),
                    _ => archive = Some(value.into()),
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown option: {arg}"),
                ))
            }
        }
    }
    let archive = archive.unwrap_or_else(|| workspace.join(".agentbox/agent-events"));
    let mut collector = Collector::new(home, workspace, archive);
    if !home_override {
        collector = collector.with_env_roots();
    }
    if !demo {
        collector.poll();
    }
    let mut events = if demo {
        crate::activity_data::demo_events()
    } else {
        collector.rows()
    };
    let mut coverage = if demo {
        "Synthetic fixture · no agent files opened".to_string()
    } else {
        collector.coverage()
    };
    if text || json || (!snapshot && !io::stdout().is_terminal()) {
        if json {
            println!(
                "{}",
                serde_json::json!({"coverage": coverage, "events": events})
            );
        } else {
            println!("{coverage}");
            let mut previous = String::new();
            for e in &events {
                let branch = format!(
                    "{} / {} / {} / {}",
                    e.at.get(..10).unwrap_or("?"),
                    e.project,
                    e.agent,
                    e.session
                );
                if branch != previous {
                    println!("■ {branch}");
                    previous = branch;
                }
                println!(
                    "  └─ {} {} {}",
                    e.at.get(11..19).unwrap_or("?"),
                    e.kind,
                    e.text
                );
            }
        }
        return Ok(());
    }
    let mut state = State {
        yaw: -0.3,
        zoom: 40.0,
        tour: true,
        ..State::default()
    };
    if snapshot {
        if !io::stdout().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--snapshot requires a terminal; use --text or --json for redirected output",
            ));
        }
        render(&events, &coverage, &mut state, (120, 38), demo).display_render()?;
        return Ok(());
    }
    let _terminal = crate::Terminal::enter()?;
    let mut next_poll = Instant::now() + POLL;
    let mut dirty = true;
    let mut last_frame = Instant::now();
    loop {
        if !demo && Instant::now() >= next_poll {
            collector.poll();
            let updated = collector.rows();
            let new_coverage = collector.coverage();
            dirty |= updated != events || coverage != new_coverage;
            events = updated;
            coverage = new_coverage;
            next_poll = Instant::now() + POLL;
        }
        if last_frame.elapsed() >= Duration::from_millis(250) {
            tour_step(&events, &mut state, last_frame.elapsed().as_secs_f64());
            last_frame = Instant::now();
            dirty |= state.tour && !events.is_empty();
        }
        if dirty {
            let dims = crossterm::terminal::size().unwrap_or((110, 32));
            let view = render(
                &events,
                &coverage,
                &mut state,
                (
                    usize::from(dims.0).clamp(1, 300),
                    usize::from(dims.1).saturating_sub(1).clamp(1, 100),
                ),
                demo,
            );
            view.display_render()?;
            io::stdout().flush()?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        match event::read()? {
            Input::Resize(..) => dirty = true,
            Input::Key(key) if key.kind != KeyEventKind::Release => {
                dirty = true;
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(());
                }
                if matches!(
                    key.code,
                    KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                        | KeyCode::Tab
                        | KeyCode::Enter
                        | KeyCode::Char('j' | 'k' | '[' | ']' | '+' | '=' | '-' | '0' | 'f')
                ) {
                    state.tour = false;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Left => state.yaw -= 0.12,
                    KeyCode::Right => state.yaw += 0.12,
                    KeyCode::Up => state.pitch = (state.pitch + 1.0).min(15.0),
                    KeyCode::Down => state.pitch = (state.pitch - 1.0).max(-8.0),
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        state.zoom = (state.zoom - 2.0).max(16.0)
                    }
                    KeyCode::Char('-') => state.zoom = (state.zoom + 2.0).min(60.0),
                    KeyCode::Char(' ') => {
                        state.tour = !state.tour;
                        state.dwell = 0.0;
                    }
                    KeyCode::PageUp => {
                        state.record_page += 1;
                        state.selected = 0;
                    }
                    KeyCode::PageDown => {
                        state.record_page = state.record_page.saturating_sub(1);
                        state.selected = 0;
                    }
                    KeyCode::Char('j') => state.selected = state.selected.saturating_add(1),
                    KeyCode::Char('k') => state.selected = state.selected.saturating_sub(1),
                    KeyCode::Char('[') => {
                        state.day = state.day.saturating_add(1);
                        state.selected = 0;
                    }
                    KeyCode::Char(']') => {
                        state.day = state.day.saturating_sub(1);
                        state.selected = 0;
                    }
                    KeyCode::Tab => {
                        let data = slice(&events, &mut state);
                        state.lane_page = (state.lane_page + 1) % data.lane_pages;
                        state.selected = 0;
                    }
                    KeyCode::Char('f') => state.flat = !state.flat,
                    KeyCode::Enter => state.detail = !state.detail,
                    KeyCode::Char('0') => {
                        state.yaw = -0.3;
                        state.pitch = 0.0;
                        state.zoom = 40.0;
                        state.focus = Vec3D::ZERO;
                        state.camera_x = 0.0;
                    }
                    _ => dirty = false,
                }
            }
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tour_moves_camera_advances_records_and_pauses() {
        let events = crate::activity_data::demo_events();
        let mut state = State {
            tour: true,
            zoom: 40.0,
            ..State::default()
        };
        for _ in 0..20 {
            tour_step(&events, &mut state, 0.25);
        }
        assert_eq!(state.selected, 1);
        assert!(state.focus.is_finite());
        assert!(state.yaw != 0.0 && state.zoom != 40.0);
        state.tour = false;
        let camera = (state.yaw, state.zoom, state.selected, state.focus);
        tour_step(&events, &mut state, 1.0);
        assert_eq!(camera, (state.yaw, state.zoom, state.selected, state.focus));
    }
    #[test]
    fn every_retained_record_can_be_paged_and_toured() {
        let template = crate::activity_data::demo_events().remove(0);
        let events: Vec<_> = (0..260)
            .map(|i| {
                let mut e = template.clone();
                e.key = format!("{i:03}");
                e
            })
            .collect();
        let mut state = State::default();
        let mut visited = BTreeSet::new();
        for _ in 0..260 {
            let data = slice(&events, &mut state);
            visited.insert(data.records[state.selected].key.clone());
            advance_selection(&events, &mut state);
        }
        assert_eq!(visited.len(), 260);
        assert_eq!(state.record_page, 0);
        assert_eq!(state.selected, 0);
    }
    #[test]
    fn scene_is_bounded_and_selection_survives_empty_data() {
        let events = crate::activity_data::demo_events();
        let mut state = State {
            selected: usize::MAX,
            ..State::default()
        };
        let data = slice(&events, &mut state);
        assert!(data.records.len() <= MAX_NODES);
        assert!(data.lanes.len() <= MAX_LANES);
        assert!(scene(&data, state.selected)
            .vertices
            .iter()
            .all(|v| v.is_finite()));
        let empty = slice(&[], &mut state);
        assert!(empty.records.is_empty());
        assert_eq!(state.selected, 0);
    }
    #[test]
    fn tiny_terminals_render_without_panicking() {
        for dims in [(1, 1), (30, 8), (80, 24), (120, 38)] {
            render(
                &[],
                "empty",
                &mut State {
                    zoom: 40.0,
                    ..State::default()
                },
                dims,
                false,
            );
        }
    }
}
