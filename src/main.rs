//! SystemScape — scrolling 3D system telemetry history in the terminal.
//!
//! Eight telemetry classes (thermals, power, load, memory, disk and network) are
//! right-to-left scrolling histogram walls stacked into the depth axis, the
//! whole scene rotating slowly through a full 360°. 48 bars × 150s = 2h
//! window. Sensors are polled every ~2s and each bar keeps the PEAK seen in
//! its 150s slot, so short spikes survive and can be correlated across
//! classes. Rendered with gemini-engine (the renderer behind display3d).

use gemini_engine::{
    ascii::Text,
    core::{ColChar, Colour, Modifier, Vec2D},
    gameloop,
    mesh3d::{Face, Mesh3D, Transform3D, Vec3D},
    view::{View, WrappingMode},
    view3d::{DisplayMode, Light, Viewport},
};
use std::collections::VecDeque;
use std::fs;
use std::process::Command;
use std::time::Instant;

const FPS: f32 = 10.0;
const FOV: f64 = 60.0;
const POLL_FRAMES: u32 = 20; // sensor poll every 2s
const SLOT_SECS: f64 = 150.0; // one history bar per 150s
const HISTORY: usize = 48; // 48 × 150s = 2h end to end
const DX: f64 = 0.42; // time-axis spacing
const ROW_GAP: f64 = 1.3; // depth spacing between class walls
const SPIN: f64 = 0.008; // rad/frame — full turn ≈ 78s

fn read_f64(path: &str) -> Option<f64> {
    fs::read_to_string(path).ok()?.trim().parse::<f64>().ok()
}

/// Interpolate over colour stops keyed by a 0..1 normalised value.
fn gradient(stops: &[(f64, (u8, u8, u8))], n: f64) -> Colour {
    let n = n.clamp(0.0, 1.0);
    for w in stops.windows(2) {
        let (n0, c0) = w[0];
        let (n1, c1) = w[1];
        if n <= n1 {
            let f = if n1 > n0 {
                ((n - n0) / (n1 - n0)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let lerp = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * f) as u8;
            return Colour::rgb(lerp(c0.0, c1.0), lerp(c0.1, c1.1), lerp(c0.2, c1.2));
        }
    }
    let (r, g, b) = stops[stops.len() - 1].1;
    Colour::rgb(r, g, b)
}

#[derive(Clone, Copy)]
enum Scale {
    Thermal,    // blue → green → yellow → red
    Power,      // deep purple → pink
    Load,       // teal → cyan → white
    Throughput, // indigo → cyan → white
}

impl Scale {
    fn colour(self, n: f64) -> Colour {
        match self {
            Self::Thermal => gradient(
                &[
                    (0.00, (59, 130, 246)),
                    (0.30, (34, 197, 94)),
                    (0.55, (234, 179, 8)),
                    (0.78, (249, 115, 22)),
                    (1.00, (239, 68, 68)),
                ],
                n,
            ),
            Self::Power => gradient(&[(0.0, (91, 33, 182)), (1.0, (236, 72, 153))], n),
            Self::Load => gradient(
                &[
                    (0.0, (15, 118, 110)),
                    (0.6, (34, 211, 238)),
                    (1.0, (240, 253, 250)),
                ],
                n,
            ),
            Self::Throughput => gradient(
                &[
                    (0.0, (49, 46, 129)),
                    (0.55, (6, 182, 212)),
                    (1.0, (240, 249, 255)),
                ],
                n,
            ),
        }
    }
}

struct Channel {
    tag: &'static str,
    min: f64,
    max: f64,
    scale: Scale,
    history: VecDeque<f64>,
    /// Peak seen since the current 150s slot opened
    slot_peak: f64,
}

impl Channel {
    fn new(tag: &'static str, min: f64, max: f64, scale: Scale) -> Self {
        Self {
            tag,
            min,
            max,
            scale,
            history: VecDeque::with_capacity(HISTORY),
            slot_peak: f64::NAN,
        }
    }

    /// Fold a fresh poll value into the open slot's peak.
    fn poll(&mut self, v: f64) {
        self.slot_peak = if self.slot_peak.is_nan() {
            v
        } else {
            self.slot_peak.max(v)
        };
    }

    /// Close the slot: commit its peak as a history bar, open the next one.
    fn commit_slot(&mut self) {
        if self.slot_peak.is_nan() {
            return;
        }
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(self.slot_peak);
        self.slot_peak = f64::NAN;
    }

    fn norm(&self, v: f64) -> f64 {
        ((v - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
    }
}

/// One poll of every parameter we can read.
#[derive(Default)]
struct Snapshot {
    pkg: Vec<f64>,
    core_max: Option<f64>,
    gpu: Vec<f64>,
    nvme: Option<f64>,
    pch: Option<f64>,
    nic: Option<f64>,
    power_w: Option<f64>,
    cpu_pct: Option<f64>,
    mem_pct: Option<f64>,
    disk_mbps: Option<f64>,
    net_mbps: Option<f64>,
}

#[derive(Default)]
struct RateState {
    disk_bytes: u64,
    net_bytes: u64,
    sampled_at: Option<Instant>,
}

impl Snapshot {
    fn cpu_temp(&self) -> Option<f64> {
        self.pkg
            .iter()
            .copied()
            .chain(self.core_max)
            .fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))))
    }
    fn gpu_temp(&self) -> Option<f64> {
        self.gpu
            .iter()
            .copied()
            .fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))))
    }
}

fn disk_bytes_from(stats: &str) -> u64 {
    stats
        .lines()
        .filter_map(|line| {
            let f: Vec<_> = line.split_whitespace().collect();
            let name = *f.get(2)?;
            let whole_disk = (name.starts_with("nvme") || name.starts_with("mmcblk"))
                && !name.contains('p')
                || ["sd", "vd", "xvd"].iter().any(|prefix| {
                    name.starts_with(prefix)
                        && name.chars().last().is_some_and(|c| c.is_ascii_alphabetic())
                });
            if !whole_disk {
                return None;
            }
            let read_sectors = f.get(5)?.parse::<u64>().ok()?;
            let write_sectors = f.get(9)?.parse::<u64>().ok()?;
            Some((read_sectors + write_sectors) * 512)
        })
        .sum()
}

fn net_bytes_from(dev: &str) -> u64 {
    dev.lines()
        .skip(2)
        .filter_map(|line| {
            let (iface, counters) = line.split_once(':')?;
            if iface.trim() == "lo" {
                return None;
            }
            let f: Vec<_> = counters.split_whitespace().collect();
            Some(f.first()?.parse::<u64>().ok()? + f.get(8)?.parse::<u64>().ok()?)
        })
        .sum()
}

fn poll_sensors(prev_cpu: &mut (u64, u64), rates: &mut RateState) -> Snapshot {
    let mut s = Snapshot::default();
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        let mut hwmons: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        hwmons.sort();
        for h in hwmons {
            let base = h.display().to_string();
            let name = fs::read_to_string(format!("{base}/name"))
                .unwrap_or_default()
                .trim()
                .to_string();
            match name.as_str() {
                "coretemp" => {
                    let (mut i, mut misses) = (1, 0);
                    while misses < 8 {
                        let label = fs::read_to_string(format!("{base}/temp{i}_label"));
                        let input = read_f64(&format!("{base}/temp{i}_input"));
                        i += 1;
                        let Ok(label) = label else {
                            misses += 1;
                            continue;
                        };
                        misses = 0;
                        let Some(milli) = input else { continue };
                        let t = milli / 1000.0;
                        if label.trim().starts_with("Package") {
                            s.pkg.push(t);
                        } else {
                            s.core_max = Some(s.core_max.map_or(t, |m: f64| m.max(t)));
                        }
                    }
                }
                "nvme" => s.nvme = read_f64(&format!("{base}/temp1_input")).map(|m| m / 1000.0),
                "pch_lewisburg" => {
                    s.pch = read_f64(&format!("{base}/temp1_input")).map(|m| m / 1000.0)
                }
                "ixgbe" => s.nic = read_f64(&format!("{base}/temp1_input")).map(|m| m / 1000.0),
                "power_meter" => {
                    s.power_w = read_f64(&format!("{base}/power1_average")).map(|u| u / 1e6)
                }
                _ => {}
            }
        }
    }

    // GPU temps via nvidia-smi (three cards on this host)
    if let Ok(out) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=temperature.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        s.gpu = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<f64>().ok())
            .collect();
    }

    // CPU busy % from /proc/stat deltas
    if let Ok(stat) = fs::read_to_string("/proc/stat") {
        if let Some(line) = stat.lines().next() {
            let f: Vec<u64> = line
                .split_whitespace()
                .skip(1)
                .filter_map(|x| x.parse().ok())
                .collect();
            if f.len() >= 5 {
                let total: u64 = f.iter().sum();
                let idle = f[3] + f[4];
                let (pt, pi) = *prev_cpu;
                if pt > 0 && total > pt {
                    s.cpu_pct = Some(100.0 * (1.0 - (idle - pi) as f64 / (total - pt) as f64));
                }
                *prev_cpu = (total, idle);
            }
        }
    }

    // Memory used % from /proc/meminfo
    if let Ok(mi) = fs::read_to_string("/proc/meminfo") {
        let get = |k: &str| {
            mi.lines()
                .find(|l| l.starts_with(k))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
        };
        if let (Some(total), Some(avail)) = (get("MemTotal:"), get("MemAvailable:")) {
            s.mem_pct = Some(100.0 * (1.0 - avail / total));
        }
    }

    let disk_bytes = fs::read_to_string("/proc/diskstats")
        .ok()
        .map(|v| disk_bytes_from(&v));
    let net_bytes = fs::read_to_string("/proc/net/dev")
        .ok()
        .map(|v| net_bytes_from(&v));
    let now = Instant::now();
    if let Some(previous_at) = rates.sampled_at {
        let seconds = now.duration_since(previous_at).as_secs_f64().max(0.001);
        if let Some(bytes) = disk_bytes {
            s.disk_mbps =
                Some(bytes.saturating_sub(rates.disk_bytes) as f64 / seconds / 1_000_000.0);
        }
        if let Some(bytes) = net_bytes {
            s.net_mbps = Some(bytes.saturating_sub(rates.net_bytes) as f64 / seconds / 1_000_000.0);
        }
    }
    if let Some(bytes) = disk_bytes {
        rates.disk_bytes = bytes;
    }
    if let Some(bytes) = net_bytes {
        rates.net_bytes = bytes;
    }
    rates.sampled_at = Some(now);
    s
}

/// Append a cuboid to the mesh (no bottom face — the camera never sees it).
/// Vertex/face ordering mirrors Mesh3D::default_cube() for backface culling.
fn push_bar(
    mesh: &mut Mesh3D,
    cx: f64,
    cz: f64,
    half_w: f64,
    half_d: f64,
    height: f64,
    colour: Colour,
) {
    let b = mesh.vertices.len();
    let (y0, y1) = (0.0, height);
    let fill = ColChar::SOLID.with_mod(Modifier::Colour(colour));
    mesh.vertices.extend([
        Vec3D::new(cx + half_w, y1, cz - half_d),
        Vec3D::new(cx + half_w, y1, cz + half_d),
        Vec3D::new(cx + half_w, y0, cz - half_d),
        Vec3D::new(cx + half_w, y0, cz + half_d),
        Vec3D::new(cx - half_w, y1, cz - half_d),
        Vec3D::new(cx - half_w, y1, cz + half_d),
        Vec3D::new(cx - half_w, y0, cz - half_d),
        Vec3D::new(cx - half_w, y0, cz + half_d),
    ]);
    for idx in [
        [2usize, 3, 1, 0], // +x
        [4, 5, 7, 6],      // -x
        [1, 3, 7, 5],      // +z
        [4, 6, 2, 0],      // -z
        [0, 1, 5, 4],      // top
    ] {
        mesh.faces
            .push(Face::new(idx.iter().map(|i| b + i).collect(), fill));
    }
}

/// One scrolling histogram wall per class, newest bar at the right edge,
/// the open (in-progress) slot's peak rendered live at the rightmost slot.
/// The drawn bars are re-centred on the rotation axis so a partially-filled
/// history doesn't orbit off screen.
fn build_scene(channels: &[Channel], scroll: f64) -> Mesh3D {
    let mut mesh = Mesh3D::new(Vec::new(), Vec::new());
    let right = HISTORY as f64 * DX / 2.0;
    let n_ch = channels.len() as f64;
    let max_len = channels.iter().map(|c| c.history.len()).max().unwrap_or(0);
    // midpoint of [oldest bar .. live bar] — subtracting it keeps the drawn
    // mass centred on x=0 whatever the fill level
    let centre = right - scroll * DX - max_len as f64 * DX / 2.0;
    for (ci, ch) in channels.iter().enumerate() {
        // channel 0 renders closest to the camera (largest z)
        let z = ((n_ch - 1.0) / 2.0 - ci as f64) * ROW_GAP;
        let len = ch.history.len();
        for (k, &v) in ch.history.iter().enumerate() {
            let x = right - (len - k) as f64 * DX - scroll * DX - centre;
            let n = ch.norm(v);
            push_bar(
                &mut mesh,
                x,
                z,
                0.17,
                0.28,
                0.15 + n * 2.8,
                ch.scale.colour(n),
            );
        }
        // live bar: the currently-accumulating slot
        if !ch.slot_peak.is_nan() {
            let n = ch.norm(ch.slot_peak);
            push_bar(
                &mut mesh,
                right - scroll * DX - centre,
                z,
                0.17,
                0.28,
                0.15 + n * 2.8,
                ch.scale.colour(n),
            );
        }
    }
    mesh
}

/// `--demo`: prefill the full 2h window with synthetic, cross-correlated
/// telemetry — two load events (driving CPU temp and power) plus an
/// independent GPU event (dragging disk temp and memory with it).
fn demo_prefill(channels: &mut [Channel]) {
    let gauss = |t: f64, c: f64, w: f64| (-((t - c) / w).powi(2)).exp();
    for k in 0..HISTORY {
        let t = k as f64 / (HISTORY - 1) as f64; // 0..1 across the window
        let jitter = ((k as f64 * 12.9898).sin() * 43758.5453).fract().abs(); // 0..1
        let ev1 = gauss(t, 0.22, 0.05); // load burst early
        let ev2 = gauss(t, 0.78, 0.03); // sharp spike late
        let gpu_ev = gauss(t, 0.55, 0.09); // independent GPU job mid-window
        let load = (6.0 + 88.0 * ev1 + 75.0 * ev2 + 8.0 * jitter).min(100.0);
        let vals = [
            34.0 + 42.0 * ev1 + 35.0 * ev2 + 5.0 * jitter, // CPU°
            36.0 + 46.0 * gpu_ev + 4.0 * jitter,           // GPU°
            28.0 + 4.0 * jitter + 24.0 * gpu_ev,           // DISK° (checkpoint writes)
            225.0 + 420.0 * ev1 + 360.0 * ev2 + 260.0 * gpu_ev, // PWR
            load,                                          // LOAD
            13.0 + 48.0 * (t * 1.4).min(1.0) + 18.0 * gpu_ev, // MEM ramp
            20.0 + 2200.0 * gpu_ev + 600.0 * ev2,          // IO MB/s
            8.0 + 720.0 * ev1 + 950.0 * ev2,               // NET MB/s
        ];
        for (ch, v) in channels.iter_mut().zip(vals) {
            if ch.history.len() == HISTORY {
                ch.history.pop_front();
            }
            ch.history.push_back(v.clamp(ch.min, ch.max));
        }
    }
}

fn now_bar(s: &Snapshot) -> String {
    let mut p = vec!["NOW".to_string()];
    if let Some(v) = s.cpu_temp() {
        p.push(format!("CPU↑{v:.0}°"));
    }
    if let Some(v) = s.gpu_temp() {
        p.push(format!("GPU↑{v:.0}°"));
    }
    if let Some(v) = s.nvme {
        p.push(format!("NVMe {v:.0}°"));
    }
    if let Some(v) = s.pch {
        p.push(format!("PCH {v:.0}°"));
    }
    if let Some(v) = s.nic {
        p.push(format!("NIC {v:.0}°"));
    }
    if let Some(v) = s.power_w {
        p.push(format!("PWR {v:.0}W"));
    }
    if let Some(v) = s.cpu_pct {
        p.push(format!("LOAD {v:.0}%"));
    }
    if let Some(v) = s.mem_pct {
        p.push(format!("MEM {v:.0}%"));
    }
    if let Some(v) = s.disk_mbps {
        p.push(format!("IO {v:.0}MB/s"));
    }
    if let Some(v) = s.net_mbps {
        p.push(format!("NET {v:.0}MB/s"));
    }
    p.join("  ")
}

fn poll_into_channels(channels: &mut [Channel], s: &Snapshot) {
    for ch in channels.iter_mut() {
        let v = match ch.tag {
            "CPU°" => s.cpu_temp(),
            "GPU°" => s.gpu_temp(),
            "DISK°" => s.nvme,
            "PWR" => s.power_w,
            "LOAD" => s.cpu_pct,
            "MEM" => s.mem_pct,
            "IO" => s.disk_mbps,
            "NET" => s.net_mbps,
            _ => None,
        };
        if let Some(v) = v {
            ch.poll(v);
        }
    }
}

fn term_dims() -> (usize, usize) {
    let (w, h) = terminal_size::terminal_size()
        .map(|(tw, th)| (i64::from(tw.0), i64::from(th.0)))
        .unwrap_or((110, 32));
    (w.clamp(40, 300) as usize, (h - 1).clamp(16, 100) as usize)
}

fn make_view(dims: (usize, usize)) -> View {
    View::new(dims.0, dims.1, ColChar::EMPTY).with_wrapping_mode(WrappingMode::Ignore)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("systemscape {}\n\nUsage: systemscape [--demo]\n\n  --demo     prefill two hours of correlated synthetic telemetry\n  --help     show this help\n  --version  print version", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("systemscape {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let mut dims = term_dims();
    let mut view = make_view(dims);

    let mut viewport = Viewport::new(
        Transform3D::look_at_lh(
            Vec3D::new(0.0, 9.5, 27.0),
            Vec3D::new(0.0, 1.3, 0.0),
            Vec3D::NEG_Y,
        ),
        FOV,
        view.center(),
    );
    viewport.display_mode = DisplayMode::Illuminated {
        lights: vec![
            Light::new_ambient(0.55),
            Light::new_directional(0.45, Vec3D::new(2.0, 1.0, 3.0)),
        ],
    };

    let mut channels = vec![
        Channel::new("CPU°", 25.0, 95.0, Scale::Thermal),
        Channel::new("GPU°", 25.0, 90.0, Scale::Thermal),
        Channel::new("DISK°", 25.0, 75.0, Scale::Thermal),
        Channel::new("PWR", 0.0, 900.0, Scale::Power),
        Channel::new("LOAD", 0.0, 100.0, Scale::Load),
        Channel::new("MEM", 0.0, 100.0, Scale::Load),
        Channel::new("IO", 0.0, 5000.0, Scale::Throughput),
        Channel::new("NET", 0.0, 1250.0, Scale::Throughput),
    ];

    if args.iter().any(|a| a == "--demo") {
        demo_prefill(&mut channels);
    }

    let mut prev_cpu = (0u64, 0u64);
    let mut rates = RateState::default();
    let mut snap = poll_sensors(&mut prev_cpu, &mut rates);
    poll_into_channels(&mut channels, &snap);

    let mut theta: f64 = 0.0;
    let mut frame: u64 = 0;
    let slot_frames = (SLOT_SECS * f64::from(FPS)) as u64; // frames per history bar

    loop {
        // Follow pane resizes: rebuild the canvas and re-centre the camera,
        // then wipe the terminal so stale glyphs outside the new frame vanish.
        let now_dims = term_dims();
        if now_dims != dims {
            dims = now_dims;
            view = make_view(dims);
            viewport.canvas_centre = view.center();
            print!("\x1b[2J");
        }

        if frame > 0 && frame % u64::from(POLL_FRAMES) == 0 {
            snap = poll_sensors(&mut prev_cpu, &mut rates);
            poll_into_channels(&mut channels, &snap);
        }
        if frame > 0 && frame % slot_frames == 0 {
            for ch in &mut channels {
                ch.commit_slot();
            }
            poll_into_channels(&mut channels, &snap);
        }
        let scroll = (frame % slot_frames) as f64 / slot_frames as f64;
        frame = frame.wrapping_add(1);
        theta = (theta + SPIN) % std::f64::consts::TAU;

        let scene = build_scene(&channels, scroll);
        viewport.objects = vec![scene.with_transform(Transform3D::from_rotation_y(theta))];

        view.clear();
        view.draw(&viewport);
        view.draw(&Text::new(
            Vec2D::new(1, 0),
            &now_bar(&snap),
            Modifier::Colour(Colour::rgb(169, 177, 214)),
        ));
        let legend_y = view.center().y * 2 - 1;
        let legend = format!(
            "front→back: {} · 2h window · 150s/bar (peak-hold) · newest→right",
            channels
                .iter()
                .map(|c| c.tag)
                .collect::<Vec<_>>()
                .join(" · ")
        );
        view.draw(&Text::new(
            Vec2D::new(1, legend_y),
            &legend,
            Modifier::Colour(Colour::rgb(86, 95, 137)),
        ));
        let _ = view.display_render();
        let _ = gameloop::sleep_fps(FPS, None);
    }
}

#[cfg(test)]
mod tests {
    use super::{disk_bytes_from, net_bytes_from};

    #[test]
    fn disk_rate_counts_whole_disks_not_partitions() {
        let stats = "259 0 nvme0n1 10 0 100 0 20 0 200 0 0 0 0 0 0 0\n\
                     259 1 nvme0n1p1 10 0 999 0 20 0 999 0 0 0 0 0 0 0\n\
                     8 0 sda 10 0 50 0 20 0 70 0 0 0 0 0 0 0";
        assert_eq!(disk_bytes_from(stats), (100 + 200 + 50 + 70) * 512);
    }

    #[test]
    fn network_rate_excludes_loopback_and_sums_rx_tx() {
        let dev = "Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n\
                   lo: 100 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\n\
                 eth0: 300 0 0 0 0 0 0 0 400 0 0 0 0 0 0 0";
        assert_eq!(net_bytes_from(dev), 700);
    }
}
