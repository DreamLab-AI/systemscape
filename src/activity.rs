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
const WORLD_SCALE: f64 = 6.0;
const FRAME_MS: u64 = 55; // ~18 FPS; operator prefers motion richness over minimum CPU
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
    districts: bool,
    hud: bool,
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

/// Elevated forward flight through a 144-unit landscape. The camera and
/// look-ahead point travel independently, exposing near/far motion parallax.
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
    let depth = (data.lanes.len() as f64 * 1.5 + 2.0).max(5.5) * WORLD_SCALE;
    let phase = state.tour_elapsed * 0.12;
    let ahead = phase + 0.95;
    let eye = Vec3D::new(
        54.0 * phase.cos(),
        38.0 + 8.0 * (phase * 1.7).sin(),
        depth * 0.65 * phase.sin(),
    );
    state.focus = Vec3D::new(48.0 * ahead.cos(), 9.5, depth * 0.55 * ahead.sin());
    state.camera_x = eye.x - state.focus.x;
    state.pitch = eye.y - state.focus.y - 28.0;
    state.zoom = eye.z - state.focus.z;
    state.yaw = 0.0;
}

fn world(point: Vec3D) -> Vec3D {
    Vec3D::new(point.x * WORLD_SCALE, point.y * 2.0, point.z * WORLD_SCALE)
}

fn steer(state: &mut State, yaw: f64, rise: f64) {
    let offset = Vec3D::new(state.camera_x, 28.0 + state.pitch, state.zoom);
    let eye = state.focus + offset;
    let mut direction = Transform3D::from_rotation_y(yaw).transform_vector3(-offset);
    direction.y += rise * direction.length();
    state.focus = eye + direction;
    state.camera_x = -direction.x;
    state.pitch = -direction.y - 28.0;
    state.zoom = -direction.z;
}

fn dolly(state: &mut State, factor: f64) {
    let offset = Vec3D::new(state.camera_x, 28.0 + state.pitch, state.zoom);
    let distance = offset.length();
    let scaled = offset * ((distance * factor).clamp(8.0, 250.0) / distance.max(0.01));
    state.camera_x = scaled.x;
    state.pitch = scaled.y - 28.0;
    state.zoom = scaled.z;
}

fn colour(kind: &str) -> Colour {
    match kind {
        "prompt" => Colour::rgb(20, 255, 190),
        "failure" => Colour::rgb(255, 65, 150),
        "receipt" => Colour::rgb(255, 225, 40),
        "tool" => Colour::rgb(45, 185, 255),
        _ => Colour::rgb(195, 80, 255),
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

/// Decorative terrain gives the time paths a shared spatial frame. Its relief
/// is deterministic and does not pretend to encode throughput or task success.
pub(crate) fn terrain(lanes: usize) -> Mesh3D {
    let mut mesh = Mesh3D::new(Vec::new(), Vec::new());
    let depth = (lanes as f64 * 1.5 + 2.0).max(5.5);
    let (nx, nz) = (80usize, 48usize);
    for j in 0..=nz {
        for i in 0..=nx {
            let x = -12.0 + 24.0 * i as f64 / nx as f64;
            let z = -depth + 2.0 * depth * j as f64 / nz as f64;
            let relief = (x * 0.52).sin() * (z * 0.73).cos() * 1.35;
            let coast = (x / 12.0).powi(4) + (z / depth).powi(4);
            let y = -0.85 + relief - coast * 0.55;
            mesh.vertices.push(Vec3D::new(x, y, z));
        }
    }
    for j in 0..nz {
        for i in 0..nx {
            let x = -12.0 + 24.0 * (i as f64 + 0.5) / nx as f64;
            let z = -depth + 2.0 * depth * (j as f64 + 0.5) / nz as f64;
            let radius = (x / 12.0).powi(4) + (z / depth).powi(4);
            if radius > 1.0 + 0.12 * (x * 2.0 + z).sin() {
                continue;
            }
            let noise = ((i * 17 + j * 31 + i * j * 7) % 11) as u8;
            let coast = radius > 0.72;
            let glyph = if coast {
                ['~', ':', '.'][noise as usize % 3]
            } else {
                ['░', ':', '+', '*', '=', '▒', ':', '+', '^', '▓', '*'][noise as usize]
            };
            let district = ((z / depth + 1.0) * 2.5) as usize;
            let palettes = [
                (8, 245, 225),
                (185, 25, 255),
                (18, 255, 92),
                (255, 152, 12),
                (35, 105, 255),
                (255, 28, 145),
            ];
            let (r, g, b) = if coast {
                (24, 155, 255)
            } else {
                palettes[district.min(5)]
            };
            // Wide luminance range supplies shaded valleys and emissive ridges,
            // using full RGB rather than washing every glyph towards white.
            let shade = 0.26 + 0.74 * (noise as f64 / 10.0).powf(0.7);
            let colour = Colour::rgb(
                (r as f64 * shade) as u8,
                (g as f64 * shade) as u8,
                (b as f64 * shade) as u8,
            );
            let n = j * (nx + 1) + i;
            mesh.faces.push(crate::Face::new(
                vec![n + 1, n + nx + 2, n + nx + 1, n],
                crate::ColChar::new(glyph, Modifier::Colour(colour)),
            ));
        }
    }
    mesh
}

pub(crate) fn texture(mesh: &mut Mesh3D, start: usize, glyph: char) {
    for (index, face) in mesh.faces[start..].iter_mut().enumerate() {
        face.fill_char.text_char = if index % 5 == 4 { glyph } else { ':' };
    }
}

fn position(data: &Slice<'_>, index: usize) -> Vec3D {
    let record = data.records[index];
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
    Vec3D::new(
        x,
        if record.kind == "prompt" { 5.5 } else { 2.8 },
        lane_z(lane, data.lanes.len()),
    )
}

fn scene(data: &Slice<'_>, selected: usize) -> Mesh3D {
    let mut mesh = terrain(data.lanes.len());
    let rail = Colour::rgb(48, 125, 148);
    for (i, _) in data.lanes.iter().enumerate() {
        let z = lane_z(i, data.lanes.len());
        let f = mesh.faces.len();
        push_bar(&mut mesh, 0.0, z, 9.0, 0.045, 0.06, rail);
        texture(&mut mesh, f, '=');
        // Each agent district has a luminous gateway: stable large landmarks
        // make camera motion legible even where recorded actions are sparse.
        for x in [-9.5, 9.5] {
            let f = mesh.faces.len();
            push_bar(&mut mesh, x, z - 0.8, 0.10, 0.10, 6.0, colour("tool"));
            push_bar(&mut mesh, x, z + 0.8, 0.10, 0.10, 6.0, colour("event"));
            texture(&mut mesh, f, '#');
            let v = mesh.vertices.len();
            let f = mesh.faces.len();
            push_bar(&mut mesh, x, z, 0.10, 0.9, 0.22, colour("prompt"));
            for vertex in &mut mesh.vertices[v..] {
                vertex.y += 6.0;
            }
            texture(&mut mesh, f, '=');
        }
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
        let y = if e.kind == "prompt" { 5.5 } else { 2.8 };
        let col = if i == selected {
            Colour::rgb(255, 255, 255)
        } else {
            colour(&e.kind)
        };
        // Stem connects each record to its agent's time rail. Prompt stems are
        // taller; record height is a category, never an invented cost metric.
        let f = mesh.faces.len();
        push_bar(&mut mesh, x, z, 0.035, 0.035, y, col);
        texture(&mut mesh, f, '|');
        let f = mesh.faces.len();
        node(
            &mut mesh,
            Vec3D::new(x, y, z),
            if i == selected { 0.28 } else { 0.18 },
            col,
        );
        texture(
            &mut mesh,
            f,
            match e.kind.as_str() {
                "prompt" => '@',
                "failure" => '!',
                "receipt" => '*',
                "tool" => '+',
                _ => '#',
            },
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
            let f = mesh.faces.len().saturating_sub(5);
            texture(&mut mesh, f, '-');
        }
    }
    for vertex in &mut mesh.vertices {
        *vertex = world(*vertex);
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

/// gemini-engine 1.2 does not clip polygons crossing the camera plane before
/// rasterisation. Clip in camera space to bound raster work during close flight.
pub(crate) fn clip_scene(mesh: Mesh3D, viewport: &Viewport, dims: (usize, usize)) -> Mesh3D {
    let transform = viewport.camera_transform.mul_mat4(&mesh.transform);
    let inverse = transform.inverse();
    let scale = viewport.canvas_centre.x.max(viewport.canvas_centre.y) as f64;
    let focal = 1.0 / (viewport.fov.to_radians() * 0.5).tan();
    let cx = viewport.canvas_centre.x as f64;
    let cy = viewport.canvas_centre.y as f64;
    let planes = [
        (Vec3D::Z, -2.0),
        (-Vec3D::Z, 600.0),
        (Vec3D::new(-1.0, 0.0, cx / (2.0 * focal * scale)), 0.0),
        (
            Vec3D::new(1.0, 0.0, (dims.0 as f64 - cx) / (2.0 * focal * scale)),
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
    let full_hud = state.hud || state.flat || dims.0 < 80 || dims.1 < 25;
    let footer = dims.1.saturating_sub(if full_hud { 7 } else { 1 }) as i64;
    let sidebar = if state.districts && dims.0 >= 110 && !state.flat && dims.1 >= 25 {
        25i64
    } else {
        0
    };
    // A quiet glyph field supplies depth outside the island without resembling
    // additional events. World terrain and event geometry are projected over it.
    if !state.flat && dims.0 >= 80 && dims.1 >= 25 {
        for y in 1..footer {
            for x in 1..dims.0 as i64 - 1 {
                let noise = (x * 73 + y * 151 + x * y * 7) % 97;
                if noise < 8 {
                    label(
                        &mut view,
                        x,
                        y,
                        &['.', ':', '~', '+', '.', ':', '*', '.'][noise as usize].to_string(),
                        Colour::rgb(28, 27, 57),
                    );
                }
            }
        }
    }
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
                state.focus
                    + Vec3D::new(
                        state.camera_x,
                        (28.0 + state.pitch) * if sidebar > 0 { 1.18 } else { 1.0 },
                        state.zoom * if sidebar > 0 { 1.18 } else { 1.0 },
                    ),
                state.focus + Vec3D::new(0.0, 0.5, 0.0),
                Vec3D::NEG_Y,
            ),
            82.0,
            Vec2D::new((dims.0 as i64 + sidebar) / 2, (footer + 4) / 2),
        );
        viewport.display_mode = DisplayMode::Solid;
        let rotation = Transform3D::from_rotation_y(state.yaw);
        viewport.objects = vec![clip_scene(
            scene(&data, state.selected).with_transform(rotation),
            &viewport,
            dims,
        )];
        view.draw(&viewport);
        let mut labelled: Vec<Vec2D> = Vec::new();
        let mut candidates: Vec<_> = data
            .records
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let pos = world(position(&data, i));
                let camera = viewport.camera_transform.transform_point3(pos);
                (i, e, pos, camera.z)
            })
            .filter(|(_, _, _, z)| *z > 2.0)
            .collect();
        candidates.sort_by(|a, b| a.3.total_cmp(&b.3));
        for (i, record, pos, _) in candidates.into_iter().take(24) {
            if i == state.selected {
                continue;
            }
            let p = project(&viewport, rotation, pos + Vec3D::new(0.0, 0.8, 0.0));
            if p.x <= sidebar || p.x >= dims.0 as i64 - 12 || p.y < 2 || p.y >= footer - 1 {
                continue;
            }
            if labelled
                .iter()
                .any(|q| (q.y - p.y).abs() < 2 && (q.x - p.x).abs() < 32)
            {
                continue;
            }
            label(
                &mut view,
                p.x,
                p.y,
                &format!(
                    "{} {}",
                    record.kind.to_uppercase(),
                    record.text.chars().take(22).collect::<String>()
                ),
                colour(&record.kind),
            );
            labelled.push(p);
            if labelled.len() >= 12 {
                break;
            }
        }
        if let Some(record) = data.records.get(state.selected) {
            let p = project(
                &viewport,
                rotation,
                world(position(&data, state.selected) + Vec3D::new(0.0, 0.7, 0.0)),
            );
            if p.x > sidebar && p.y > 5 && p.y < footer - 1 {
                label(
                    &mut view,
                    p.x - 2,
                    p.y - 1,
                    ". * .",
                    Colour::rgb(97, 107, 83),
                );
                label(&mut view, p.x - 2, p.y, "[ @ ]", Colour::rgb(255, 242, 158));
                label(
                    &mut view,
                    p.x + 4,
                    p.y,
                    &format!(
                        "{} {}",
                        record.kind.to_uppercase(),
                        record.at.get(11..16).unwrap_or("")
                    ),
                    Colour::rgb(248, 218, 106),
                );
            }
        }
        for (i, lane) in data.lanes.iter().enumerate() {
            let p = project(
                &viewport,
                rotation,
                world(Vec3D::new(9.0, 0.2, lane_z(i, data.lanes.len()))),
            );
            if p.x >= 0 && p.y > 3 && p.y < footer {
                let name = if sidebar > 0 {
                    format!("[{}]", i + 1)
                } else {
                    format!("{} {}", i + 1, lane.chars().take(18).collect::<String>())
                };
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
    if sidebar > 0 {
        for y in 4..footer {
            label(&mut view, 0, y, &" ".repeat(sidebar as usize), muted);
        }
        label(
            &mut view,
            1,
            5,
            "+-- AGENT DISTRICTS --+",
            Colour::rgb(240, 204, 82),
        );
        for (i, lane) in data.lanes.iter().enumerate() {
            let y = 7 + i as i64 * 2;
            if y + 1 >= footer - 3 {
                break;
            }
            let active = data
                .records
                .get(state.selected)
                .is_some_and(|e| &e.agent == lane);
            let title = format!("{}{} {}", if active { ">" } else { " " }, i + 1, lane);
            label(
                &mut view,
                1,
                y,
                &title.chars().take(23).collect::<String>(),
                if active {
                    Colour::rgb(123, 255, 203)
                } else {
                    muted
                },
            );
            let count = data.records.iter().filter(|e| &e.agent == lane).count();
            label(
                &mut view,
                3,
                y + 1,
                &format!("{count} actions in view"),
                Colour::rgb(56, 115, 151),
            );
        }
        label(
            &mut view,
            1,
            footer - 3,
            "@ prompt   + tool",
            Colour::rgb(79, 190, 182),
        );
        label(
            &mut view,
            1,
            footer - 2,
            "! error    * receipt",
            Colour::rgb(233, 173, 100),
        );
    }
    if !full_hud {
        label(&mut view, 0, 0, &" ".repeat(dims.0), bright);
        label(
            &mut view,
            1,
            0,
            &format!(
                "SYSTEMSCAPE  //  {}  //  {}{}  //  {} recorded objects",
                data.day,
                if state.tour { "FLYING TOUR" } else { "MANUAL" },
                if demo { " · DEMO" } else { "" },
                data.records.len()
            ),
            Colour::rgb(63, 235, 255),
        );
        label(&mut view, 0, footer, &" ".repeat(dims.0), bright);
        label(&mut view, 1, footer, "Space pause/fly · arrows steer · +/- zoom · ? details/coverage · h districts · Enter source · 0 overview · q exit", muted);
        return view;
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
                "FLYING TOUR · FAST / HIGH ALTITUDE"
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
        "PgUp/PgDn records  Enter source  f flat/3D  h districts  0 overview  q quit",
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
    tour_step(&events, &mut state, 0.0);
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
        if last_frame.elapsed() >= Duration::from_millis(FRAME_MS) {
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
                    usize::from(dims.0).clamp(1, 512),
                    usize::from(dims.1).saturating_sub(1).clamp(1, 180),
                ),
                demo,
            );
            // Build and publish a whole frame atomically to terminals supporting
            // synchronized updates (including tmux), avoiding half-painted terrain.
            let data = slice(&events, &mut state);
            let panel = data
                .records
                .get(state.selected)
                .filter(|_| !state.hud && !state.flat && view.width >= 80 && view.height >= 25)
                .map(|e| {
                    crate::black_panel(
                        view.width,
                        view.height,
                        &[
                            format!("{} · {} · {}", e.agent, e.kind.to_uppercase(), e.at),
                            e.text.clone(),
                        ],
                    )
                })
                .unwrap_or_default();
            let frame = format!("\x1b[?2026h{view}{panel}\x1b[?2026l");
            io::stdout().write_all(frame.as_bytes())?;
            io::stdout().flush()?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(FRAME_MS))? {
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
                    KeyCode::Left => steer(&mut state, -0.12, 0.0),
                    KeyCode::Right => steer(&mut state, 0.12, 0.0),
                    KeyCode::Up => steer(&mut state, 0.0, 0.12),
                    KeyCode::Down => steer(&mut state, 0.0, -0.12),
                    KeyCode::Char('+') | KeyCode::Char('=') => dolly(&mut state, 0.88),
                    KeyCode::Char('-') => dolly(&mut state, 1.12),
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
                    KeyCode::Char('h') => state.districts = !state.districts,
                    KeyCode::Enter => {
                        state.detail = !state.detail;
                        state.hud = true;
                    }
                    KeyCode::Char('?') => state.hud = !state.hud,
                    KeyCode::Char('0') => {
                        state.yaw = -0.3;
                        state.pitch = 0.0;
                        state.zoom = 180.0;
                        state.pitch = 65.0;
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
    fn flight_clips_every_polygon_to_camera_and_screen_bounds() {
        let events = crate::activity_data::demo_events();
        let mut state = State {
            tour: true,
            ..State::default()
        };
        for _ in 0..30 {
            tour_step(&events, &mut state, 0.5);
            let data = slice(&events, &mut state);
            let viewport = Viewport::new(
                Transform3D::look_at_lh(
                    state.focus + Vec3D::new(state.camera_x, 28.0 + state.pitch, state.zoom),
                    state.focus,
                    Vec3D::NEG_Y,
                ),
                82.0,
                Vec2D::new(100, 32),
            );
            let clipped = clip_scene(scene(&data, state.selected), &viewport, (200, 65));
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
        assert!(state.camera_x.abs() > 5.0 && state.zoom != 40.0);
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
