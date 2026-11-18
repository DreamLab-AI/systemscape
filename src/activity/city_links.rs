//! What ties the city's towers together, and what stood there the day before.
//!
//! Three overlays, each with its own key:
//!
//! - **Delegation arcs** (`b`) join a session's tower to the towers of the
//!   subagents it started, so delegated work reads as part of its parent.
//! - **Shared-file roads** (`c`) run on the ground between two sessions that
//!   both edited or wrote the same file on the day shown: the place two agents
//!   can trip over each other. A road is red while both sessions are active.
//! - **Ghost skyline** (`g`) outlines the previous recorded day's towers behind
//!   today's, at today's scale, so drift from one day to the next shows.
//!
//! Everything here is derived from the event slice and a `now` passed in, never
//! from the clock, so a replay can hand in a cut-off history and an old `now`.
//! All geometry stays inside the box `city_corners` describes, so the camera
//! fit, which only knows about that box, keeps the whole scene in frame.
use super::{
    district_name, fit, raw_height, short, tidy, City, Layout, Mark, Session, LIVE_SECS,
    MAX_TOWERS, MUTED,
};
use crate::activity_data::{clean, Event};
use crate::insight::whole_path;
use crate::pixels::{mix, PixelCanvas, Rgb};
use crate::{ColChar, Colour, Face, Mesh3D, Modifier, Vec3D};
use crossterm::event::KeyCode;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Roads drawn at most; the rest are counted in the key.
pub(super) const MAX_ROADS: usize = 6;
/// Scene rows below which arcs, roads and the key would only add clutter.
const MIN_LINK_ROWS: i64 = 8;
/// Scene rows below which the ghost skyline is left out.
const MIN_GHOST_ROWS: i64 = 12;
/// Distance from a tower's centre to the lane its roads run along: past the
/// tower's side, inside the gap between columns or the plate's margin.
const LANE: f64 = 1.55;
const ARC: Rgb = (255, 120, 170);
const ROAD_LIVE: Rgb = (255, 70, 85);
const ROAD_IDLE: Rgb = (132, 104, 112);
const GHOST: Rgb = (128, 150, 184);
const OFF: Rgb = (70, 82, 100);

/// Which overlays are on. All start on; each key flips one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Show {
    pub bridges: bool,
    pub roads: bool,
    pub ghost: bool,
}

impl Default for Show {
    fn default() -> Self {
        Self {
            bridges: true,
            roads: true,
            ghost: true,
        }
    }
}

/// Flip the overlay a key belongs to. Returns whether the key was one of
/// them, so the caller can stop there: a view toggle, like `h`, leaves the
/// tour flying.
pub(super) fn toggle(show: &mut Show, key: KeyCode) -> bool {
    match key {
        KeyCode::Char('b') => show.bridges = !show.bridges,
        KeyCode::Char('c') => show.roads = !show.roads,
        KeyCode::Char('g') => show.ghost = !show.ghost,
        _ => return false,
    }
    true
}

/// A short session name that tells a subagent from its parent: the first
/// eight characters of the parent's ID and six of the agent's.
pub(super) fn short_id(id: &str) -> String {
    match id.split_once('/') {
        Some((parent, child)) => format!("{}/{}", take(parent, 8), take(child, 6)),
        None => take(id, 8),
    }
}

fn take(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// The session that delegated this one, if it is a subagent.
pub(super) fn parent_of<'a>(session: &Session<'a>) -> Option<&'a str> {
    session.records.iter().find_map(|e| e.parent.as_deref())
}

/// The agent column of the strip and the session table: a subagent is marked
/// and named by its own ID, so it does not read as a second copy of its parent.
pub(super) fn who(session: &Session<'_>) -> String {
    match session.id.split_once('/') {
        Some((_, child)) if parent_of(session).is_some() => {
            // Six characters keep `↳claude 7d1e0c` inside the session
            // table's fourteen-column agent field.
            format!("↳{} {}", session.agent, take(child, 6))
        }
        _ => session.agent.to_owned(),
    }
}

/// Two drawn sessions that edited or wrote the same files on the day shown.
#[derive(Debug, PartialEq)]
pub(super) struct Road<'a> {
    /// Session indices, `a < b`.
    pub a: usize,
    pub b: usize,
    /// One edit record per shared file, in path order. Paths are compared
    /// whole, as the summary compares them ([`whole_path`]), so the city and
    /// the summary agree on what is contended.
    pub files: Vec<&'a Event>,
    /// Both sessions have a record within `LIVE_SECS` of `now`.
    pub live: bool,
}

/// The previous recorded day's skyline: unscaled session heights per district.
#[derive(Debug, PartialEq)]
pub(super) struct Ghost {
    pub day: String,
    /// District name and its sessions' heights, tallest first.
    pub districts: Vec<(String, Vec<f64>)>,
}

/// Everything the overlays need, derived once per frame.
pub(super) struct Links<'a> {
    pub show: Show,
    /// The moment the history is seen at: a road is red, and an arc bright,
    /// while its sessions have records within `LIVE_SECS` of it.
    pub now: f64,
    /// (parent, child) session indices, both drawn.
    pub bridges: Vec<(usize, usize)>,
    /// Every contended pair among the drawn sessions, most pressing first.
    pub roads: Vec<Road<'a>>,
    pub ghost: Option<Ghost>,
}

impl<'a> Links<'a> {
    /// Derive the overlays that are switched on. `events` is the history the
    /// city was built from and `now` the moment it is seen at.
    pub fn build(events: &'a [Event], city: &City<'a>, show: Show, now: f64) -> Self {
        Self {
            show,
            now,
            bridges: if show.bridges {
                bridges(city)
            } else {
                Vec::new()
            },
            roads: if show.roads {
                roads(city, now)
            } else {
                Vec::new()
            },
            ghost: show.ghost.then(|| ghost(events, city)).flatten(),
        }
    }
}

/// Parent-to-subagent links where both sessions are among the drawn towers,
/// which are always the first `MAX_TOWERS` sessions.
fn bridges(city: &City<'_>) -> Vec<(usize, usize)> {
    let drawn = &city.sessions[..city.sessions.len().min(MAX_TOWERS)];
    let mut out = Vec::new();
    for (child, session) in drawn.iter().enumerate() {
        let Some(parent) = parent_of(session) else {
            continue;
        };
        if let Some(found) = drawn
            .iter()
            .position(|s| s.agent == session.agent && s.id == parent)
        {
            out.push((found, child));
        }
    }
    out
}

/// Pairs of drawn sessions that both edited or wrote a file, live pairs
/// first, then those sharing the most files. The work is bounded by the drawn
/// towers' own records.
fn roads<'a>(city: &City<'a>, now: f64) -> Vec<Road<'a>> {
    let drawn = city.sessions.len().min(MAX_TOWERS);
    // Codex records paths relative to the project and Claude records them
    // whole; both are made whole before comparing, as the summary does.
    let mut editors: BTreeMap<String, (&'a Event, BTreeSet<usize>)> = BTreeMap::new();
    for (i, session) in city.sessions[..drawn].iter().enumerate() {
        for record in &session.records {
            if Mark::of(record) == Mark::Edit {
                if let Some(path) = whole_path(record) {
                    editors
                        .entry(path)
                        .or_insert((record, BTreeSet::new()))
                        .1
                        .insert(i);
                }
            }
        }
    }
    let mut pairs: BTreeMap<(usize, usize), Vec<&'a Event>> = BTreeMap::new();
    for (record, sessions) in editors.values() {
        let sessions: Vec<usize> = sessions.iter().copied().collect();
        for (n, &a) in sessions.iter().enumerate() {
            for &b in &sessions[n + 1..] {
                pairs.entry((a, b)).or_default().push(record);
            }
        }
    }
    let live = |i: usize| now - city.sessions[i].last < LIVE_SECS;
    let mut out: Vec<Road<'a>> = pairs
        .into_iter()
        .map(|((a, b), files)| Road {
            a,
            b,
            files,
            live: live(a) && live(b),
        })
        .collect();
    out.sort_by(|x, y| {
        y.live
            .cmp(&x.live)
            .then(y.files.len().cmp(&x.files.len()))
            .then((x.a, x.b).cmp(&(y.a, y.b)))
    });
    out
}

/// The skyline of the recorded day before the one shown: its most recent
/// `MAX_TOWERS` sessions, grouped by district as today's are.
fn ghost(events: &[Event], city: &City<'_>) -> Option<Ghost> {
    let day = *city.days.get(city.index + 1)?;
    // (agent, session) -> (unscaled height, newest timestamp, its project)
    let mut sessions: HashMap<(&str, &str), (f64, &str, &str)> = HashMap::new();
    for event in events.iter().filter(|e| e.at.starts_with(day)) {
        let entry = sessions
            .entry((&event.agent, &event.session))
            .or_insert((0.0, "", ""));
        entry.0 += Mark::of(event).weight();
        if event.at.as_str() >= entry.1 {
            entry.1 = &event.at;
            entry.2 = &event.project;
        }
    }
    let mut sessions: Vec<_> = sessions.into_iter().collect();
    sessions.sort_by(|(ka, a), (kb, b)| b.1.cmp(a.1).then(ka.cmp(kb)));
    let mut districts: Vec<(String, Vec<f64>)> = Vec::new();
    for (_, (height, _, project)) in sessions.into_iter().take(MAX_TOWERS) {
        let name = district_name(project);
        match districts.iter_mut().find(|(n, _)| n == name) {
            Some((_, heights)) => heights.push(height),
            None => districts.push((name.to_owned(), vec![height])),
        }
    }
    for (_, heights) in &mut districts {
        heights.sort_by(|a, b| b.total_cmp(a));
    }
    Some(Ghost {
        day: day.to_owned(),
        districts,
    })
}

/// What the overlays put in the scene, for the key.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Drawn {
    /// The pane had room for arcs, roads and the key.
    pub room: bool,
    pub bridges: usize,
    pub roads: usize,
    /// Contended pairs beyond `MAX_ROADS`.
    pub more: usize,
    /// The previous day and how many of its towers are drawn.
    pub ghost: Option<(String, usize)>,
    /// Ghost towers cut at today's tallest; ghost sessions with no district
    /// in today's city, or no room in theirs.
    pub cut: usize,
    pub unplaced: usize,
}

/// The ghost skyline and the roads as their own meshes (the ghost drawn as a
/// wireframe, both before the towers so towers stand in front), and the arcs
/// added to the towers' mesh so they sort among the floors.
pub(super) fn scene(
    city: &City<'_>,
    links: &Links<'_>,
    lay: &Layout,
    scale: f64,
    tallest: f64,
    scene_rows: i64,
    towers: &mut Mesh3D,
) -> (Mesh3D, Mesh3D, Drawn) {
    let mut ghost_mesh = Mesh3D::new(Vec::new(), Vec::new());
    let mut road_mesh = Mesh3D::new(Vec::new(), Vec::new());
    let mut drawn = Drawn::default();
    if scene_rows < MIN_LINK_ROWS {
        return (ghost_mesh, road_mesh, drawn);
    }
    drawn.room = true;
    let place = |session: usize| {
        lay.towers
            .iter()
            .find(|t| t.session == session)
            .map(|t| (t.x, t.z))
    };
    let live = |session: usize| links.now - city.sessions[session].last < LIVE_SECS;
    let top = |session: usize| raw_height(&city.sessions[session]) * scale;

    for &(parent, child) in &links.bridges {
        if let (Some(from), Some(to)) = (place(parent), place(child)) {
            let colour = if live(child) {
                ARC
            } else {
                mix(ARC, (44, 52, 68), 0.5)
            };
            arc(
                towers,
                (from, top(parent)),
                (to, top(child)),
                tallest,
                lay.stretch,
                colour,
            );
            drawn.bridges += 1;
        }
    }

    for (n, road) in links.roads.iter().take(MAX_ROADS).enumerate() {
        if let (Some(a), Some(b)) = (place(road.a), place(road.b)) {
            let colour = if road.live { ROAD_LIVE } else { ROAD_IDLE };
            street(&mut road_mesh, a, b, n, lay.stretch, colour);
            drawn.roads += 1;
        }
    }
    drawn.more = links.roads.len() - drawn.roads;

    if let Some(ghost) = links
        .ghost
        .as_ref()
        .filter(|_| scene_rows >= MIN_GHOST_ROWS)
    {
        let (placed, cut, unplaced) = skyline(&mut ghost_mesh, ghost, lay, scale, tallest);
        drawn.ghost = Some((ghost.day.clone(), placed));
        drawn.cut = cut;
        drawn.unplaced = unplaced;
    }
    (ghost_mesh, road_mesh, drawn)
}

fn fill(c: Rgb) -> ColChar {
    ColChar::SOLID.with_mod(Modifier::Colour(Colour::rgb(c.0, c.1, c.2)))
}

/// Push a face in both windings: the engine culls back faces, and a ribbon
/// should show from either side.
fn two_sided(mesh: &mut Mesh3D, corners: [Vec3D; 4], c: Rgb) {
    let b = mesh.vertices.len();
    mesh.vertices.extend(corners);
    mesh.faces
        .push(Face::new(vec![b, b + 1, b + 2, b + 3], fill(c)));
    mesh.faces
        .push(Face::new(vec![b + 3, b + 2, b + 1, b], fill(c)));
}

/// A skywalk from roof to roof, rising towards the middle but never above
/// `ceiling` (today's tallest tower), so it stays inside the fitted box. It is
/// built of short pieces because the engine orders faces by their centres,
/// and one long ribbon would sort wrongly against the floors it passes.
fn arc(
    mesh: &mut Mesh3D,
    from: ((f64, f64), f64),
    to: ((f64, f64), f64),
    ceiling: f64,
    stretch: f64,
    c: Rgb,
) {
    let (((x0, z0), y0), ((x1, z1), y1)) = (from, to);
    let length = (x1 - x0).hypot(z1 - z0);
    if length < 1e-6 {
        return;
    }
    let high = y0.max(y1);
    let rise = (high + (0.6 * length).max(3.0)).min(ceiling) - high;
    let pieces = (length / 0.7).ceil().clamp(4.0, 24.0) as usize;
    let half = 0.45 * stretch;
    let (nx, nz) = (-(z1 - z0) / length * half, (x1 - x0) / length * half);
    let point = |s: f64| {
        let y = y0 + (y1 - y0) * s + rise.max(0.0) * 4.0 * s * (1.0 - s);
        (x0 + (x1 - x0) * s, y, z0 + (z1 - z0) * s)
    };
    let side = (0.55 * c.0 as f64, 0.55 * c.1 as f64, 0.55 * c.2 as f64);
    let side = (side.0 as u8, side.1 as u8, side.2 as u8);
    for k in 0..pieces {
        let (a, b) = (
            point(k as f64 / pieces as f64),
            point((k + 1) as f64 / pieces as f64),
        );
        two_sided(
            mesh,
            [
                Vec3D::new(a.0 + nx, a.1, a.2 + nz),
                Vec3D::new(b.0 + nx, b.1, b.2 + nz),
                Vec3D::new(b.0 - nx, b.1, b.2 - nz),
                Vec3D::new(a.0 - nx, a.1, a.2 - nz),
            ],
            c,
        );
        let drop = 0.35_f64.min(a.1.min(b.1));
        two_sided(
            mesh,
            [
                Vec3D::new(a.0, a.1, a.2),
                Vec3D::new(b.0, b.1, b.2),
                Vec3D::new(b.0, b.1 - drop, b.2),
                Vec3D::new(a.0, a.1 - drop, a.2),
            ],
            side,
        );
    }
}

/// A flat strip on the ground from `a` to `b`, extended at both ends by its
/// half width so consecutive strips join at corners. Wound like `quad`, so
/// it faces up.
fn strip(mesh: &mut Mesh3D, a: (f64, f64), b: (f64, f64), half: f64, c: Rgb) {
    let length = (b.0 - a.0).hypot(b.1 - a.1);
    if length < 1e-6 {
        return;
    }
    let (dx, dz) = ((b.0 - a.0) / length * half, (b.1 - a.1) / length * half);
    let (p, q) = ((a.0 - dx, a.1 - dz), (b.0 + dx, b.1 + dz));
    let mut corners = [
        (p.0 - dz, p.1 + dx),
        (q.0 - dz, q.1 + dx),
        (q.0 + dz, q.1 - dx),
        (p.0 + dz, p.1 - dx),
    ];
    // `quad` winds counter-clockwise in (x, z); match it so the face is kept.
    let area: f64 = (0..4)
        .map(|i| {
            let (u, v) = (corners[i], corners[(i + 1) % 4]);
            u.0 * v.1 - v.0 * u.1
        })
        .sum();
    if area < 0.0 {
        corners.reverse();
    }
    let base = mesh.vertices.len();
    mesh.vertices
        .extend(corners.iter().map(|(x, z)| Vec3D::new(*x, 0.02, *z)));
    mesh.faces
        .push(Face::new(vec![base, base + 1, base + 2, base + 3], fill(c)));
}

/// A road between two towers' feet. Towers in one column stand front to
/// back, so a straight road would hide behind the front one: the road leaves
/// sideways into the lane beside the column, runs along it, and turns in.
/// Roads within a column alternate sides so two of them stay apart.
fn street(mesh: &mut Mesh3D, a: (f64, f64), b: (f64, f64), n: usize, stretch: f64, c: Rgb) {
    let side = if (b.0 - a.0).abs() > 1e-6 {
        (b.0 - a.0).signum()
    } else if n.is_multiple_of(2) {
        -1.0
    } else {
        1.0
    };
    let lane = a.0 + side * LANE * stretch;
    let half = 0.2 * stretch;
    for (p, q) in [
        (a, (lane, a.1)),
        ((lane, a.1), (lane, b.1)),
        ((lane, b.1), b),
    ] {
        strip(mesh, p, q, half, c);
    }
}

/// The ghost towers: one outline per session of the previous day, standing
/// in a row behind today's district of the same name, tallest first. A ghost
/// taller than today's tallest tower is cut at that height and drawn with a
/// notched top, so the fitted camera never has to back away from today's
/// city for yesterday's. Returns towers placed, cut, and left out.
fn skyline(
    mesh: &mut Mesh3D,
    ghost: &Ghost,
    lay: &Layout,
    scale: f64,
    ceiling: f64,
) -> (usize, usize, usize) {
    let z = -lay.depth / 2.0 - 1.0; // behind the plates, inside the fitted box
    let (mut placed, mut cut, mut unplaced) = (0, 0, 0);
    for (name, heights) in &ghost.districts {
        let Some(district) = lay.districts.iter().find(|d| &d.name == name) else {
            unplaced += heights.len();
            continue;
        };
        let inset = 0.3 * lay.stretch;
        let inner = (district.left - district.right - 2.0 * inset).max(0.0);
        let room = ((inner / (0.6 * lay.stretch)).floor() as usize).max(1);
        let shown = heights.len().min(room);
        unplaced += heights.len() - shown;
        let slot = inner / shown as f64;
        let half = (slot * 0.36).min(lay.stretch);
        for (k, raw) in heights.iter().take(shown).enumerate() {
            let x = district.left - inset - (k as f64 + 0.5) * slot;
            let mut height = raw * scale;
            let mut top = vec![
                Vec3D::new(x + half, height.min(ceiling), z),
                Vec3D::new(x - half, height.min(ceiling), z),
            ];
            if height > ceiling {
                height = ceiling;
                cut += 1;
                top.insert(1, Vec3D::new(x, height - (0.4 * height).min(1.0), z));
            }
            let b = mesh.vertices.len();
            mesh.vertices.push(Vec3D::new(x + half, 0.0, z));
            mesh.vertices.extend(top);
            mesh.vertices.push(Vec3D::new(x - half, 0.0, z));
            mesh.faces
                .push(Face::new((b..mesh.vertices.len()).collect(), fill(GHOST)));
            placed += 1;
        }
    }
    (placed, cut, unplaced)
}

/// A path as shown: relative to the project when inside it, cleaned of
/// controls and directional formatting, home shortened to `~`.
fn display_path(path: &str, project: &str) -> String {
    let relative = path
        .strip_prefix(project)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path);
    tidy(&clean(relative))
}

/// Key for the overlays, on the row under the legend, right-aligned after
/// column `from`. Each item names its key; a switched-off one is dimmed. A
/// short form is used when the long one does not fit, and nothing at all
/// when neither does or the pane had no room for the overlays.
pub(super) fn key(
    canvas: &mut PixelCanvas,
    links: &Links<'_>,
    drawn: &Drawn,
    row: i64,
    from: i64,
    columns: i64,
) {
    if !drawn.room {
        return;
    }
    let show = links.show;
    let roads = if drawn.more > 0 {
        format!("{} +{} more", drawn.roads, drawn.more)
    } else {
        drawn.roads.to_string()
    };
    let ghost = match &drawn.ghost {
        Some((day, _)) => {
            let mut text = day.get(5..).unwrap_or(day).to_owned();
            if drawn.cut > 0 {
                text += &format!(" ∨{} cut", drawn.cut);
            }
            if drawn.unplaced > 0 {
                text += &format!(" +{} unplaced", drawn.unplaced);
            }
            text
        }
        None if show.ghost && links.ghost.is_none() => "no earlier day".into(),
        None if show.ghost => "needs a taller pane".into(),
        None => String::new(),
    };
    let long = [
        (
            show.bridges,
            ARC,
            "⌒ b",
            format!("delegation {}", drawn.bridges),
        ),
        (
            show.roads,
            ROAD_LIVE,
            "═ c",
            format!("shared files {roads}"),
        ),
        (show.ghost, GHOST, "▯ g", format!("ghost {ghost}")),
    ];
    let short = [
        (show.bridges, ARC, "b⌒", drawn.bridges.to_string()),
        (show.roads, ROAD_LIVE, "c═", roads.replace(" more", "")),
        (show.ghost, GHOST, "g▯", String::new()),
    ];
    for items in [long, short] {
        let parts: Vec<(Rgb, String)> = items
            .into_iter()
            .map(|(on, hue, glyph, text)| {
                if on {
                    (hue, format!("{glyph} {text}").trim_end().to_owned())
                } else {
                    (OFF, format!("{glyph} off"))
                }
            })
            .collect();
        let width: i64 = parts
            .iter()
            .map(|(_, t)| t.chars().count() as i64 + 2)
            .sum::<i64>()
            - 2;
        let start = columns - 1 - width;
        if start <= from {
            continue;
        }
        let mut col = start;
        for (hue, text) in parts {
            canvas.label(col, row, &text, mix(hue, MUTED, 0.35));
            col += text.chars().count() as i64 + 2;
        }
        return;
    }
}

/// One line for the detail view about the selected session's links: whom it
/// was delegated by, how many subagents it started, and the files it shares
/// with other drawn sessions.
pub(super) fn note(
    city: &City<'_>,
    links: &Links<'_>,
    selected: usize,
    width: usize,
) -> Option<String> {
    let session = city.sessions.get(selected)?;
    let mut parts = Vec::new();
    if let Some(parent) = parent_of(session) {
        parts.push(format!("subagent of {} {}", session.agent, clean(parent)));
    }
    let children = links.bridges.iter().filter(|(p, _)| *p == selected).count();
    if children > 0 {
        parts.push(format!(
            "started {children} subagent{}",
            if children == 1 { "" } else { "s" }
        ));
    }
    let mine: Vec<&Road> = links
        .roads
        .iter()
        .filter(|r| r.a == selected || r.b == selected)
        .collect();
    if let Some(first) = mine.first() {
        let other = &city.sessions[if first.a == selected {
            first.b
        } else {
            first.a
        }];
        let files: usize = mine.iter().map(|r| r.files.len()).sum();
        let mut text = format!(
            "shares {} with {} {}{}",
            display_path(
                &whole_path(first.files[0]).unwrap_or_default(),
                session.project
            ),
            other.agent,
            short(other.id),
            if first.live { " (both active)" } else { "" }
        );
        if files > 1 {
            text += &format!(" · {} more shared", files - 1);
        }
        parts.push(text);
    }
    (!parts.is_empty()).then(|| fit(&parts.join(" · "), width))
}

#[cfg(test)]
mod tests {
    use super::super::{
        city, city_corners, city_view, handle_key, render, scene_frame, select, unix_now, Action,
        State,
    };
    use super::*;
    use crate::activity_data::demo_events;
    use crossterm::event::{KeyEvent, KeyModifiers};

    const ALL_OFF: Show = Show {
        bridges: false,
        roads: false,
        ghost: false,
    };

    fn record(key: usize, at: &str, session: &str, text: &str, path: Option<&str>) -> Event {
        Event {
            key: format!("{key:05}"),
            at: at.into(),
            agent: "claude".into(),
            session: session.into(),
            project: "/work/alpha".into(),
            kind: if text.is_empty() { "prompt" } else { "tool" }.into(),
            text: text.into(),
            source: "fixture".into(),
            parent: session.split_once('/').map(|(p, _)| p.into()),
            path: path.map(Into::into),
        }
    }

    fn at(day: u32, minute: u32) -> String {
        format!("2026-03-{day:02}T10:{minute:02}:00.000Z")
    }

    fn noon(day: u32) -> f64 {
        super::super::seconds(&at(day, 30))
    }

    #[test]
    fn a_subagent_has_a_name_of_its_own() {
        assert_eq!(short_id("93bc1f6c-demo"), "93bc1f6c");
        assert_eq!(short_id("93bc1f6c-demo/7d1e0c55aa"), "93bc1f6c/7d1e0c");
        assert_ne!(
            short_id("93bc1f6c-demo"),
            short_id("93bc1f6c-demo/7d1e0c55")
        );
        let events = [
            record(0, &at(2, 0), "p", "Read a", None),
            record(1, &at(2, 1), "p/kid", "Read b", None),
        ];
        let built = city(&events, &mut State::default());
        let kid = built.sessions.iter().find(|s| s.id == "p/kid").unwrap();
        let parent = built.sessions.iter().find(|s| s.id == "p").unwrap();
        assert_eq!(
            (who(kid).as_str(), who(parent).as_str()),
            ("↳claude kid", "claude")
        );
        assert_eq!(parent_of(kid), Some("p"));
    }

    #[test]
    fn a_bridge_joins_a_parent_to_each_drawn_subagent_only() {
        let events = [
            record(0, &at(2, 0), "p", "Read a", None),
            record(1, &at(2, 1), "p/one", "Read b", None),
            record(2, &at(2, 2), "p/two", "Read c", None),
            // A subagent whose parent worked on another day has nothing to join.
            record(3, &at(2, 3), "q/orphan", "Read d", None),
            record(4, &at(1, 3), "q", "Read e", None),
        ];
        let built = city(&events, &mut State::default());
        let mut found: Vec<(&str, &str)> = bridges(&built)
            .into_iter()
            .map(|(p, c)| (built.sessions[p].id, built.sessions[c].id))
            .collect();
        found.sort();
        assert_eq!(found, vec![("p", "p/one"), ("p", "p/two")]);
    }

    #[test]
    fn sessions_editing_one_file_share_a_road_that_is_red_only_while_both_are_active() {
        let events = [
            record(0, &at(2, 0), "a", "Edit x.rs", Some("/work/alpha/x.rs")),
            record(1, &at(2, 25), "b", "Write x.rs", Some("/work/alpha/x.rs")),
            record(2, &at(2, 26), "b", "Edit y.rs", Some("/work/alpha/y.rs")),
            // Reading a file is not contention, nor is editing it on another day.
            record(3, &at(2, 27), "c", "Read x.rs", Some("/work/alpha/x.rs")),
            record(4, &at(1, 0), "d", "Edit x.rs", Some("/work/alpha/x.rs")),
        ];
        let built = city(&events, &mut State::default());
        let id = |i: usize| built.sessions[i].id;
        let found = roads(&built, noon(2));
        assert_eq!(found.len(), 1);
        let road = &found[0];
        assert_eq!(
            (BTreeSet::from([id(road.a), id(road.b)]), paths(road)),
            (
                BTreeSet::from(["a", "b"]),
                vec!["/work/alpha/x.rs".to_string()]
            )
        );
        // At 10:30 "b" is active but "a" last worked half an hour ago.
        assert!(!road.live);
        // Once "a" records again, both are active and the road turns red.
        let early = super::super::seconds(&at(2, 26)) + 1.0;
        let mut busy = events.to_vec();
        busy.push(record(5, &at(2, 26), "a", "Read z", None));
        let built = city(&busy, &mut State::default());
        assert!(roads(&built, early)[0].live);
    }

    fn paths(road: &Road<'_>) -> Vec<String> {
        road.files.iter().filter_map(|e| whole_path(e)).collect()
    }

    #[test]
    fn a_codex_relative_path_and_a_claude_whole_path_to_one_file_share_a_road() {
        let mut events = [
            record(
                0,
                &at(2, 0),
                "claude-s",
                "Edit src/x.rs",
                Some("/work/alpha/src/x.rs"),
            ),
            record(
                1,
                &at(2, 1),
                "codex-s",
                "apply_patch src/x.rs",
                Some("./src/x.rs"),
            ),
            // Same relative name in another project is another file.
            record(
                2,
                &at(2, 2),
                "other",
                "apply_patch src/x.rs",
                Some("src/x.rs"),
            ),
        ];
        events[1].agent = "codex".into();
        events[2].agent = "codex".into();
        events[2].project = "/work/beta".into();
        let built = city(&events, &mut State::default());
        let found = roads(&built, noon(2));
        assert_eq!(found.len(), 1, "{found:?}");
        let ids = BTreeSet::from([built.sessions[found[0].a].id, built.sessions[found[0].b].id]);
        assert_eq!(ids, BTreeSet::from(["claude-s", "codex-s"]));
        assert_eq!(paths(&found[0]), vec!["/work/alpha/src/x.rs".to_string()]);
    }

    #[test]
    fn roads_and_the_summary_agree_on_contended_files_in_the_demo() {
        let events = demo_events();
        let built = city(&events, &mut State::default());
        assert!(
            built.sessions.len() <= MAX_TOWERS,
            "every demo session is drawn"
        );
        let mut from_roads: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for road in roads(&built, unix_now()) {
            for path in paths(&road) {
                for i in [road.a, road.b] {
                    let s = &built.sessions[i];
                    from_roads
                        .entry(path.clone())
                        .or_default()
                        .insert(format!("{} {}", s.agent, s.id));
                }
            }
        }
        let from_summary: BTreeMap<String, BTreeSet<String>> =
            crate::insight::contended(&events, &built.day)
                .into_iter()
                .map(|c| (c.path, c.sessions.into_iter().collect()))
                .collect();
        assert!(!from_summary.is_empty(), "the demo has a contended file");
        assert_eq!(from_roads, from_summary);
    }

    #[test]
    fn roads_are_capped_and_the_rest_counted() {
        // Eight sessions all editing one file: 28 pairs, six drawn.
        let events: Vec<Event> = (0..8)
            .map(|i| record(i, &at(2, i as u32), &format!("s{i}"), "Edit f", Some("/f")))
            .collect();
        let mut state = State::default();
        let built = city(&events, &mut state);
        let links = Links::build(&events, &built, Show::default(), noon(2));
        assert_eq!(links.roads.len(), 28);
        let (centre, frame) = scene_frame((165, 50), 2, 40);
        let (lay, tallest, _) = city_view(&built, 0.0, 0.0, centre, frame);
        let scale = super::super::tower_scale(&built, &lay);
        let mut towers = Mesh3D::new(Vec::new(), Vec::new());
        let (_, roads, drawn) = scene(&built, &links, &lay, scale, tallest, 38, &mut towers);
        assert_eq!((drawn.roads, drawn.more), (MAX_ROADS, 28 - MAX_ROADS));
        // Each road is up to three strips; a same-row pair needs no lane run.
        assert!((MAX_ROADS * 2..=MAX_ROADS * 3).contains(&roads.faces.len()));
        let mut canvas = PixelCanvas::new(165, 50, (0, 0, 0), (0, 0, 0));
        key(&mut canvas, &links, &drawn, 2, 0, 165);
        assert!(canvas_text(&canvas).contains("shared files 6 +22 more"));
    }

    #[test]
    fn the_ghost_is_the_previous_recorded_day_by_district_tallest_first() {
        let mut events = vec![
            record(0, &at(1, 0), "old1", "Read a", None),
            record(1, &at(1, 1), "old1", "Read a", None),
            record(2, &at(1, 2), "old2", "Read a", None),
            record(3, &at(3, 0), "today", "Read a", None),
        ];
        events[2].project = "/work/beta".into();
        // Day 2 has no records, so day 1 is the previous recorded day.
        let built = city(&events, &mut State::default());
        let ghost = ghost(&events, &built).unwrap();
        assert_eq!(ghost.day, "2026-03-01");
        assert_eq!(
            ghost.districts,
            vec![
                ("beta".to_string(), vec![0.5]),
                ("alpha".to_string(), vec![1.0])
            ]
        );
        // The oldest day has nothing behind it.
        let mut state = State {
            day: Some("2026-03-01".into()),
            ..State::default()
        };
        assert!(super::ghost(&events, &city(&events, &mut state)).is_none());
    }

    /// Every vertex the overlays add lies inside the box the camera fits.
    fn assert_inside(built: &City<'_>, links: &Links<'_>, dims: (usize, usize)) -> Drawn {
        let (centre, frame) = scene_frame(dims, 2, dims.1 as i64 - 4);
        let (lay, tallest, _) = city_view(built, 0.0, 0.0, centre, frame);
        let scale = super::super::tower_scale(built, &lay);
        let mut towers = Mesh3D::new(Vec::new(), Vec::new());
        let (ghost, roads, drawn) = scene(built, links, &lay, scale, tallest, 40, &mut towers);
        let corners = city_corners(&lay, tallest);
        let (lo, hi) = corners.iter().fold(
            (Vec3D::splat(f64::MAX), Vec3D::splat(f64::MIN)),
            |(lo, hi), c| (lo.min(*c), hi.max(*c)),
        );
        for v in ghost
            .vertices
            .iter()
            .chain(&roads.vertices)
            .chain(&towers.vertices)
        {
            assert!(
                v.cmpge(lo - 1e-9).all() && v.cmple(hi + 1e-9).all(),
                "{dims:?}: {v:?} outside {lo:?}..{hi:?}"
            );
        }
        drawn
    }

    #[test]
    fn ghost_and_arcs_stay_inside_the_fitted_box_and_tall_ghosts_are_cut() {
        // Yesterday's session is far taller than today's whole city.
        let mut events: Vec<Event> = (0..40)
            .map(|i| {
                record(
                    i,
                    &format!("2026-03-01T10:{i:02}:00.000Z"),
                    "big",
                    "Bash x",
                    None,
                )
            })
            .collect();
        events.push(record(50, &at(2, 0), "p", "Bash y", None));
        events.push(record(51, &at(2, 1), "p/kid", "Bash z", None));
        events.push(record(52, &at(2, 2), "p/kid", "Bash z", None));
        let mut state = State::default();
        let built = city(&events, &mut state);
        select(&mut state, &built, 0);
        let links = Links::build(&events, &built, Show::default(), noon(2));
        let demo = demo_events();
        let mut demo_state = State::default();
        let demo_city = city(&demo, &mut demo_state);
        let demo_links = Links::build(&demo, &demo_city, Show::default(), unix_now());
        for dims in [(80usize, 20usize), (120, 38), (200, 65), (512, 180)] {
            let drawn = assert_inside(&built, &links, dims);
            assert_eq!((drawn.bridges, drawn.cut), (1, 1), "{dims:?}");
            assert!(drawn.ghost.is_some_and(|(_, placed)| placed == 1));
            assert_inside(&demo_city, &demo_links, dims);
        }
    }

    #[test]
    fn overlays_draw_nothing_in_panes_too_small_for_them() {
        let events = demo_events();
        let now = unix_now();
        let frame = |dims, show, strip| {
            let mut state = State {
                links: show,
                strip,
                tour: false,
                ..State::default()
            };
            render(&events, "fixture", &mut state, dims, true, now)
        };
        // Panes so small the list has no spare row: nothing changes.
        for dims in [(1, 1), (20, 7), (30, 8), (79, 10)] {
            assert_eq!(
                frame(dims, Show::default(), true),
                frame(dims, ALL_OFF, true),
                "{dims:?}"
            );
        }
        // The smallest city has room for arcs and roads but not the ghost.
        let ghost_off = Show {
            ghost: false,
            ..Show::default()
        };
        // Only the key row differs: it says the ghost needs a taller pane.
        let with = frame((80, 20), Show::default(), true);
        let without = frame((80, 20), ghost_off, true);
        for (row, (a, b)) in with.iter().zip(&without).enumerate() {
            assert!(row == 2 || a == b, "row {row} changed");
        }
        assert!(super::super::tests::plain(&with).contains("ghost needs a taller pane"));
        assert_ne!(
            frame((80, 20), Show::default(), false),
            frame((80, 20), ghost_off, false)
        );
    }

    #[test]
    fn every_overlay_shows_in_the_demo_city_at_several_sizes_and_hides_with_its_key() {
        let events = demo_events();
        let now = unix_now();
        for dims in [(120usize, 38usize), (165, 35), (200, 65), (512, 180)] {
            let frame = |show| {
                let mut state = State {
                    links: show,
                    tour: false,
                    ..State::default()
                };
                render(&events, "fixture", &mut state, dims, true, now)
            };
            let all = frame(Show::default());
            let text = super::super::tests::plain(&all);
            assert!(
                text.contains("delegation 1")
                    && text.contains("shared files")
                    && text.contains("ghost "),
                "{dims:?}: key missing"
            );
            for off in [
                Show {
                    bridges: false,
                    ..Show::default()
                },
                Show {
                    roads: false,
                    ..Show::default()
                },
                Show {
                    ghost: false,
                    ..Show::default()
                },
            ] {
                assert_ne!(all, frame(off), "{dims:?}: {off:?} drew nothing");
            }
            let none = super::super::tests::plain(&frame(ALL_OFF));
            assert!(none.contains("b off") && none.contains("c off") && none.contains("g off"));
        }
    }

    #[test]
    fn the_key_keeps_clear_of_the_hidden_session_note() {
        // Sixty sessions: twenty-four are left out of the city and noted on row 2.
        let events: Vec<Event> = (0..60)
            .map(|i| {
                record(
                    i,
                    &at(2, i as u32 % 60),
                    &format!("s{i}"),
                    "Edit f",
                    Some("/f"),
                )
            })
            .collect();
        for dims in [(80usize, 30usize), (120, 38), (200, 65)] {
            let mut state = State::default();
            let text =
                super::super::tests::plain(&render(&events, "f", &mut state, dims, false, noon(2)));
            let row = text.lines().nth(2).unwrap();
            assert!(
                row.contains("+24 older sessions in the flat view (f)"),
                "{dims:?}: {row}"
            );
            assert!(
                row.contains("c═ 6 +") || row.contains("shared files 6 +"),
                "{dims:?}: {row}"
            );
        }
    }

    #[test]
    fn overlay_keys_redraw_without_stopping_the_tour() {
        let events = demo_events();
        let mut state = State::default();
        for c in ['b', 'c', 'g'] {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            assert_eq!(handle_key(&mut state, key, &events, false), Action::Redraw);
        }
        assert!(state.tour);
        assert_eq!(state.links, ALL_OFF);
    }

    #[test]
    fn the_detail_view_names_a_subagents_parent() {
        let events = demo_events();
        let mut state = State {
            flat: true,
            ..State::default()
        };
        let built = city(&events, &mut state);
        let kid = built
            .sessions
            .iter()
            .position(|s| s.id.contains('/'))
            .unwrap();
        select(&mut state, &built, kid);
        let text = super::super::tests::plain(&render(
            &events,
            "fixture",
            &mut state,
            (120, 40),
            true,
            unix_now(),
        ));
        assert!(text.contains("subagent of claude 93bc1f6c-demo"), "{text}");
        assert!(text.contains("93bc1f6c/7d1e0c") && text.contains("↳claude 7d1e0c"));
    }

    #[test]
    fn the_note_names_the_parent_the_subagents_and_a_shared_file() {
        let mut events = vec![
            record(
                0,
                &at(2, 0),
                "p",
                "Edit src/x.rs",
                Some("/work/alpha/src/x.rs"),
            ),
            record(
                1,
                &at(2, 1),
                "p/kid",
                "Edit src/x.rs",
                Some("/work/alpha/src/x.rs"),
            ),
        ];
        // Directional formatting in a recorded path never reaches the screen.
        events[1].path = Some("/work/alpha/src/x\u{202e}.rs".into());
        events[0].path = events[1].path.clone();
        let mut state = State::default();
        let built = city(&events, &mut state);
        let links = Links::build(&events, &built, Show::default(), noon(2));
        let kid = built.sessions.iter().position(|s| s.id == "p/kid").unwrap();
        let parent = 1 - kid;
        let text = note(&built, &links, kid, 200).unwrap();
        assert!(text.contains("subagent of claude p"), "{text}");
        assert!(text.contains("shares src/x.rs with claude p"), "{text}");
        assert!(!text.contains('\u{202e}'));
        let text = note(&built, &links, parent, 200).unwrap();
        assert!(
            text.contains("started 1 subagent ·") && text.contains("with claude p/kid"),
            "{text}"
        );
        let off = Links::build(
            &events,
            &built,
            Show {
                bridges: false,
                roads: false,
                ghost: false,
            },
            noon(2),
        );
        assert_eq!(note(&built, &off, parent, 200), None);
    }

    #[test]
    fn each_overlay_has_a_key_that_leaves_the_others_alone() {
        let mut show = Show::default();
        assert!(toggle(&mut show, KeyCode::Char('b')));
        assert!(toggle(&mut show, KeyCode::Char('g')));
        assert_eq!(
            show,
            Show {
                bridges: false,
                roads: true,
                ghost: false
            }
        );
        assert!(toggle(&mut show, KeyCode::Char('c')));
        assert!(!show.roads);
        assert!(!toggle(&mut show, KeyCode::Char('h')));
    }

    fn canvas_text(canvas: &PixelCanvas) -> String {
        let mut text = String::new();
        for row in canvas.rows() {
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
}
