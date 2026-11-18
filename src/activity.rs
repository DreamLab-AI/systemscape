//! Activity city: districts are projects, towers are sessions, floors are
//! recorded actions stacked oldest at the ground to newest at the top.
//!
//! The 3D view is for a glance: who is working, where, and on what kind of
//! thing. The flat view (`f`, Enter, or any small pane) is for detail: every
//! session and every record of the selected one. Records are evidence of logged
//! activity; "active" only means a record arrived within the last few minutes.
use crate::activity_data::{Collector, Event};
use crate::pixels::{mix, PixelCanvas, Rgb};
use crate::telemetry::quad;
use crate::{
    ColChar, Colour, DisplayMode, Face, Mesh3D, Modifier, Transform3D, Vec2D, Vec3D, Viewport,
};
use crossterm::event::{self, Event as Input, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::{BTreeSet, HashMap};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::time::{Duration, Instant};

mod city_links;
mod heat;
pub(crate) mod marks;

const FRAME_MS: u64 = 55; // ~18 FPS while the tour is flying
const POLL: Duration = Duration::from_secs(2);
const MAX_TOWERS: usize = 36;
const MAX_DISTRICTS: usize = 8;
const TOWER_HEIGHT: f64 = 14.0; // world height of the tallest tower
const MIN_BAND: f64 = 0.22; // thinner floors merge into their neighbour
const PITCH: f64 = 3.2; // spacing between tower centres
const LIVE_SECS: f64 = 300.0; // a record this recent counts as "active"
const DWELL_SECS: f64 = 6.0; // tour time on each session
const FOV: f64 = 48.0;
const SKY_TOP: Rgb = (7, 10, 20);
const SKY_HORIZON: Rgb = (20, 27, 44);
const PANEL: Rgb = (10, 14, 22);
const MUTED: Rgb = (126, 144, 164);
const BRIGHT: Rgb = (204, 219, 235);
const GREEN: Rgb = (123, 255, 203);
const RED: Rgb = (255, 70, 85);
const GOLD: Rgb = (240, 204, 82);

/// What a record is, for colour and floor height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Shell,
    Read,
    Edit,
    Web,
    Memory,
    Agent,
    Tool,
    Prompt,
    Failure,
    Receipt,
    Event,
}

const LEGEND: [Mark; 10] = [
    Mark::Shell,
    Mark::Read,
    Mark::Edit,
    Mark::Web,
    Mark::Memory,
    Mark::Agent,
    Mark::Tool,
    Mark::Prompt,
    Mark::Failure,
    Mark::Receipt,
];

impl Mark {
    fn of(event: &Event) -> Self {
        match event.kind.as_str() {
            "prompt" => Self::Prompt,
            "failure" => Self::Failure,
            "receipt" => Self::Receipt,
            "tool" => Self::of_tool(&event.text),
            _ => Self::Event,
        }
    }

    /// Classify a tool call by the tool's name, the first word of its text.
    fn of_tool(text: &str) -> Self {
        let name = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
        if has(&["todo", "plan"]) {
            Self::Tool
        } else if has(&["memory"]) {
            Self::Memory
        } else if has(&["web", "fetch", "browser", "perplexity", "scrape"]) {
            Self::Web
        } else if has(&["bash", "exec", "shell", "stdin", "terminal", "command"]) {
            Self::Shell
        } else if has(&["edit", "write", "patch"]) {
            Self::Edit
        } else if has(&["read", "grep", "glob", "search", "view", "list"]) || name == "ls" {
            Self::Read
        } else if has(&["agent", "task", "skill", "workflow", "sendmessage"]) {
            Self::Agent
        } else {
            Self::Tool
        }
    }

    fn hue(self) -> Rgb {
        match self {
            Self::Shell => (255, 150, 60),
            Self::Read => (90, 165, 255),
            Self::Edit => (130, 225, 110),
            Self::Web => (225, 120, 235),
            Self::Memory => (70, 220, 200),
            Self::Agent => (255, 120, 170),
            Self::Tool => (170, 160, 150),
            Self::Prompt => (255, 244, 185),
            Self::Failure => RED,
            Self::Receipt => (255, 215, 40),
            Self::Event => (110, 122, 140),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Web => "web",
            Self::Memory => "memory",
            Self::Agent => "agent",
            Self::Tool => "tool",
            Self::Prompt => "prompt",
            Self::Failure => "error",
            Self::Receipt => "commit",
            Self::Event => "event",
        }
    }

    /// Floor height of one record before the city-wide scale is applied.
    fn weight(self) -> f64 {
        match self {
            Self::Prompt => 0.25,
            Self::Failure | Self::Receipt => 0.4,
            Self::Event => 0.12,
            _ => 0.5,
        }
    }
}

struct State {
    /// Index into the day's sessions, newest first; follows `key` across updates.
    selected: usize,
    key: String,
    /// The day being shown; `None` follows the newest day.
    day: Option<String>,
    /// Key of the selected record in the detail view; `None` is the newest.
    record: Option<String>,
    yaw: f64,
    lift: f64,
    dolly: f64,
    /// Seconds of flight.
    t: f64,
    tour: bool,
    dwell: f64,
    flat: bool,
    detail: bool,
    coverage: bool,
    strip: bool,
    links: city_links::Show,
    heat: heat::Heat,
}

impl Default for State {
    fn default() -> Self {
        Self {
            selected: 0,
            key: String::new(),
            day: None,
            record: None,
            yaw: 0.0,
            lift: 0.0,
            dolly: 1.0,
            t: 0.0,
            tour: true,
            dwell: 0.0,
            flat: false,
            detail: false,
            coverage: false,
            strip: true,
            links: city_links::Show::default(),
            heat: heat::Heat::default(),
        }
    }
}

struct Session<'a> {
    agent: &'a str,
    id: &'a str,
    project: &'a str,
    /// Oldest first.
    records: Vec<&'a Event>,
    /// Unix seconds of the newest record.
    last: f64,
}

impl Session<'_> {
    fn key(&self) -> String {
        format!("{}\u{1f}{}", self.agent, self.id)
    }

    fn count(&self, wanted: impl Fn(Mark) -> bool) -> usize {
        self.records.iter().filter(|e| wanted(Mark::of(e))).count()
    }

    fn newest(&self) -> &Event {
        self.records[self.records.len() - 1]
    }

    /// Status dot: stuck on a repeated failure, a recent failure, recent activity, or idle.
    fn status(&self, now: f64) -> (char, Rgb) {
        let age = now - self.last;
        if crate::insight::stuck_run(&self.records).is_some() {
            ('↻', RED)
        } else if Mark::of(self.newest()) == Mark::Failure && age < 3.0 * LIVE_SECS {
            ('!', RED)
        } else if age < LIVE_SECS {
            ('●', GREEN)
        } else {
            ('○', (90, 104, 122))
        }
    }
}

/// One day of recorded work, grouped into sessions, most recent first.
struct City<'a> {
    day: String,
    /// Every day with records, newest first, and the position of `day` in it.
    days: Vec<&'a str>,
    index: usize,
    sessions: Vec<Session<'a>>,
}

fn seconds(at: &str) -> f64 {
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|v| v.timestamp_millis() as f64 / 1000.0)
        .unwrap_or(0.0)
}

fn city<'a>(events: &'a [Event], state: &mut State) -> City<'a> {
    let days: Vec<_> = events
        .iter()
        .filter_map(|e| e.at.get(..10))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .rev()
        .collect();
    // A chosen day is kept by date, so a new day arriving at midnight does not
    // shift the view; a day that has aged out of the history falls back to newest.
    let index = match state.day.as_deref() {
        Some(chosen) => days.iter().position(|d| *d == chosen).unwrap_or_else(|| {
            state.day = None;
            0
        }),
        None => 0,
    };
    let day = days.get(index).copied().unwrap_or("No history");
    let mut by_session: HashMap<(&str, &str), usize> = HashMap::new();
    let mut sessions: Vec<Session<'a>> = Vec::new();
    for event in events.iter().filter(|e| e.at.starts_with(day)) {
        let slot = *by_session
            .entry((event.agent.as_str(), event.session.as_str()))
            .or_insert_with(|| {
                sessions.push(Session {
                    agent: &event.agent,
                    id: &event.session,
                    project: &event.project,
                    records: Vec::new(),
                    last: 0.0,
                });
                sessions.len() - 1
            });
        sessions[slot].records.push(event);
    }
    for session in &mut sessions {
        // Timestamps are normalised to UTC milliseconds, so they sort as text.
        session
            .records
            .sort_by(|a, b| a.at.cmp(&b.at).then(a.key.cmp(&b.key)));
        let newest = session.records[session.records.len() - 1];
        session.project = &newest.project;
        session.last = seconds(&newest.at);
    }
    sessions.sort_by(|a, b| {
        b.last
            .total_cmp(&a.last)
            .then_with(|| (a.agent, a.id).cmp(&(b.agent, b.id)))
    });
    // Keep the same session selected when newer work reorders the list.
    match sessions.iter().position(|s| s.key() == state.key) {
        Some(found) if !state.key.is_empty() => state.selected = found,
        _ => {
            state.selected = state.selected.min(sessions.len().saturating_sub(1));
            state.key = sessions
                .get(state.selected)
                .map(Session::key)
                .unwrap_or_default();
        }
    }
    City {
        day: day.into(),
        days,
        index,
        sessions,
    }
}

fn select(state: &mut State, city: &City<'_>, index: usize) {
    if let Some(session) = city.sessions.get(index) {
        state.selected = index;
        state.key = session.key();
        state.record = None;
    }
}

/// Position of the selected record, counted from the newest.
fn record_index(state: &State, session: &Session<'_>) -> usize {
    state
        .record
        .as_deref()
        .and_then(|key| session.records.iter().rev().position(|e| e.key == key))
        .unwrap_or(0)
}

/// Move the record selection, anchored to the record's key so new records
/// arriving at the top do not shift it.
fn step_record(state: &mut State, session: &Session<'_>, delta: isize) {
    let last = session.records.len().saturating_sub(1);
    let index = record_index(state, session)
        .saturating_add_signed(delta)
        .min(last);
    state.record = (index > 0).then(|| session.records[last - index].key.clone());
}

/// Show an older (+1) or newer (-1) day. At either end nothing changes.
fn step_day(state: &mut State, city: &City<'_>, delta: isize) {
    let Some(index) = city
        .index
        .checked_add_signed(delta)
        .filter(|i| *i < city.days.len())
    else {
        return;
    };
    state.day = (index > 0).then(|| city.days[index].to_owned());
    state.key.clear();
    state.selected = 0;
    state.record = None;
}

/// Tour: fly, and every few seconds move on to the next active session, or to
/// the next session of any kind when nothing is active.
fn tour_step(events: &[Event], state: &mut State, dt: f64, now: f64) {
    if !state.tour {
        return;
    }
    let dt = dt.clamp(0.0, 1.0);
    state.t += dt;
    state.dwell += dt;
    if state.dwell < DWELL_SECS {
        return;
    }
    state.dwell = 0.0;
    let city = city(events, state);
    let visible = city.sessions.len().min(MAX_TOWERS);
    let live: Vec<usize> = (0..visible)
        .filter(|&i| now - city.sessions[i].last < LIVE_SECS)
        .collect();
    let pool = if live.is_empty() {
        (0..visible).collect()
    } else {
        live
    };
    if let Some(&first) = pool.first() {
        let next = pool
            .iter()
            .copied()
            .find(|&i| i > state.selected)
            .unwrap_or(first);
        select(state, &city, next);
    }
}

/// Last path component: the name a project goes by.
fn district_name(project: &str) -> &str {
    match project.rsplit('/').find(|part| !part.is_empty()) {
        Some("?") | None => "unknown",
        Some(name) => name,
    }
}

struct Tower {
    session: usize,
    x: f64,
    z: f64,
}

struct District {
    name: String,
    /// World x of the left and right screen edges (left is the larger x).
    left: f64,
    right: f64,
}

struct Layout {
    towers: Vec<Tower>,
    districts: Vec<District>,
    width: f64,
    depth: f64,
    /// Sideways stretch: towers and spacing widen to use a wide pane.
    stretch: f64,
    /// Sessions beyond `MAX_TOWERS`, listed only in the flat view.
    hidden: usize,
}

/// Districts run left to right in order of recent activity; within a district
/// the most recent sessions stand at the front.
fn layout(city: &City<'_>, stretch: f64) -> Layout {
    let visible = city.sessions.len().min(MAX_TOWERS);
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, session) in city.sessions.iter().take(visible).enumerate() {
        let name = district_name(session.project);
        let slot = match groups.iter().position(|(n, _)| n == name) {
            Some(slot) => slot,
            None if groups.len() < MAX_DISTRICTS => {
                groups.push((name.to_owned(), Vec::new()));
                groups.len() - 1
            }
            None => {
                groups[MAX_DISTRICTS - 1].0 = "other projects".into();
                MAX_DISTRICTS - 1
            }
        };
        groups[slot].1.push(i);
    }
    let rows = 3usize;
    let depth = rows as f64 * PITCH;
    let (margin, gap) = (0.5 * stretch, 1.8 * stretch);
    let pitch = PITCH * stretch;
    let width: f64 = groups
        .iter()
        .map(|(_, members)| members.len().div_ceil(rows) as f64 * pitch + 2.0 * margin)
        .sum::<f64>()
        + gap * groups.len().saturating_sub(1) as f64;
    // The camera looks down -z with a left-handed view: screen-right is world -x.
    let to_x = |p: f64| width / 2.0 - p;
    let (mut towers, mut districts, mut cursor) = (Vec::new(), Vec::new(), 0.0);
    for (name, members) in groups {
        let span = members.len().div_ceil(rows) as f64 * pitch + 2.0 * margin;
        for (i, session) in members.into_iter().enumerate() {
            towers.push(Tower {
                session,
                x: to_x(cursor + margin + ((i / rows) as f64 + 0.5) * pitch),
                z: depth / 2.0 - ((i % rows) as f64 + 0.5) * PITCH,
            });
        }
        districts.push(District {
            name,
            left: to_x(cursor),
            right: to_x(cursor + span),
        });
        cursor += span + gap;
    }
    Layout {
        towers,
        districts,
        width,
        depth,
        stretch,
        hidden: city.sessions.len() - visible,
    }
}

/// Unscaled height of a session's full stack.
fn raw_height(session: &Session<'_>) -> f64 {
    session.records.iter().map(|e| Mark::of(e).weight()).sum()
}

/// A session's floors, ground to top. Records thinner than `MIN_BAND` after
/// scaling merge with their neighbours, so a tower never needs more than
/// `TOWER_HEIGHT / MIN_BAND` floors however many records it holds. A merged
/// floor takes the colour of its largest share, except that an error or a
/// commit inside it always shows (a floor holding both is split in two).
fn bands(session: &Session<'_>, scale: f64) -> Vec<(f64, Mark)> {
    let mut out: Vec<(f64, Mark)> = Vec::new();
    let mut shares: Vec<(Mark, f64)> = Vec::new();
    let mut height = 0.0;
    let mut flush = |height: &mut f64, shares: &mut Vec<(Mark, f64)>| {
        if shares.is_empty() {
            return;
        }
        // An error or commit always shows; when a floor holds both, it is split.
        let urgent: Vec<Mark> = shares
            .iter()
            .map(|(mark, _)| *mark)
            .filter(|mark| matches!(mark, Mark::Failure | Mark::Receipt))
            .collect();
        let largest = shares
            .iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(mark, _)| *mark)
            .unwrap_or(Mark::Tool);
        let marks = if urgent.is_empty() {
            vec![largest]
        } else {
            urgent
        };
        let part = *height / marks.len() as f64;
        for mark in marks {
            match out.last_mut() {
                Some((previous, same)) if *same == mark => *previous += part,
                _ => out.push((part, mark)),
            }
        }
        *height = 0.0;
        shares.clear();
    };
    for record in &session.records {
        let mark = Mark::of(record);
        let h = mark.weight() * scale;
        height += h;
        match shares.iter_mut().find(|(m, _)| *m == mark) {
            Some((_, share)) => *share += h,
            None => shares.push((mark, h)),
        }
        if height >= MIN_BAND {
            flush(&mut height, &mut shares);
        }
    }
    flush(&mut height, &mut shares);
    out
}

fn fill(c: Rgb) -> ColChar {
    ColChar::SOLID.with_mod(Modifier::Colour(Colour::rgb(c.0, c.1, c.2)))
}

/// One floor of a tower: the three sides the camera can see, plus a roof on
/// the top floor. Vertex and face ordering mirror `Mesh3D::default_cube()`.
fn floor(
    mesh: &mut Mesh3D,
    centre: (f64, f64),
    half_width: f64,
    y: (f64, f64),
    base: Rgb,
    roof: Option<Rgb>,
) {
    let b = mesh.vertices.len();
    for x in [centre.0 + half_width, centre.0 - half_width] {
        for height in [y.1, y.0] {
            for z in [centre.1 - 1.0, centre.1 + 1.0] {
                mesh.vertices.push(Vec3D::new(x, height, z));
            }
        }
    }
    let shade = |f: f64| mix((6, 8, 14), base, f);
    for (indices, c) in [
        ([2usize, 3, 1, 0], Some(shade(0.55))), // +x
        ([4, 5, 7, 6], Some(shade(0.55))),      // -x
        ([1, 3, 7, 5], Some(shade(0.88))),      // +z, facing the camera
        ([0, 1, 5, 4], roof),
    ] {
        if let Some(c) = c {
            mesh.faces
                .push(Face::new(indices.iter().map(|i| b + i).collect(), fill(c)));
        }
    }
}

/// World height given to the tallest tower. A small city gets shorter,
/// chunkier towers so the camera can come closer and fill the pane.
fn height_cap(lay: &Layout) -> f64 {
    (lay.width / lay.stretch * 0.5).clamp(6.0, TOWER_HEIGHT)
}

/// Scale applied to every visible tower so the tallest reaches `height_cap`.
/// Towers are never stretched: a quiet day stays a low skyline.
fn tower_scale(city: &City<'_>, lay: &Layout) -> f64 {
    let tallest = lay
        .towers
        .iter()
        .map(|t| raw_height(&city.sessions[t.session]))
        .fold(0.0, f64::max);
    (height_cap(lay) / tallest.max(0.001)).min(1.0)
}

/// Ground plates and towers as two meshes; the ground is drawn first so the
/// engine's per-face depth sort never puts a plate over a tower.
fn build_scene(
    city: &City<'_>,
    lay: &Layout,
    selected: usize,
    t: f64,
    now: f64,
    heat: Option<&heat::Tint>,
) -> (Mesh3D, Mesh3D) {
    let mut ground = Mesh3D::new(Vec::new(), Vec::new());
    let mut towers = Mesh3D::new(Vec::new(), Vec::new());
    let selected_district = lay
        .towers
        .iter()
        .position(|t| t.session == selected)
        .map(|i| {
            let x = lay.towers[i].x;
            lay.districts
                .iter()
                .position(|d| x <= d.left && x >= d.right)
        });
    for (i, district) in lay.districts.iter().enumerate() {
        let lit = selected_district == Some(Some(i));
        quad(
            &mut ground,
            district.right,
            district.left,
            0.0,
            -lay.depth / 2.0 - 0.4,
            lay.depth / 2.0 + 0.4,
            if lit { (58, 76, 104) } else { (36, 47, 68) },
        );
    }
    let scale = tower_scale(city, lay);
    for tower in &lay.towers {
        let session = &city.sessions[tower.session];
        let live = now - session.last < LIVE_SECS;
        let floors = bands(session, scale);
        let mut y = 0.0;
        for (i, (height, mark)) in floors.iter().enumerate() {
            let mut base = mark.hue();
            if !live {
                base = mix(base, (44, 52, 68), 0.55); // idle sessions recede
            }
            if tower.session == selected {
                base = mix(base, (255, 255, 255), 0.14);
            }
            if let Some(heat) = heat {
                base = heat.floor(
                    &session.records,
                    scale,
                    (y, y + height),
                    tower.session == selected,
                );
            }
            let roof = (i + 1 == floors.len()).then(|| {
                // Active towers carry a slow beacon on the roof.
                let pulse = if live {
                    0.35 + 0.3 * (t * 3.0).sin()
                } else {
                    0.1
                };
                mix(base, (255, 255, 255), pulse)
            });
            floor(
                &mut towers,
                (tower.x, tower.z),
                lay.stretch,
                (y, y + height),
                base,
                roof,
            );
            y += height;
        }
    }
    (ground, towers)
}

fn age(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Truncate to `width` columns, marking the cut.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    if width > 0 {
        cut.push('…');
    }
    cut
}

/// Shorten the home directory to `~` for display, only where it is a whole
/// path prefix: `/home/dev/x` becomes `~/x`, `/home/devuser/x` is untouched.
fn tidy(text: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) => tidy_home(text, home.trim_end_matches('/')),
        Err(_) => text.to_owned(),
    }
}

fn tidy_home(text: &str, home: &str) -> String {
    if home.len() < 2 {
        return text.to_owned();
    }
    let path_char = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(home) {
        let before = rest[..at].chars().next_back();
        let after = rest[at + home.len()..].chars().next();
        let starts = before.is_none_or(|c| !path_char(c));
        let ends = after.is_none_or(|c| c == '/' || !path_char(c));
        out.push_str(&rest[..at]);
        out.push_str(if starts && ends { "~" } else { home });
        rest = &rest[at + home.len()..];
    }
    out.push_str(rest);
    out
}

fn short(id: &str) -> String {
    city_links::short_id(id)
}

/// Match gemini-engine's perspective transform, to place text labels over the
/// scene. Geometry itself is rendered and clipped by gemini-engine.
pub(crate) fn project(viewport: &Viewport, rotation: Transform3D, point: Vec3D) -> Vec2D {
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

/// gemini-engine 1.2 does not clip polygons crossing the camera plane before
/// rasterisation. Clip in camera space to bound raster work during close flight.
pub(crate) fn clip_scene(mesh: Mesh3D, viewport: &Viewport, dims: (usize, usize)) -> Mesh3D {
    let transform = viewport.camera_transform.mul_mat4(&mesh.transform);
    let inverse = transform.inverse();
    let scale = viewport.canvas_centre.x.max(viewport.canvas_centre.y) as f64;
    let focal = 1.0 / (viewport.fov.to_radians() * 0.5).tan();
    let cx = viewport.canvas_centre.x as f64;
    let cy = viewport.canvas_centre.y as f64;
    let stretch = viewport.character_width_multiplier;
    let planes = [
        (Vec3D::Z, -2.0),
        (-Vec3D::Z, 600.0),
        (Vec3D::new(-1.0, 0.0, cx / (stretch * focal * scale)), 0.0),
        (
            Vec3D::new(1.0, 0.0, (dims.0 as f64 - cx) / (stretch * focal * scale)),
            0.0,
        ),
        (Vec3D::new(0.0, 1.0, cy / (focal * scale)), 0.0),
        (
            Vec3D::new(0.0, -1.0, (dims.1 as f64 - cy) / (focal * scale)),
            0.0,
        ),
    ];
    let mut output = Mesh3D::new(Vec::new(), Vec::new()).with_transform(mesh.transform);
    for face in &mesh.faces {
        let mut polygon: Vec<_> = face
            .v_indices
            .iter()
            .map(|&i| transform.transform_point3(mesh.vertices[i]))
            .collect();
        for (normal, offset) in planes {
            if polygon.is_empty() {
                break;
            }
            let mut clipped = Vec::new();
            for i in 0..polygon.len() {
                let a = polygon[i];
                let b = polygon[(i + 1) % polygon.len()];
                let da = normal.dot(a) + offset;
                let db = normal.dot(b) + offset;
                if da >= 0.0 {
                    clipped.push(a);
                }
                if (da >= 0.0) != (db >= 0.0) {
                    clipped.push(a.lerp(b, da / (da - db)));
                }
            }
            polygon = clipped;
        }
        if polygon.len() >= 3 {
            let first = output.vertices.len();
            output
                .vertices
                .extend(polygon.iter().map(|p| inverse.transform_point3(*p)));
            output.faces.push(crate::Face::new(
                (first..output.vertices.len()).collect(),
                face.fill_char,
            ));
        }
    }
    output
}

fn header(canvas: &mut PixelCanvas, city: &City<'_>, state: &State, demo: bool, now: f64) {
    let projects: BTreeSet<_> = city
        .sessions
        .iter()
        .map(|s| district_name(s.project))
        .collect();
    let active = city
        .sessions
        .iter()
        .filter(|s| now - s.last < LIVE_SECS)
        .count();
    canvas.label(1, 0, "SYSTEMSCAPE WORK", (63, 235, 255));
    canvas.label(
        19,
        0,
        &format!(
            "{} (day {}/{}) · {} sessions · {} projects · {} active · {}{}",
            city.day,
            city.index + 1,
            city.days.len().max(1),
            city.sessions.len(),
            projects.len(),
            active,
            if state.tour {
                "FOLLOWING LIVE"
            } else {
                "MANUAL"
            },
            if demo { " · DEMO" } else { "" }
        ),
        MUTED,
    );
}

/// Sideways sway of the tour camera, in radians either side of `yaw`.
const SWAY: f64 = 0.38;

/// Camera aimed at the city's centre from `angle`, `distance` away.
fn city_camera(tallest: f64, angle: f64, distance: f64, lift: f64, centre: Vec2D) -> Viewport {
    let target = Vec3D::new(0.0, tallest * 0.3, 0.0);
    let eye = target
        + Vec3D::new(
            distance * angle.sin(),
            distance * 0.42 + lift,
            distance * angle.cos(),
        );
    let mut viewport = Viewport::new(
        Transform3D::look_at_lh(eye, target, Vec3D::NEG_Y),
        FOV,
        centre,
    );
    viewport.display_mode = DisplayMode::Solid;
    viewport.character_width_multiplier = 1.0; // samples are square
    viewport
}

/// Corners of everything the city draws: plates, towers and the district
/// names in front.
fn city_corners(lay: &Layout, tallest: f64) -> Vec<Vec3D> {
    let (x, z) = (lay.width / 2.0, lay.depth / 2.0 + 1.6);
    let mut corners = Vec::with_capacity(8);
    for y in [0.0, tallest] {
        for (cx, cz) in [(x, z), (-x, z), (x, -z), (-x, -z)] {
            corners.push(Vec3D::new(cx, y, cz));
        }
    }
    corners
}

/// The nearest camera distance at which the whole city stays inside `frame`
/// (sample-space x0, y0, x1, y1) from every angle the sway reaches. Found by
/// projecting the city's corners and scaling the distance by how far they
/// overflow, which converges in a few steps.
fn fit_distance(
    lay: &Layout,
    tallest: f64,
    yaw: f64,
    lift: f64,
    centre: Vec2D,
    frame: (f64, f64, f64, f64),
) -> f64 {
    let corners = city_corners(lay, tallest);
    let (x0, y0, x1, y1) = frame;
    let (cx, cy) = (f64::from(centre.x as i32), f64::from(centre.y as i32));
    let mut widest: f64 = 12.0;
    for angle in [yaw - SWAY, yaw, yaw + SWAY] {
        let mut distance = 40.0_f64.max(widest);
        for _ in 0..10 {
            let viewport = city_camera(tallest, angle, distance, lift, centre);
            let mut overflow: f64 = 0.0;
            for corner in &corners {
                let depth = viewport.camera_transform.transform_point3(*corner).z;
                if depth <= 1.0 {
                    overflow = overflow.max(2.0);
                    continue;
                }
                let p = project(&viewport, Transform3D::IDENTITY, *corner);
                let (px, py) = (p.x as f64, p.y as f64);
                let ratio = |offset: f64, room: f64| offset / room.max(1.0);
                overflow = overflow
                    .max(ratio(cx - px, cx - x0))
                    .max(ratio(px - cx, x1 - cx))
                    .max(ratio(cy - py, cy - y0))
                    .max(ratio(py - cy, y1 - cy));
            }
            distance *= overflow.clamp(0.5, 2.0);
            if (overflow - 1.0).abs() < 0.01 {
                break;
            }
        }
        widest = widest.max(distance * 1.04);
    }
    widest
}

/// The layout, tallest tower and camera distance for a city drawn into
/// `frame`. A wide pane fits a small city by its height, so the city is
/// stretched sideways as far as that costs no extra distance.
fn city_view(
    city: &City<'_>,
    yaw: f64,
    lift: f64,
    centre: Vec2D,
    frame: (f64, f64, f64, f64),
) -> (Layout, f64, f64) {
    let tallest_of = |lay: &Layout| {
        let scale = tower_scale(city, lay);
        lay.towers
            .iter()
            .map(|t| raw_height(&city.sessions[t.session]) * scale)
            .fold(1.0, f64::max)
    };
    let distance = |lay: &Layout| fit_distance(lay, tallest_of(lay), yaw, lift, centre, frame);
    let plain = layout(city, 1.0);
    let closest = distance(&plain);
    let lay = [2.6, 2.2, 1.9, 1.6, 1.35, 1.15]
        .into_iter()
        .map(|stretch| layout(city, stretch))
        .find(|lay| distance(lay) <= closest * 1.03)
        .unwrap_or(plain);
    let tallest = tallest_of(&lay);
    let fitted = distance(&lay);
    (lay, tallest, fitted)
}

/// Scene area of a pane in samples: the centre the camera aims at, and the
/// frame (x0, y0, x1, y1) the city must stay inside, with a small margin.
fn scene_frame(
    dims: (usize, usize),
    scene_top: i64,
    scene_bottom: i64,
) -> (Vec2D, (f64, f64, f64, f64)) {
    let (width, height) = PixelCanvas::samples_for(dims.0, dims.1);
    let cell = height as i64 / (dims.1 as i64).max(1);
    let centre = Vec2D::new(width as i64 / 2, (scene_top + scene_bottom) * cell / 2);
    let frame = (
        4.0,
        (scene_top * cell) as f64 + 2.0,
        width as f64 - 4.0,
        (scene_bottom * cell) as f64 - 2.0,
    );
    (centre, frame)
}

/// The glance view: the city, a callout on the selected tower and a strip of
/// the most recently active sessions.
fn render_city(
    city: &City<'_>,
    links: &city_links::Links<'_>,
    coverage: &str,
    state: &mut State,
    dims: (usize, usize),
    demo: bool,
    now: f64,
) -> Vec<String> {
    let rows = dims.1 as i64;
    let columns = dims.0 as i64;
    let mut canvas = PixelCanvas::new(dims.0, dims.1, SKY_TOP, SKY_HORIZON);
    let strip = if state.strip {
        (city.sessions.len().min(5) as i64 + 1).min(rows / 3)
    } else {
        0
    };
    let scene_top = 2;
    let scene_bottom = rows - 1 - strip; // first row below the scene
    let (width, height) = canvas.sample_dims();
    let (centre, frame) = scene_frame(dims, scene_top, scene_bottom);
    let (lay, tallest, fitted) = city_view(city, state.yaw, state.lift, centre, frame);
    let scale = tower_scale(city, &lay);
    let selected_top = lay
        .towers
        .iter()
        .find(|t| t.session == state.selected)
        .map(|t| Vec3D::new(t.x, raw_height(&city.sessions[t.session]) * scale, t.z));

    // The camera sways in front of the city, always aimed at its centre.
    let radius = fitted * state.dolly;
    let angle = state.yaw + SWAY * (state.t * 0.11).sin();
    let viewport = city_camera(tallest, angle, radius, state.lift, centre);
    let heat = heat::view(&state.heat, &city.day, now);
    let tint = match &heat {
        heat::View::Ready(tint) => Some(tint),
        _ => None,
    };
    let (ground, mut towers) = build_scene(city, &lay, state.selected, state.t, now, tint);
    let scene_rows = scene_bottom - scene_top;
    let (ghost, roads, drawn) =
        city_links::scene(city, links, &lay, scale, tallest, scene_rows, &mut towers);
    let mut viewport = viewport;
    for (mesh, wire) in [
        (ground, false),
        (ghost, true),
        (roads, false),
        (towers, false),
    ] {
        viewport.display_mode = if wire {
            DisplayMode::Wireframe {
                backface_culling: false,
            }
        } else {
            DisplayMode::Solid
        };
        viewport.objects = vec![clip_scene(mesh, &viewport, (width, height))];
        gemini_engine::core::CanDraw::draw_to(&viewport, &mut canvas);
    }
    let at = |point: Vec3D| PixelCanvas::cell_of(project(&viewport, Transform3D::IDENTITY, point));

    // Callout tethered to the selected tower, placed first so district names
    // can keep clear of it.
    let mut callout: Option<(i64, i64, i64)> = None; // column, row, width
    if let (Some(top), Some(session)) = (selected_top, city.sessions.get(state.selected)) {
        let (col, row) = at(top);
        let newest = session.newest();
        let lines = [
            format!(
                "{} · {} · {}",
                session.agent,
                district_name(session.project),
                short(session.id)
            ),
            format!("{}  {}", Mark::of(newest).name(), tidy(&newest.text)),
            format!(
                "{} actions · {} prompts · {} errors · {} ago",
                session.count(|m| !matches!(
                    m,
                    Mark::Prompt | Mark::Failure | Mark::Receipt | Mark::Event
                )),
                session.count(|m| m == Mark::Prompt),
                session.count(|m| m == Mark::Failure),
                age(now - session.last)
            ),
        ];
        let widest = (columns - 6).clamp(10, 64) as usize;
        let lines = lines.map(|line| fit(&line, widest));
        let longest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as i64;
        let col = (col + 2).min(columns - longest - 4).max(0);
        let row = row.clamp(scene_top, (scene_bottom - 3).max(scene_top));
        canvas.label(col, row, "╶─┐", GOLD);
        canvas.label(col + 2, row + 1, "│", GOLD);
        canvas.label(col + 2, row + 2, "└", GOLD);
        canvas.label(col + 4, row, &lines[0], BRIGHT);
        canvas.label(col + 4, row + 1, &lines[1], Mark::of(newest).hue());
        canvas.label(col + 4, row + 2, &lines[2], MUTED);
        if let Some(energy) = heat::session_energy(&heat, &session.records) {
            canvas.label(col + 4, row + 3, &fit(&energy.short(), widest), MUTED);
        }
        callout = Some((col, row, longest + 4));
    }

    // District names along the front edge. Alternate districts drop a row, so
    // a name may run on under its neighbour as far as the district after that.
    // A name the callout covers moves down a row, or is left out.
    let covered = |col: i64, row: i64, len: i64| {
        callout.is_some_and(|(c, r, w)| row >= r && row < r + 3 && col < c + w && col + len > c)
    };
    let fronts: Vec<(i64, i64)> = lay
        .districts
        .iter()
        .map(|d| at(Vec3D::new(d.left, 0.0, lay.depth / 2.0 + 1.2)))
        .collect();
    for (i, district) in lay.districts.iter().enumerate() {
        let (left, row) = fronts[i];
        let limit = fronts.get(i + 2).map_or(columns, |next| next.0 - 1);
        let name = fit(&district.name, (limit - left).max(0) as usize);
        let len = name.chars().count() as i64;
        let row = (row + (i % 2) as i64).clamp(scene_top, scene_bottom - 1);
        let row = [row, row + 1, row + 2]
            .into_iter()
            .find(|r| *r < scene_bottom && !covered(left, *r, len));
        if let Some(row) = row {
            canvas.label(left, row, &name, BRIGHT);
        }
    }

    header(&mut canvas, city, state, demo, now);
    let mut col = 1;
    for mark in LEGEND {
        canvas.label(col, 1, "■", mark.hue());
        canvas.label(col + 2, 1, mark.name(), MUTED);
        col += mark.name().len() as i64 + 4;
    }
    canvas.label(
        col,
        1,
        "floors: oldest at ground, newest on top",
        (86, 100, 120),
    );
    heat::legend(&mut canvas, &heat, 1, columns);
    if state.coverage {
        canvas.label(1, 2, coverage, MUTED);
    } else if lay.hidden > 0 {
        canvas.label(
            1,
            2,
            &format!("+{} older sessions in the flat view (f)", lay.hidden),
            MUTED,
        );
    }
    if !state.coverage {
        // The overlay key shares the row, clear of the note on hidden sessions.
        let from = if lay.hidden > 0 { 44 } else { 0 };
        city_links::key(&mut canvas, links, &drawn, 2, from, columns);
    }
    if city.sessions.is_empty() {
        canvas.label(
            1,
            3,
            "No recorded work found. Waiting for transcript or archive updates.",
            BRIGHT,
        );
    }

    // Strip: the most recent sessions and what each last did.
    if strip > 0 {
        canvas.fill_rows(scene_bottom as usize, (rows - 1) as usize, PANEL);
    }
    canvas.fill_rows(0, 2, PANEL);
    canvas.fill_rows((rows - 1) as usize, rows as usize, PANEL);
    if strip > 0 {
        canvas.label(1, scene_bottom, "MOST RECENT", GOLD);
        canvas.label(
            14,
            scene_bottom,
            "● record in the last 5 min   ○ idle   ! ended on an error   ↻ stuck",
            (86, 100, 120),
        );
        for (i, session) in city.sessions.iter().take((strip - 1) as usize).enumerate() {
            let row = scene_bottom + 1 + i as i64;
            let (dot, tint) = session.status(now);
            let chosen = i == state.selected;
            canvas.label(1, row, if chosen { ">" } else { " " }, GOLD);
            canvas.label(3, row, &dot.to_string(), tint);
            canvas.label(
                5,
                row,
                &fit(&city_links::who(session), 16),
                if chosen { BRIGHT } else { MUTED },
            );
            canvas.label(22, row, &fit(district_name(session.project), 18), MUTED);
            let room = (columns - 41 - 7).max(0) as usize;
            let newest = session.newest();
            canvas.label(
                41,
                row,
                &fit(&tidy(&newest.text), room),
                mix(Mark::of(newest).hue(), BRIGHT, 0.35),
            );
            let ago = age(now - session.last);
            canvas.label(columns - 1 - ago.len() as i64, row, &ago, MUTED);
        }
    }
    canvas.label(
        1,
        rows - 1,
        "Space follow/hold · j/k session · Enter or f detail · arrows steer · +/- zoom · [/] day · h strip · ? sources · q quit",
        MUTED,
    );
    canvas.rows()
}

/// The detail view: every session of the day, then every record of the
/// selected session, newest first. Also the layout for small panes.
fn render_flat(
    city: &City<'_>,
    links: &city_links::Links<'_>,
    coverage: &str,
    state: &mut State,
    dims: (usize, usize),
    demo: bool,
    now: f64,
) -> Vec<String> {
    let rows = dims.1 as i64;
    let columns = dims.0;
    let mut canvas = PixelCanvas::new(dims.0, dims.1, PANEL, PANEL);
    header(&mut canvas, city, state, demo, now);
    let Some(session) = city.sessions.get(state.selected) else {
        canvas.label(
            1,
            2,
            "No recorded work found. Waiting for transcript or archive updates.",
            BRIGHT,
        );
        canvas.label(1, 3, coverage, MUTED);
        canvas.label(1, rows - 1, "f 3D · q quit", MUTED);
        return canvas.rows();
    };

    // Sessions table.
    let listed = city.sessions.len().min(((rows - 7) / 3).max(2) as usize);
    let first = (state.selected + 1).saturating_sub(listed);
    for (line, (i, each)) in city
        .sessions
        .iter()
        .enumerate()
        .skip(first)
        .take(listed)
        .enumerate()
    {
        let row = 1 + line as i64;
        let chosen = i == state.selected;
        let (dot, tint) = each.status(now);
        let ago = age(now - each.last);
        canvas.label(0, row, if chosen { ">" } else { " " }, GOLD);
        canvas.label(1, row, &dot.to_string(), tint);
        canvas.label(3, row, &format!("{ago:>3}"), MUTED);
        canvas.label(
            7,
            row,
            &fit(&city_links::who(each), 14),
            if chosen { BRIGHT } else { MUTED },
        );
        canvas.label(
            22,
            row,
            &fit(district_name(each.project), 16),
            if chosen { BRIGHT } else { MUTED },
        );
        let counts = format!(
            "{}a {}p {}!",
            each.count(|m| !matches!(
                m,
                Mark::Prompt | Mark::Failure | Mark::Receipt | Mark::Event
            )),
            each.count(|m| m == Mark::Prompt),
            each.count(|m| m == Mark::Failure)
        );
        canvas.label(39, row, &counts, MUTED);
        let newest = each.newest();
        canvas.label(
            54,
            row,
            &fit(&tidy(&newest.text), columns.saturating_sub(55)),
            mix(Mark::of(newest).hue(), PANEL, 0.25),
        );
    }
    let below = 1 + listed as i64;
    canvas.label(
        0,
        below,
        &fit(
            &format!(
                "── {}/{} · {} · {} · {} records ──",
                state.selected + 1,
                city.sessions.len(),
                session.agent,
                short(session.id),
                session.records.len()
            ),
            columns,
        ),
        GOLD,
    );
    if let Some(energy) =
        heat::session_energy(&heat::view(&state.heat, &city.day, now), &session.records)
    {
        // After the separator above, whose text this repeats only to measure it.
        let from = 1 + format!(
            "── {}/{} · {} · {} · {} records ──",
            state.selected + 1,
            city.sessions.len(),
            session.agent,
            short(session.id),
            session.records.len()
        )
        .chars()
        .count();
        canvas.label(
            from as i64,
            below,
            &fit(&energy.long(), columns.saturating_sub(from + 1)),
            MUTED,
        );
    }

    // Records of the selected session, newest first.
    let chosen_record = record_index(state, session);
    // A line about the session's links, when the pane has rows to spare.
    let note = city_links::note(city, links, state.selected, columns.saturating_sub(2))
        .filter(|_| rows - below - 6 >= 3);
    let space = (rows - below - 5 - i64::from(note.is_some())).max(1) as usize;
    if let Some(note) = &note {
        canvas.label(1, rows - 5, note, mix(GOLD, MUTED, 0.4));
    }
    let first = (chosen_record + 1).saturating_sub(space);
    for (line, (i, record)) in session
        .records
        .iter()
        .rev()
        .enumerate()
        .skip(first)
        .take(space)
        .enumerate()
    {
        let row = below + 1 + line as i64;
        let mark = Mark::of(record);
        let chosen = i == chosen_record;
        canvas.label(0, row, if chosen { ">" } else { " " }, GOLD);
        canvas.label(2, row, record.at.get(11..19).unwrap_or("?"), MUTED);
        canvas.label(11, row, mark.name(), mark.hue());
        canvas.label(
            18,
            row,
            &fit(&tidy(&record.text), columns.saturating_sub(19)),
            if chosen {
                BRIGHT
            } else {
                mix(mark.hue(), BRIGHT, 0.45)
            },
        );
    }

    // Full text of the selected record, wrapped over two lines, then where it came from.
    let record = session.records[session.records.len() - 1 - chosen_record];
    let wide = columns.saturating_sub(2).max(1);
    let text: Vec<char> = record.text.chars().collect();
    for (line, chunk) in text.chunks(wide).take(2).enumerate() {
        canvas.label(
            1,
            rows - 4 + line as i64,
            &chunk.iter().collect::<String>(),
            BRIGHT,
        );
    }
    let origin = if state.detail {
        format!("Source: {}", record.source)
    } else if state.coverage {
        coverage.to_owned()
    } else {
        format!("Project: {}  Session: {}", session.project, session.id)
    };
    canvas.label(1, rows - 2, &fit(&origin, wide), MUTED);
    canvas.label(
        1,
        rows - 1,
        &fit(
            "j/k record · Tab session · [/] day · Enter source · ? sources · f 3D · q quit · logged ≠ live",
            wide,
        ),
        MUTED,
    );
    canvas.rows()
}

fn render(
    events: &[Event],
    coverage: &str,
    state: &mut State,
    dims: (usize, usize),
    demo: bool,
    now: f64,
) -> Vec<String> {
    let city = city(events, state);
    let links = city_links::Links::build(events, &city, state.links, now);
    if state.flat || dims.0 < 80 || dims.1 < 20 {
        render_flat(&city, &links, coverage, state, dims, demo, now)
    } else {
        render_city(&city, &links, coverage, state, dims, demo, now)
    }
}

fn unix_now() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

/// What a key press asks of the run loop.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Quit,
    Redraw,
    /// The key means nothing here: nothing changed.
    Ignore,
}

/// Apply one key press to the view state. `in_list` is whether the detail
/// view is on screen, where `j`/`k` and Enter act on records.
fn handle_key(state: &mut State, key: KeyEvent, events: &[Event], in_list: bool) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
    }
    if key.modifiers.is_empty() && city_links::toggle(&mut state.links, key.code) {
        return Action::Redraw; // a view toggle, like `h`, leaves the tour flying
    }
    if key.modifiers.is_empty() && heat::toggle(&mut state.heat, key.code) {
        return Action::Redraw;
    }
    if !matches!(
        key.code,
        KeyCode::Char(' ' | '?' | 'h' | 'q') | KeyCode::Esc
    ) {
        state.tour = false;
    }
    let city = city(events, state);
    // In the city, only drawn towers can be selected.
    let count = if in_list {
        city.sessions.len()
    } else {
        city.sessions.len().min(MAX_TOWERS)
    };
    let step = |state: &mut State, forward: bool| {
        if count > 0 {
            let current = state.selected.min(count - 1);
            let next = if forward {
                (current + 1) % count
            } else {
                (current + count - 1) % count
            };
            select(state, &city, next);
        }
    };
    let session = city.sessions.get(state.selected);
    let records = |state: &mut State, delta: isize| {
        if let Some(session) = session {
            step_record(state, session, delta);
        }
    };
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Char(' ') => {
            state.tour = !state.tour;
            state.dwell = 0.0;
        }
        KeyCode::Left => state.yaw = (state.yaw - 0.08).max(-1.1),
        KeyCode::Right => state.yaw = (state.yaw + 0.08).min(1.1),
        KeyCode::Up => state.lift = (state.lift + 2.0).min(60.0),
        KeyCode::Down => state.lift = (state.lift - 2.0).max(-12.0),
        KeyCode::Char('+') | KeyCode::Char('=') => state.dolly = (state.dolly * 0.9).max(0.4),
        KeyCode::Char('-') => state.dolly = (state.dolly * 1.1).min(3.0),
        KeyCode::Char('0') => (state.yaw, state.lift, state.dolly) = (0.0, 0.0, 1.0),
        KeyCode::Char('j') if in_list => records(state, 1),
        KeyCode::Char('k') if in_list => records(state, -1),
        KeyCode::PageDown => records(state, 10),
        KeyCode::PageUp => records(state, -10),
        KeyCode::Char('j') | KeyCode::Tab => step(state, true),
        KeyCode::Char('k') | KeyCode::BackTab => step(state, false),
        KeyCode::Char('[') => step_day(state, &city, 1),
        KeyCode::Char(']') => step_day(state, &city, -1),
        KeyCode::Char('f') => state.flat = !state.flat,
        KeyCode::Enter if in_list => state.detail = !state.detail,
        KeyCode::Enter => state.flat = true,
        KeyCode::Char('h') => state.strip = !state.strip,
        KeyCode::Char('?') => state.coverage = !state.coverage,
        _ => return Action::Ignore,
    }
    Action::Redraw
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
    let mut state = State::default();
    if snapshot {
        if !io::stdout().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--snapshot requires a terminal; use --text or --json for redirected output",
            ));
        }
        for row in render(&events, &coverage, &mut state, (120, 38), demo, unix_now()) {
            println!("{row}");
        }
        return Ok(());
    }
    state.heat = heat::Heat::new(demo);
    let _terminal = crate::Terminal::enter()?;
    let mut next_poll = Instant::now() + POLL;
    let mut dirty = true;
    let mut drawn = Instant::now();
    let mut sight = crate::Sight::new();
    let mut buffer = crate::FrameBuffer::default();
    let mut last_frame = Instant::now();
    loop {
        if sight.tick() {
            dirty = true;
            next_poll = Instant::now();
        }
        let visible = sight.visible();
        // Off screen, the view neither polls nor draws. The transcripts are
        // append-only logs read from a saved offset, so everything written
        // meanwhile is picked up by the first poll once it is shown again.
        if visible && !demo && Instant::now() >= next_poll {
            collector.poll();
            let updated = collector.rows();
            let new_coverage = collector.coverage();
            dirty |= updated != events || coverage != new_coverage;
            events = updated;
            coverage = new_coverage;
            next_poll = Instant::now() + POLL;
        }
        let now = unix_now();
        dirty |= visible && state.heat.refresh(now);
        let dims = crate::term_dims();
        let in_list = state.flat || dims.0 < 80 || dims.1 < 20;
        // The tour animates only while the view has the keyboard. Visible but
        // unfocused, it stays current and redraws twice a second.
        let flying = sight.focused() && state.tour && !in_list;
        if visible && last_frame.elapsed() >= Duration::from_millis(FRAME_MS) {
            let dt = if flying {
                last_frame.elapsed().as_secs_f64()
            } else {
                0.0
            };
            tour_step(&events, &mut state, dt, now);
            last_frame = Instant::now();
        }
        // A new or resized pane is always drawn once, so it is never blank.
        if buffer.dims != dims
            || visible && (dirty || flying || drawn.elapsed() >= Duration::from_millis(500))
        {
            buffer.present_rows(
                render(&events, &coverage, &mut state, dims, demo, now),
                dims,
            )?;
            dirty = false;
            drawn = Instant::now();
        }
        let wait = match (visible, flying) {
            (false, _) => Duration::from_secs(2),
            (true, true) => Duration::from_millis(FRAME_MS),
            (true, false) => Duration::from_millis(250),
        };
        if !event::poll(wait)? {
            continue;
        }
        match event::read()? {
            Input::Resize(..) => dirty = true,
            Input::FocusGained => {
                if sight.focus(true) {
                    next_poll = Instant::now(); // catch up straight away
                }
                dirty = true;
            }
            Input::FocusLost => {
                sight.focus(false);
            }
            Input::Key(key) if key.kind != KeyEventKind::Release => {
                match handle_key(&mut state, key, &events, in_list) {
                    Action::Quit => return Ok(()),
                    Action::Redraw => dirty = true,
                    Action::Ignore => (),
                }
            }
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity_data::demo_events;

    pub(super) fn plain(rows: &[String]) -> String {
        let mut text = String::new();
        for row in rows {
            let mut escape = false;
            for c in row.chars() {
                match (escape, c) {
                    (false, '\x1b') => escape = true,
                    (true, 'm') => escape = false,
                    (false, _) => text.push(c),
                    _ => {}
                }
            }
            text.push('\n');
        }
        text
    }

    fn event(key: usize, at: &str, session: &str, kind: &str, text: &str) -> Event {
        Event {
            key: format!("{key:05}"),
            at: at.into(),
            agent: "claude".into(),
            session: session.into(),
            project: "/work/alpha".into(),
            kind: kind.into(),
            text: text.into(),
            source: "fixture".into(),
            parent: None,
            path: None,
        }
    }

    #[test]
    fn tools_are_classified_by_name() {
        for (text, mark) in [
            ("Bash cargo test", Mark::Shell),
            ("exec_command pwd", Mark::Shell),
            ("Read src/main.rs", Mark::Read),
            ("Grep fn main", Mark::Read),
            ("Edit src/main.rs", Mark::Edit),
            ("apply_patch src/lib.rs", Mark::Edit),
            ("WebSearch rust tui", Mark::Web),
            ("mcp__browser-gpu__click", Mark::Web),
            ("mcp__claude-flow__memory_search focus", Mark::Memory),
            ("Agent review the diff", Mark::Agent),
            ("update_plan", Mark::Tool),
            ("write_stdin session 3", Mark::Shell),
            ("TodoWrite", Mark::Tool),
            ("Write docs/a.md", Mark::Edit),
            ("", Mark::Tool),
        ] {
            assert_eq!(Mark::of_tool(text), mark, "{text}");
        }
    }

    #[test]
    fn sessions_sort_by_recency_and_selection_follows_its_session() {
        let mut events = vec![
            event(0, "2026-01-02T10:00:00.000Z", "old", "prompt", "first"),
            event(1, "2026-01-02T11:00:00.000Z", "new", "tool", "Read a"),
            event(2, "2026-01-01T09:00:00.000Z", "yesterday", "tool", "Read b"),
        ];
        let mut state = State::default();
        let built = city(&events, &mut state);
        assert_eq!((built.days.len(), built.sessions.len()), (2, 2));
        assert_eq!(built.sessions[0].id, "new");
        select(&mut state, &built, 0);
        // The other session records newer work and overtakes it.
        events.push(event(
            3,
            "2026-01-02T12:00:00.000Z",
            "old",
            "tool",
            "Bash ls",
        ));
        let built = city(&events, &mut state);
        assert_eq!(built.sessions[0].id, "old");
        assert_eq!(built.sessions[state.selected].id, "new");
        assert_eq!(built.sessions[0].records.len(), 2);
        assert_eq!(built.sessions[0].newest().text, "Bash ls");

        let mut empty = State {
            selected: usize::MAX,
            ..State::default()
        };
        assert!(city(&[], &mut empty).sessions.is_empty());
        assert_eq!(empty.selected, 0);
    }

    #[test]
    fn huge_sessions_compress_to_a_bounded_tower_that_keeps_errors() {
        let mut events: Vec<_> = (0..5000)
            .map(|i| {
                let at = format!(
                    "2026-01-02T10:{:02}:{:02}.{:03}Z",
                    i / 600,
                    (i / 10) % 60,
                    i % 10
                );
                event(
                    i,
                    &at,
                    "big",
                    "tool",
                    if i % 3 == 0 { "Bash x" } else { "Read y" },
                )
            })
            .collect();
        events[2500].kind = "failure".into();
        events.push(event(
            9000,
            "2026-01-02T09:00:00.000Z",
            "small",
            "tool",
            "Edit z",
        ));
        let mut state = State::default();
        let built = city(&events, &mut state);
        let lay = layout(&built, 1.0);
        let scale = tower_scale(&built, &lay);
        let big = built.sessions.iter().find(|s| s.id == "big").unwrap();
        let floors = bands(big, scale);
        assert!(
            floors.len() as f64 <= TOWER_HEIGHT / MIN_BAND + 1.0,
            "{} floors",
            floors.len()
        );
        let total: f64 = floors.iter().map(|(h, _)| h).sum();
        assert!((total - height_cap(&lay)).abs() < 1e-6);
        assert!(floors.iter().any(|(_, mark)| *mark == Mark::Failure));
        let (ground, towers) = build_scene(&built, &lay, 0, 0.0, 0.0, None);
        assert!(ground
            .vertices
            .iter()
            .chain(&towers.vertices)
            .all(|v| v.is_finite()));
        // An uncompressed session keeps one floor per run of like records.
        let small = built.sessions.iter().find(|s| s.id == "small").unwrap();
        assert_eq!(bands(small, 1.0), vec![(0.5, Mark::Edit)]);
    }

    #[test]
    fn layout_caps_towers_and_districts() {
        let events: Vec<_> = (0..60)
            .map(|i| {
                let mut e = event(
                    i,
                    &format!("2026-01-02T10:{i:02}:00.000Z"),
                    &format!("s{i}"),
                    "tool",
                    "Read",
                );
                e.project = format!("/work/p{}", i % 12);
                e
            })
            .collect();
        let built = city(&events, &mut State::default());
        let lay = layout(&built, 1.0);
        assert_eq!(
            (lay.towers.len(), lay.hidden),
            (MAX_TOWERS, 60 - MAX_TOWERS)
        );
        assert_eq!(lay.districts.len(), MAX_DISTRICTS);
        assert_eq!(lay.districts[MAX_DISTRICTS - 1].name, "other projects");
        for tower in &lay.towers {
            assert!(tower.x.abs() <= lay.width / 2.0 && tower.z.abs() <= lay.depth / 2.0);
        }
        assert_eq!(district_name("/home/u/workspace/agentbox/"), "agentbox");
        assert_eq!(district_name("?"), "unknown");
    }

    #[test]
    fn city_names_districts_and_describes_the_selection() {
        let events = demo_events();
        let now = unix_now();
        let mut state = State::default();
        let rows = render(&events, "fixture", &mut state, (165, 35), true, now);
        assert_eq!(rows.len(), 35);
        let text = plain(&rows);
        let built = city(&events, &mut state);
        for name in built.sessions.iter().map(|s| district_name(s.project)) {
            assert!(text.contains(name), "district {name} unnamed");
        }
        let chosen = &built.sessions[state.selected];
        assert!(
            text.contains(&fit(&chosen.newest().text, 40)),
            "callout text missing"
        );
        assert!(text.contains("MOST RECENT") && text.contains('●') && text.contains('▀'));
        assert!(text.contains("FOLLOWING LIVE") && text.contains("DEMO"));
        state.strip = false;
        assert!(!plain(&render(
            &events,
            "fixture",
            &mut state,
            (165, 35),
            true,
            now
        ))
        .contains("MOST RECENT"));
    }

    #[test]
    fn flat_view_lists_sessions_and_every_record_of_the_selected_one() {
        let events = demo_events();
        let now = unix_now();
        let mut state = State {
            flat: true,
            ..State::default()
        };
        let text = plain(&render(
            &events,
            "fixture",
            &mut state,
            (120, 40),
            true,
            now,
        ));
        let built = city(&events, &mut state);
        let chosen = &built.sessions[state.selected];
        assert!(text.contains(&format!("{} records", chosen.records.len())));
        assert!(text.contains("Project:"));
        state.detail = true;
        step_record(&mut state, chosen, isize::MAX); // clamps to the oldest record
        let text = plain(&render(
            &events,
            "fixture",
            &mut state,
            (120, 40),
            true,
            now,
        ));
        assert!(text.contains("Source: synthetic demo"));
        assert_eq!(record_index(&state, chosen), chosen.records.len() - 1);
        assert!(text.contains(chosen.records[0].at.get(11..19).unwrap()));
        // Small panes get the same list rather than a cramped city.
        let small = plain(&render(
            &events,
            "fixture",
            &mut State::default(),
            (47, 27),
            true,
            now,
        ));
        assert!(small.contains("records") && !small.contains("MOST RECENT"));
    }

    #[test]
    fn every_size_renders_full_rows_without_panicking() {
        let events = demo_events();
        let now = unix_now();
        for dims in [
            (1, 1),
            (20, 7),
            (30, 8),
            (47, 27),
            (80, 19),
            (80, 20),
            (80, 24),
            (120, 38),
            (512, 180),
        ] {
            for flat in [false, true] {
                for data in [&events[..], &[]] {
                    let mut state = State {
                        flat,
                        yaw: 1.1,
                        dolly: 0.4,
                        ..State::default()
                    };
                    assert_eq!(
                        render(data, "fixture", &mut state, dims, false, now).len(),
                        dims.1
                    );
                }
            }
        }
    }

    #[test]
    fn tour_follows_active_sessions_and_holds_when_paused() {
        let events = demo_events();
        let now = unix_now();
        let mut state = State::default();
        let built = city(&events, &mut state);
        let live: Vec<usize> = (0..built.sessions.len())
            .filter(|&i| now - built.sessions[i].last < LIVE_SECS)
            .collect();
        assert!(live.len() >= 2, "the demo needs several active sessions");
        let mut visited = BTreeSet::new();
        for _ in 0..200 {
            tour_step(&events, &mut state, 0.5, now);
            visited.insert(state.selected);
        }
        assert_eq!(visited.into_iter().collect::<Vec<_>>(), live);
        state.tour = false;
        let held = (state.selected, state.t);
        tour_step(&events, &mut state, 1.0, now);
        assert_eq!(held, (state.selected, state.t));
    }

    #[test]
    fn clipping_bounds_every_polygon_to_camera_and_screen() {
        let events = demo_events();
        let mut state = State::default();
        let built = city(&events, &mut state);
        let lay = layout(&built, 1.0);
        let (_, towers) = build_scene(&built, &lay, 0, 0.0, unix_now(), None);
        for eye in [Vec3D::new(0.0, 6.0, 9.0), Vec3D::new(30.0, 20.0, 40.0)] {
            let mut viewport = Viewport::new(
                Transform3D::look_at_lh(eye, Vec3D::ZERO, Vec3D::NEG_Y),
                FOV,
                Vec2D::new(100, 32),
            );
            viewport.character_width_multiplier = 1.0;
            let clipped = clip_scene(towers.clone(), &viewport, (200, 65));
            assert!(!clipped.faces.is_empty());
            for point in clipped.vertices {
                let camera = viewport.camera_transform.transform_point3(point);
                assert!(camera.z >= 2.0 - 1e-8);
                let p = project(&viewport, Transform3D::IDENTITY, point);
                assert!((-1..=201).contains(&p.x) && (-1..=66).contains(&p.y));
            }
        }
    }

    #[test]
    fn the_whole_city_stays_in_frame_at_every_size_sway_and_selection() {
        let many: Vec<_> = (0..60)
            .map(|i| {
                let at = format!("2026-01-02T10:{:02}:00.000Z", i % 60);
                let mut e = event(i, &at, &format!("s{i}"), "tool", "Read");
                e.project = format!("/work/p{}", i % 12);
                e
            })
            .collect();
        let few: Vec<_> = (0..3)
            .map(|i| {
                let at = format!("2026-01-02T10:0{i}:00.000Z");
                event(i, &at, &format!("s{i}"), "tool", "Bash x")
            })
            .collect();
        for events in [demo_events(), many, few] {
            for dims in [
                (80usize, 20usize),
                (120, 38),
                (165, 35),
                (100, 60),
                (90, 180),
                (512, 180),
            ] {
                for selected in [0, 2] {
                    // t = 14.28 and 42.8 put the sway near its two extremes.
                    for t in [0.0, 14.28, 42.8] {
                        let mut state = State {
                            t,
                            ..State::default()
                        };
                        let built = city(&events, &mut state);
                        select(&mut state, &built, selected.min(built.sessions.len() - 1));
                        let rows = render(&events, "fixture", &mut state, dims, false, 0.0);
                        assert_eq!(rows.len(), dims.1);
                        // The same camera render_city uses.
                        let strip = (built.sessions.len().min(5) as i64 + 1).min(dims.1 as i64 / 3);
                        let bottom = dims.1 as i64 - 1 - strip;
                        let (centre, frame) = scene_frame(dims, 2, bottom);
                        let (lay, tallest, fitted) = city_view(&built, 0.0, 0.0, centre, frame);
                        let (width, height) = PixelCanvas::samples_for(dims.0, dims.1);
                        let (width, cell) = (width as i64, height as i64 / dims.1 as i64);
                        let angle = SWAY * (t * 0.11).sin();
                        let viewport = city_camera(tallest, angle, fitted, 0.0, centre);
                        for corner in city_corners(&lay, tallest) {
                            let p = project(&viewport, Transform3D::IDENTITY, corner);
                            assert!(
                                p.x >= 0 && p.x <= width && p.y >= 2 * cell && p.y <= bottom * cell,
                                "{dims:?} t {t} corner {corner:?} at {p:?} outside {width}x{}",
                                bottom * cell
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn days_are_kept_by_date_and_records_by_key() {
        let mut events = vec![
            event(0, "2026-01-01T10:00:00.000Z", "a", "prompt", "old day"),
            event(1, "2026-01-02T10:00:00.000Z", "b", "tool", "Read 1"),
            event(2, "2026-01-02T10:01:00.000Z", "b", "tool", "Read 2"),
            event(3, "2026-01-02T10:02:00.000Z", "b", "tool", "Read 3"),
        ];
        let mut state = State::default();
        let built = city(&events, &mut state);
        assert_eq!(built.day, "2026-01-02");
        step_day(&mut state, &built, -1); // already newest: nothing changes
        assert_eq!(state.day, None);
        step_day(&mut state, &built, 1);
        assert_eq!(state.day.as_deref(), Some("2026-01-01"));
        // A new day arrives at midnight; the chosen day stays put.
        events.push(event(
            4,
            "2026-01-03T00:00:01.000Z",
            "c",
            "tool",
            "Read new",
        ));
        let built = city(&events, &mut state);
        assert_eq!((built.day.as_str(), built.index), ("2026-01-01", 2));
        step_day(&mut state, &built, 1); // already oldest
        assert_eq!(state.day.as_deref(), Some("2026-01-01"));
        step_day(&mut state, &built, -2);
        assert_eq!(state.day, None, "back to following the newest day");

        // Records: select the second newest, then newer records arrive.
        state.day = Some("2026-01-02".into());
        let built = city(&events, &mut state);
        let session = &built.sessions[0];
        step_record(&mut state, session, 1);
        assert_eq!(state.record.as_deref(), Some("00002"));
        step_record(&mut state, session, 99);
        assert_eq!(
            state.record.as_deref(),
            Some("00001"),
            "clamped to the oldest"
        );
        step_record(&mut state, session, -99);
        assert_eq!(state.record, None, "newest");
        step_record(&mut state, session, 1);
        events.push(event(5, "2026-01-02T10:03:00.000Z", "b", "tool", "Read 4"));
        let built = city(&events, &mut state);
        assert_eq!(
            record_index(&state, &built.sessions[0]),
            2,
            "selection stays on its record"
        );
    }

    #[test]
    fn home_is_shortened_only_as_a_whole_path_prefix() {
        assert_eq!(tidy_home("Read /home/dev/x.rs", "/home/dev"), "Read ~/x.rs");
        assert_eq!(tidy_home("cd /home/dev", "/home/dev"), "cd ~");
        assert_eq!(
            tidy_home("Read /home/devuser/x", "/home/dev"),
            "Read /home/devuser/x"
        );
        assert_eq!(tidy_home("/srv/home/dev/x", "/home/dev"), "/srv/home/dev/x");
        assert_eq!(tidy_home("a=/home/dev/b c", "/home/dev"), "a=~/b c");
    }

    #[test]
    fn a_merged_floor_with_an_error_and_a_commit_shows_both() {
        let events: Vec<_> = (0..4)
            .map(|i| {
                let kind = ["failure", "receipt", "tool", "tool"][i];
                event(
                    i,
                    &format!("2026-01-02T10:00:0{i}.000Z"),
                    "s",
                    kind,
                    "Read x",
                )
            })
            .collect();
        let built = city(&events, &mut State::default());
        let floors = bands(&built.sessions[0], 0.1); // everything merges into one floor
        let marks: Vec<Mark> = floors.iter().map(|(_, m)| *m).collect();
        assert_eq!(marks, vec![Mark::Failure, Mark::Receipt]);
        let total: f64 = floors.iter().map(|(h, _)| h).sum();
        assert!((total - (0.4 + 0.4 + 0.5 + 0.5) * 0.1).abs() < 1e-9);
    }

    #[test]
    fn a_stuck_session_is_marked_in_the_strip_and_the_list_at_every_size() {
        // The demo's 77e385bc session repeats one failing command three times.
        let events = demo_events();
        let now = unix_now();
        for (dims, flat) in [
            ((120, 38), false),
            ((165, 35), false),
            ((200, 65), false),
            ((47, 27), true),
            ((80, 24), true),
            ((120, 40), true),
        ] {
            let mut state = State {
                flat,
                ..State::default()
            };
            let text = plain(&render(&events, "fixture", &mut state, dims, true, now));
            let marked: Vec<&str> = text.lines().filter(|l| l.contains('↻')).collect();
            assert!(
                marked.iter().any(|l| l.contains("codex")),
                "{dims:?} flat={flat}: no stuck marker on the codex session"
            );
            let stuck_rows = marked.iter().filter(|l| !l.contains("stuck")).count();
            assert_eq!(stuck_rows, 1, "{dims:?}: only the stuck session is marked");
        }
    }
}
