//! Telemetry: one labelled 3D ridge per channel, time fixed left to right.
//!
//! Each channel is a ridge of 48 peak-hold slots (150 s each, 2 h end to end)
//! plus the live slot at the NOW edge. Every channel has its own hue; value
//! drives height and brightness. GPUs are split per card (temperature,
//! utilisation, VRAM). The camera sways and bobs but never orbits, so older is
//! always left and NOW is always right.
use crate::activity::{clip_scene, project};
use crate::hosts::{self, Federation, Link, Remote, Report};
use crate::pixels::{mix, PixelCanvas, Rgb};
use crate::{
    ColChar, Colour, DisplayMode, Face, Mesh3D, Modifier, Transform3D, Vec2D, Vec3D, Viewport,
};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use gemini_engine::core::CanDraw;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const FRAME_MS: u64 = 55; // ~18 FPS
const POLL: Duration = Duration::from_secs(2);
/// GPU query interval while the view is off screen. The cheap /proc and hwmon
/// sensors keep their two-second pace so rates and peaks are not diluted;
/// nvidia-smi, the costly one, runs a fifth as often.
const BACKGROUND_GPU_POLL: Duration = Duration::from_secs(10);
const SLOT_SECS: f64 = 150.0; // one history slot per 150s
const HISTORY: usize = 48; // 48 × 150s = 2h end to end
const RELIEF: f64 = 5.0; // world height of a full-scale value
const FOV: f64 = 48.0; // long lens: rows stay evenly spaced front to back
const SKY_TOP: Rgb = (7, 10, 20);
const SKY_HORIZON: Rgb = (20, 27, 44);
const GROUND: Rgb = (17, 23, 34);
const MUTED: Rgb = (126, 144, 164);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    CpuTemp,
    GpuTemp,
    DiskTemp,
    Power,
    Load,
    Mem,
    GpuUtil,
    Vram,
    Io,
    Net,
}

struct Channel {
    kind: Kind,
    gpu: Option<usize>,
    /// Full-scale reference for kinds measured against a capacity (VRAM, GiB).
    total: f64,
    history: VecDeque<f64>,
    /// Peak seen since the current slot opened.
    slot_peak: f64,
    now: Option<f64>,
}

impl Channel {
    fn new(kind: Kind, gpu: Option<usize>) -> Self {
        Self {
            kind,
            gpu,
            total: 1.0,
            history: VecDeque::with_capacity(HISTORY),
            slot_peak: f64::NAN,
            now: None,
        }
    }

    fn label(&self) -> String {
        let card = self.gpu.map(|i| i.to_string()).unwrap_or_default();
        match self.kind {
            Kind::CpuTemp => "CPU°".into(),
            Kind::GpuTemp => format!("GPU{card}°"),
            Kind::DiskTemp => "DISK°".into(),
            Kind::Power => "PWR".into(),
            Kind::Load => "LOAD".into(),
            Kind::Mem => "MEM".into(),
            Kind::GpuUtil => format!("GPU{card}%"),
            Kind::Vram => format!("VRAM{card}"),
            Kind::Io => "IO".into(),
            Kind::Net => "NET".into(),
        }
    }

    /// One hue per kind; cards of the same kind are shades of that hue.
    fn hue(&self) -> Rgb {
        let base = match self.kind {
            Kind::CpuTemp => (255, 120, 60),
            Kind::GpuTemp => (255, 85, 150),
            Kind::DiskTemp => (220, 170, 120),
            Kind::Power => (255, 200, 60),
            Kind::Load => (110, 220, 110),
            Kind::Mem => (175, 215, 80),
            Kind::GpuUtil => (70, 220, 200),
            Kind::Vram => (90, 165, 255),
            Kind::Io => (130, 120, 255),
            Kind::Net => (200, 110, 240),
        };
        match self.gpu.map(|i| i % 3) {
            Some(1) => mix(base, (255, 255, 255), 0.25),
            Some(2) => mix(base, (0, 0, 0), 0.2),
            _ => base,
        }
    }

    /// Map a raw value to 0..1. Throughput is logarithmic so everyday traffic
    /// is visible beside a multi-GB/s burst; everything else is linear.
    fn norm(&self, v: f64) -> f64 {
        let linear = |lo: f64, hi: f64| (v - lo) / (hi - lo);
        let log = |full: f64| (1.0 + v.max(0.0)).ln() / (1.0 + full).ln();
        match self.kind {
            Kind::CpuTemp => linear(25.0, 95.0),
            Kind::GpuTemp => linear(25.0, 90.0),
            Kind::DiskTemp => linear(25.0, 75.0),
            Kind::Power => linear(0.0, 900.0),
            Kind::Load | Kind::Mem | Kind::GpuUtil => linear(0.0, 100.0),
            Kind::Vram => v / self.total.max(0.001),
            Kind::Io => log(5000.0),
            Kind::Net => log(1250.0),
        }
        .clamp(0.0, 1.0)
    }

    fn format(&self, v: f64) -> String {
        match self.kind {
            Kind::CpuTemp | Kind::GpuTemp | Kind::DiskTemp => format!("{v:.0}°"),
            Kind::Power => format!("{v:.0}W"),
            Kind::Load | Kind::Mem | Kind::GpuUtil => format!("{v:.0}%"),
            Kind::Vram => format!("{v:.0}/{:.0}G", self.total),
            Kind::Io | Kind::Net if v < 10.0 => format!("{v:.1}MB/s"),
            Kind::Io | Kind::Net => format!("{v:.0}MB/s"),
        }
    }

    /// Fold a fresh poll value into the open slot's peak.
    fn poll(&mut self, v: f64) {
        self.now = Some(v);
        self.slot_peak = if self.slot_peak.is_nan() {
            v
        } else {
            self.slot_peak.max(v)
        };
    }

    /// Close the slot: commit its peak as history, open the next one. A slot
    /// with no reading is committed as a NaN gap, so every channel keeps the
    /// same number of slots and ridges stay aligned in time.
    fn commit_slot(&mut self) {
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(self.slot_peak);
        self.slot_peak = f64::NAN;
    }

    fn has_data(&self) -> bool {
        !self.slot_peak.is_nan() || self.history.iter().any(|v| !v.is_nan())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Gpu {
    temp: Option<f64>,
    util: Option<f64>,
    used_gib: Option<f64>,
    total_gib: Option<f64>,
}

/// One poll of every parameter we can read. It is also the body of a probe
/// line (`hosts::Frame`): every field is optional on the wire, so a probe and
/// a viewer of different builds still agree on the fields they share.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Snapshot {
    pub(crate) pkg: Vec<f64>,
    pub(crate) core_max: Option<f64>,
    pub(crate) gpus: Vec<Gpu>,
    pub(crate) nvme: Option<f64>,
    pub(crate) pch: Option<f64>,
    pub(crate) nic: Option<f64>,
    pub(crate) power_w: Option<f64>,
    pub(crate) cpu_pct: Option<f64>,
    pub(crate) mem_pct: Option<f64>,
    pub(crate) disk_mbps: Option<f64>,
    pub(crate) net_mbps: Option<f64>,
}

impl Snapshot {
    fn cpu_temp(&self) -> Option<f64> {
        self.pkg
            .iter()
            .copied()
            .chain(self.core_max)
            .reduce(f64::max)
    }

    fn value(&self, channel: &Channel) -> Option<f64> {
        let gpu = channel.gpu.and_then(|i| self.gpus.get(i));
        match channel.kind {
            Kind::CpuTemp => self.cpu_temp(),
            Kind::GpuTemp => gpu?.temp,
            Kind::DiskTemp => self.nvme,
            Kind::Power => self.power_w,
            Kind::Load => self.cpu_pct,
            Kind::Mem => self.mem_pct,
            Kind::GpuUtil => gpu?.util,
            Kind::Vram => gpu?.used_gib,
            Kind::Io => self.disk_mbps,
            Kind::Net => self.net_mbps,
        }
    }
}

#[derive(Default)]
struct RateState {
    disk_bytes: u64,
    net_bytes: u64,
    sampled_at: Option<Instant>,
}

fn read_f64(path: &str) -> Option<f64> {
    fs::read_to_string(path).ok()?.trim().parse::<f64>().ok()
}

/// Every `tempN_label` / `tempN_input` pair of one hwmon device, in °C.
fn labelled_temps(base: &str) -> Vec<(String, f64)> {
    (1..=16)
        .filter_map(|i| {
            let label = fs::read_to_string(format!("{base}/temp{i}_label")).ok()?;
            let milli = read_f64(&format!("{base}/temp{i}_input"))?;
            Some((label.trim().to_string(), milli / 1000.0))
        })
        .collect()
}

/// AMD `k10temp`: the hottest core die (`Tccd*`) when the driver lists them,
/// else `Tdie`, else the control value `Tctl`, which can carry a fan offset.
fn amd_cpu_temp(temps: &[(String, f64)]) -> Option<f64> {
    let named = |name: &str| temps.iter().find(|(l, _)| l == name).map(|(_, t)| *t);
    temps
        .iter()
        .filter(|(label, _)| label.starts_with("Tccd"))
        .map(|(_, t)| *t)
        .reduce(f64::max)
        .or_else(|| named("Tdie"))
        .or_else(|| named("Tctl"))
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

/// Parse `nvidia-smi --query-gpu=temperature.gpu,utilization.gpu,memory.used,memory.total`
/// CSV rows. A field the driver reports as `[N/A]` stays `None`; the card keeps its index.
fn parse_gpus(csv: &str) -> Vec<Gpu> {
    csv.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split(',').map(|f| f.trim().parse::<f64>().ok());
            let mut next = || fields.next().flatten();
            Gpu {
                temp: next(),
                util: next(),
                used_gib: next().map(|mib| mib / 1024.0),
                total_gib: next().map(|mib| mib / 1024.0),
            }
        })
        .collect()
}

/// Ask nvidia-smi for every card. A missing, failing or hung tool yields no
/// cards for this poll; it is killed after 1.5 s so it cannot stall the other sensors.
fn query_gpus() -> Vec<Gpu> {
    use std::io::Read;
    use std::process::Stdio;
    let Ok(mut child) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=temperature.gpu,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut csv = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_string(&mut csv);
                }
                return if status.success() {
                    parse_gpus(&csv)
                } else {
                    Vec::new()
                };
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
}

fn poll_sensors(prev_cpu: &mut (u64, u64), rates: &mut RateState, gpus: bool) -> Snapshot {
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
            let temp1 = || read_f64(&format!("{base}/temp1_input")).map(|m| m / 1000.0);
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
                "k10temp" => s.pkg.extend(amd_cpu_temp(&labelled_temps(&base))),
                "nvme" => s.nvme = temp1(),
                "pch_lewisburg" => s.pch = temp1(),
                "ixgbe" => s.nic = temp1(),
                "power_meter" => {
                    s.power_w = read_f64(&format!("{base}/power1_average")).map(|u| u / 1e6)
                }
                _ => {}
            }
        }
    }

    if gpus {
        s.gpus = query_gpus();
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
                if pt > 0 && total > pt && idle >= pi {
                    s.cpu_pct = Some(
                        100.0 * (1.0 - (idle - pi) as f64 / (total - pt) as f64).clamp(0.0, 1.0),
                    );
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

/// Ridges front to back: heat, then what drives it, then per-card GPU work, then traffic.
fn channels_for(gpus: usize) -> Vec<Channel> {
    let per_card = |kind| (0..gpus).map(move |i| Channel::new(kind, Some(i)));
    let mut channels = vec![Channel::new(Kind::CpuTemp, None)];
    channels.extend(per_card(Kind::GpuTemp));
    channels.extend(
        [Kind::DiskTemp, Kind::Power, Kind::Load, Kind::Mem].map(|k| Channel::new(k, None)),
    );
    channels.extend(per_card(Kind::GpuUtil));
    channels.extend(per_card(Kind::Vram));
    channels.extend([Kind::Io, Kind::Net].map(|k| Channel::new(k, None)));
    channels
}

/// Add ridges for cards that appear after start-up (a slow first query, a
/// hot-plugged card). New ridges are padded with gaps to stay aligned in time.
fn ensure_cards(channels: &mut Vec<Channel>, cards: usize) {
    let known = channels.iter().filter(|c| c.kind == Kind::GpuTemp).count();
    if cards <= known {
        return;
    }
    let committed = channels.iter().map(|c| c.history.len()).max().unwrap_or(0);
    let mut old = std::mem::take(channels);
    *channels = channels_for(cards)
        .into_iter()
        .map(|mut fresh| {
            match old
                .iter()
                .position(|c| c.kind == fresh.kind && c.gpu == fresh.gpu)
            {
                Some(i) => old.swap_remove(i),
                None => {
                    fresh
                        .history
                        .extend(std::iter::repeat_n(f64::NAN, committed));
                    fresh
                }
            }
        })
        .collect();
}

fn poll_into_channels(channels: &mut [Channel], snapshot: &Snapshot) {
    for channel in channels.iter_mut() {
        if channel.kind == Kind::Vram {
            if let Some(total) = channel.gpu.and_then(|i| snapshot.gpus.get(i)?.total_gib) {
                channel.total = total;
            }
        }
        // A silent source clears the live value rather than showing a stale one.
        channel.now = snapshot.value(channel);
        if let Some(v) = channel.now {
            channel.poll(v);
        }
    }
}

/// `--demo`: prefill the 2h window with a synthetic, cross-correlated story.
/// Two load bursts drive CPU temperature, power and network; each GPU runs its
/// own job at a different time, pulling its temperature, VRAM and disk IO along.
fn demo_prefill(channels: &mut [Channel]) {
    let gauss = |t: f64, c: f64, w: f64| (-((t - c) / w).powi(2)).exp();
    for k in 0..HISTORY {
        let t = k as f64 / (HISTORY - 1) as f64;
        let jitter = ((k as f64 * 12.9898).sin() * 43758.5453).fract().abs();
        let (burst, spike) = (gauss(t, 0.22, 0.05), gauss(t, 0.78, 0.03));
        for channel in channels.iter_mut() {
            let centre = [0.55, 0.38, 0.68][channel.gpu.unwrap_or(0) % 3];
            let job = gauss(t, centre, 0.08);
            let all_jobs: f64 = [0.55, 0.38, 0.68].iter().map(|c| gauss(t, *c, 0.08)).sum();
            if channel.kind == Kind::Vram && channel.total <= 1.0 {
                channel.total = 48.0;
            }
            let v = match channel.kind {
                Kind::CpuTemp => 34.0 + 42.0 * burst + 35.0 * spike + 5.0 * jitter,
                Kind::GpuTemp => 36.0 + 46.0 * job + 4.0 * jitter,
                Kind::DiskTemp => 28.0 + 4.0 * jitter + 14.0 * all_jobs,
                Kind::Power => 225.0 + 420.0 * burst + 360.0 * spike + 150.0 * all_jobs,
                Kind::Load => (6.0 + 88.0 * burst + 75.0 * spike + 8.0 * jitter).min(100.0),
                Kind::Mem => 13.0 + 48.0 * (t * 1.4).min(1.0) + 6.0 * all_jobs,
                Kind::GpuUtil => (2.0 + 96.0 * job + 3.0 * jitter).min(100.0),
                Kind::Vram => {
                    let loaded = t > centre - 0.1 && t < centre + 0.16;
                    channel.total * if loaded { 0.72 } else { 0.06 }
                }
                Kind::Io => 2.0 + 1400.0 * all_jobs + 600.0 * spike,
                Kind::Net => 1.0 + 720.0 * burst + 950.0 * spike,
            };
            if channel.history.len() == HISTORY {
                channel.history.pop_front();
            }
            channel.history.push_back(v);
        }
    }
}

struct Camera {
    /// Seconds of flight; frozen while holding.
    t: f64,
    fly: bool,
    yaw: f64,
    lift: f64,
    dolly: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            t: 0.0,
            fly: true,
            yaw: 0.0,
            lift: 0.0,
            dolly: 1.0,
        }
    }
}

fn colour(c: Rgb) -> Colour {
    Colour::rgb(c.0, c.1, c.2)
}

/// World x of slot position `p` (0 = oldest, HISTORY = the live slot). The
/// camera looks down -z with a left-handed view, so screen-right is world -x.
fn slot_x(p: f64) -> f64 {
    HISTORY as f64 / 2.0 - p
}

/// Depth of each visible ridge: cards of one kind sit closer together than
/// different kinds. Row 0 is nearest the camera (largest z).
fn row_depths(channels: &[&Channel]) -> Vec<f64> {
    let mut offsets = Vec::with_capacity(channels.len());
    let mut z = 0.0;
    for (i, channel) in channels.iter().enumerate() {
        if i > 0 {
            z += if channels[i - 1].kind == channel.kind {
                2.0
            } else {
                3.1
            };
        }
        offsets.push(z);
    }
    let centre = z / 2.0;
    offsets.into_iter().map(|o| centre - o).collect()
}

pub(crate) fn quad(mesh: &mut Mesh3D, x0: f64, x1: f64, y: f64, z0: f64, z1: f64, c: Rgb) {
    let b = mesh.vertices.len();
    mesh.vertices.extend([
        Vec3D::new(x1, y, z0),
        Vec3D::new(x1, y, z1),
        Vec3D::new(x0, y, z1),
        Vec3D::new(x0, y, z0),
    ]);
    mesh.faces.push(Face::new(
        vec![b, b + 1, b + 2, b + 3],
        ColChar::SOLID.with_mod(Modifier::Colour(colour(c))),
    ));
}

/// A slab with its own colour per face, so tops catch the light and sides
/// fall into shade. `None` skips a side that a taller neighbour hides; the
/// back face is never built because the camera stays in front. Vertex and
/// face ordering mirror `Mesh3D::default_cube()`.
fn slab(
    mesh: &mut Mesh3D,
    centre: (f64, f64),
    half: (f64, f64),
    height: f64,
    faces: [Option<Rgb>; 4],
) {
    let b = mesh.vertices.len();
    for x in [centre.0 + half.0, centre.0 - half.0] {
        for y in [height, 0.0] {
            for z in [centre.1 - half.1, centre.1 + half.1] {
                mesh.vertices.push(Vec3D::new(x, y, z));
            }
        }
    }
    for (indices, c) in [
        [2usize, 3, 1, 0], // +x, the older side
        [4, 5, 7, 6],      // -x, the newer side
        [1, 3, 7, 5],      // +z, facing the camera
        [0, 1, 5, 4],      // top
    ]
    .iter()
    .zip(faces)
    {
        if let Some(c) = c {
            mesh.faces.push(Face::new(
                indices.iter().map(|i| b + i).collect(),
                ColChar::SOLID.with_mod(Modifier::Colour(colour(c))),
            ));
        }
    }
}

/// Ground tiles and ridges as two meshes. The ground is drawn first so the
/// engine's per-face depth sort never puts a floor tile over a ridge.
fn build_scene(
    channels: &[&Channel],
    depths: &[f64],
    scroll: f64,
    eye: Vec3D,
    reach: f64,
) -> (Mesh3D, Mesh3D) {
    // Haze deepens behind the point the camera is aimed at.
    let fog = |c: Rgb, at: Vec3D| {
        mix(
            c,
            SKY_HORIZON,
            ((at - eye).length() - reach * 0.9) / (reach * 0.5) * 0.55,
        )
    };
    let mut ground = Mesh3D::new(Vec::new(), Vec::new());
    let mut ridges = Mesh3D::new(Vec::new(), Vec::new());
    for (row, (channel, &z)) in channels.iter().zip(depths).enumerate() {
        let hue = channel.hue();
        // Each track runs to the midpoint with its neighbours, so ridges meet
        // and no dark gap shimmers between them as the camera moves.
        let front = row
            .checked_sub(1)
            .map_or(z + 1.0, |i| (z + depths[i]) / 2.0);
        let back = depths.get(row + 1).map_or(z - 1.0, |next| (z + next) / 2.0);
        let (mid, half_depth) = ((front + back) / 2.0, (front - back) / 2.0);
        // Floor: one strip per half hour in the ridge's hue, split by tick
        // tiles and ended by the NOW tile. Slot positions run 0 (oldest) to HISTORY.
        let mut floor = |from: f64, to: f64, tile: Rgb| {
            let (x0, x1) = (slot_x(to), slot_x(from));
            let at = Vec3D::new((x0 + x1) / 2.0, 0.0, z);
            quad(&mut ground, x0, x1, 0.0, back, front, fog(tile, at));
        };
        for slot in (0..HISTORY).step_by(12) {
            let slot = slot as f64;
            floor(slot - 0.5, slot + 0.5, mix(GROUND, MUTED, 0.18));
            floor(slot + 0.5, slot + 11.5, mix(GROUND, hue, 0.10));
        }
        floor(
            HISTORY as f64 - 0.5,
            HISTORY as f64 + 0.5,
            mix(GROUND, (255, 244, 185), 0.30),
        );

        let samples = channel.history.len();
        let live = (!channel.slot_peak.is_nan()).then_some(channel.slot_peak);
        let values: Vec<f64> = channel.history.iter().copied().chain(live).collect();
        let heights: Vec<f64> = values
            .iter()
            .map(|v| {
                if v.is_nan() {
                    0.0
                } else {
                    0.12 + channel.norm(*v) * RELIEF
                }
            })
            .collect();
        for (k, &height) in heights.iter().enumerate() {
            let x = slot_x((HISTORY + k) as f64 - samples as f64 - scroll);
            // Skip gaps, and the oldest slot once it slides off the floor.
            if values[k].is_nan() || x > slot_x(-0.5) {
                continue;
            }
            let n = channel.norm(values[k]);
            let lit = mix((12, 15, 24), hue, 0.38 + 0.62 * n);
            let glow = 0.12 + 0.30 * n + if k == samples { 0.25 } else { 0.0 };
            let at = Vec3D::new(x, height, z);
            let shade = |f: f64| fog(mix((6, 8, 14), lit, f), at);
            // A side only shows where the neighbouring slot is lower or absent.
            let exposed = |neighbour: Option<&f64>| neighbour.is_none_or(|h| *h < height);
            let older = k.checked_sub(1).and_then(|i| heights.get(i));
            slab(
                &mut ridges,
                (x, mid),
                (0.5, half_depth),
                height,
                [
                    exposed(older).then(|| shade(0.46)),
                    exposed(heights.get(k + 1)).then(|| shade(0.46)),
                    Some(shade(0.62)),
                    Some(fog(mix(lit, (255, 255, 255), glow), at)),
                ],
            );
        }
    }
    (ground, ridges)
}

/// Compact layout for narrow panes: one sparkline per channel.
fn render_flat(channels: &[&Channel], dims: (usize, usize), bar: Option<&HostBar>) -> Vec<String> {
    let mut canvas = PixelCanvas::new(dims.0, dims.1, (10, 14, 22), (10, 14, 22));
    match bar {
        Some(bar) => {
            canvas.label(1, 0, "SYSTEMSCAPE", (63, 235, 255));
            bar.draw(&mut canvas, 13, 0);
        }
        None => canvas.label(1, 0, "SYSTEMSCAPE  2h → now", (63, 235, 255)),
    }
    // Wide panes give each slot several columns, so the chart fills the width.
    let slots = dims.0.saturating_sub(21).min(HISTORY + 1);
    let stride = (dims.0.saturating_sub(21) / (HISTORY + 1)).max(1);
    let shown = channels.len().min(dims.1.saturating_sub(2));
    for (row, channel) in channels.iter().enumerate().take(shown) {
        let hue = channel.hue();
        let y = row as i64 + 1;
        canvas.label(1, y, &channel.label(), hue);
        if let Some(v) = channel.now {
            canvas.label(8, y, &channel.format(v), mix(hue, (255, 255, 255), 0.35));
        }
        let live = (!channel.slot_peak.is_nan()).then_some(channel.slot_peak);
        let values: Vec<f64> = channel.history.iter().copied().chain(live).collect();
        let start = values.len().saturating_sub(slots);
        let pad = slots - (values.len() - start);
        for (i, v) in values[start..].iter().enumerate() {
            if v.is_nan() {
                continue; // a gap: no reading in that slot
            }
            let n = channel.norm(*v);
            let glyph = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][(n * 7.0).round() as usize];
            canvas.label(
                (20 + (pad + i) * stride) as i64,
                y,
                &glyph.to_string().repeat(stride),
                mix((40, 48, 62), hue, 0.35 + 0.65 * n),
            );
        }
    }
    let hidden = channels.len() - shown;
    let more = if hidden > 0 {
        format!(" · +{hidden} more channels in a taller pane")
    } else {
        String::new()
    };
    let (trouble, tab) = bar.map_or((String::new(), ""), |bar| (bar.trouble(), "Tab host · "));
    canvas.label(
        1,
        dims.1 as i64 - 1,
        &format!("{trouble}f 3D · {tab}q quit{more}"),
        MUTED,
    );
    canvas.rows()
}

fn render(
    channels: &[Channel],
    snapshot: &Snapshot,
    camera: &Camera,
    scroll: f64,
    dims: (usize, usize),
    flat: bool,
    bar: Option<&HostBar>,
) -> Vec<String> {
    let visible: Vec<&Channel> = channels.iter().filter(|c| c.has_data()).collect();
    // The 3D view needs one free text row per ridge label, plus header,
    // time ruler and status line.
    if flat || dims.0 < 80 || dims.1 < 20 || visible.len() + 3 > dims.1 {
        return render_flat(&visible, dims, bar);
    }
    let mut canvas = PixelCanvas::new(dims.0, dims.1, SKY_TOP, SKY_HORIZON);
    let (width, height) = canvas.sample_dims();
    let depths = row_depths(&visible);
    let t = camera.t;
    // Sway, bob and breathe, but never orbit: older stays left, NOW stays right.
    let angle = camera.yaw + 0.26 * (t * 0.12).sin();
    // The engine scales by the larger half-dimension. Pull back when the pane
    // is taller than wide (width binds) or very short and wide (height binds).
    let aspect = height as f64 / width as f64;
    let fit = aspect.max(1.0) * (0.42 / aspect).max(1.0);
    // More cards make the scene deeper; back off so every ridge stays in frame.
    let deep = depths
        .first()
        .zip(depths.last())
        .map_or(1.0, |(front, back)| ((front - back) / 40.0).max(1.0));
    let radius = (80.0 + 5.0 * (t * 0.07).sin()) * camera.dolly * fit * deep;
    // Aim left of the scene's centre so the labels at the NOW end have room.
    let target = Vec3D::new(-6.0, 0.0, 1.5);
    let eye = target
        + Vec3D::new(
            radius * angle.sin(),
            radius * 0.72 + camera.lift + 4.0 * (t * 0.19).sin(),
            radius * angle.cos(),
        );
    let mut viewport = Viewport::new(
        Transform3D::look_at_lh(eye, target, Vec3D::NEG_Y),
        FOV,
        Vec2D::new(width as i64 / 2, height as i64 / 2),
    );
    viewport.display_mode = DisplayMode::Solid;
    viewport.character_width_multiplier = 1.0; // samples are square
    let (ground, ridges) = build_scene(&visible, &depths, scroll, eye, (eye - target).length());
    for mesh in [ground, ridges] {
        viewport.objects = vec![clip_scene(mesh, &viewport, (width, height))];
        viewport.draw_to(&mut canvas);
    }

    let cell =
        |point: Vec3D| PixelCanvas::cell_of(project(&viewport, Transform3D::IDENTITY, point));
    let columns = dims.0 as i64;
    if let Some(&front) = depths.first() {
        for (from_now, text) in [
            (48, "-2h"),
            (36, "-90m"),
            (24, "-1h"),
            (12, "-30m"),
            (0, "now"),
        ] {
            let (col, row) = cell(Vec3D::new(
                slot_x((HISTORY - from_now) as f64),
                0.0,
                front + 1.5,
            ));
            let tint = if from_now == 0 {
                (255, 244, 185)
            } else {
                MUTED
            };
            canvas.label(
                (col - text.len() as i64 / 2).clamp(0, columns - text.len() as i64),
                row.clamp(1, dims.1 as i64 - 2),
                text,
                tint,
            );
        }
    }
    // Name and live value at each ridge's NOW end, drawn over the time ruler so
    // a ridge is never left unnamed. Ridges run front to back, which is bottom
    // to top on screen: keep that order, one row each, between the header and
    // the ruler, whatever the zoom or steering does to the projection.
    let mut cells: Vec<(i64, i64)> = depths
        .iter()
        .map(|&z| cell(Vec3D::new(slot_x(HISTORY as f64) - 1.4, 0.3, z)))
        .collect();
    let mut below = dims.1 as i64 - 2;
    for cell in &mut cells {
        cell.1 = cell.1.min(below - 1);
        below = cell.1;
    }
    let mut above = 0;
    for cell in cells.iter_mut().rev() {
        cell.1 = cell.1.max(above + 1);
        above = cell.1;
    }
    for (channel, (col, row)) in visible.iter().zip(cells) {
        let value = channel.now.map(|v| channel.format(v)).unwrap_or_default();
        let col = col.clamp(0, (columns - 8 - value.chars().count() as i64).max(0));
        let hue = mix(channel.hue(), (255, 255, 255), 0.18);
        canvas.label(col, row, &channel.label(), hue);
        canvas.label(col + 7, row, &value, mix(hue, (255, 255, 255), 0.4));
    }
    canvas.label(1, 0, "SYSTEMSCAPE", (63, 235, 255));
    match bar {
        Some(bar) => {
            bar.draw(&mut canvas, 13, 0);
            // A host with nothing to draw yet says why in the middle of the scene.
            if visible.is_empty() {
                let note = bar.trouble();
                let note = note.trim_end_matches(" · ");
                let col = (dims.0 as i64 - note.chars().count() as i64) / 2;
                canvas.label(col.max(1), dims.1 as i64 / 2, note, MUTED);
            }
        }
        None => canvas.label(
            14,
            0,
            "2h of 150s peaks · height + glow = value · IO/NET log scale",
            MUTED,
        ),
    }
    let extras: Vec<String> = [("PCH", snapshot.pch), ("NIC", snapshot.nic)]
        .iter()
        .filter_map(|(name, v)| v.map(|v| format!("{name} {v:.0}°  ·  ")))
        .collect();
    canvas.label(
        1,
        dims.1 as i64 - 1,
        &format!(
            "{}{}{} · Space fly/hold · arrows steer · +/- zoom · 0 reset · f flat · {}q quit",
            bar.map(HostBar::trouble).unwrap_or_default(),
            extras.concat(),
            if camera.fly { "FLYING" } else { "HOLDING" },
            if bar.is_some() { "Tab host · " } else { "" },
        ),
        MUTED,
    );
    canvas.rows()
}

/// Telemetry options from the command line.
pub struct Options {
    pub demo: bool,
    /// An explicit hosts file; otherwise the default path is used if present.
    pub hosts: Option<std::path::PathBuf>,
}

/// `--probe`: print one snapshot of this machine per line until stdout
/// closes. This is what a viewer on another machine runs over ssh. There is
/// no screen here to tell whether anyone is looking, so the costly GPU query
/// runs on its own, slower, fixed interval and the last cards are repeated.
pub fn probe(interval: Duration, gpu_interval: Duration) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let (mut prev_cpu, mut rates) = ((0u64, 0u64), RateState::default());
    let (mut cards, mut gpu_at): (Vec<Gpu>, Option<Instant>) = (Vec::new(), None);
    loop {
        let started = Instant::now();
        let due = gpu_at.is_none_or(|at| at.elapsed() >= gpu_interval);
        let mut snapshot = poll_sensors(&mut prev_cpu, &mut rates, due);
        if due {
            gpu_at = Some(started);
            cards = snapshot.gpus.clone();
        } else {
            snapshot.gpus = cards.clone();
        }
        // JSON has no NaN: an Option becomes null, but a bare list entry would not parse.
        snapshot.pkg.retain(|t| t.is_finite());
        let frame = hosts::Frame {
            v: hosts::PROBE_VERSION,
            snapshot,
        };
        // A closed pipe (the viewer quit, ssh dropped) ends the probe.
        writeln!(out, "{}", serde_json::to_string(&frame)?)?;
        out.flush()?;
        std::thread::sleep(interval.saturating_sub(started.elapsed()));
    }
}

/// Where one host's readings come from.
enum Feed {
    /// This machine's sensor thread.
    Local,
    Remote(Remote),
}

/// One machine on screen or in the background: its ridges and its link.
struct Source {
    name: String,
    feed: Feed,
    channels: Vec<Channel>,
    snapshot: Snapshot,
    link: Link,
    last_seen: Option<Instant>,
}

impl Source {
    fn new(name: String, feed: Feed, cards: usize) -> Self {
        let link = match feed {
            Feed::Local => Link::Live,
            Feed::Remote(_) => Link::Connecting,
        };
        Self {
            name,
            feed,
            channels: channels_for(cards),
            snapshot: Snapshot::default(),
            link,
            last_seen: None,
        }
    }

    fn apply(&mut self, snapshot: Snapshot) {
        self.snapshot = snapshot;
        ensure_cards(&mut self.channels, self.snapshot.gpus.len());
        poll_into_channels(&mut self.channels, &self.snapshot);
        self.last_seen = Some(Instant::now());
        self.link = Link::Live;
    }

    /// No current reading: clear live values. History keeps what it had and
    /// later slots commit as gaps.
    fn silence(&mut self) {
        self.snapshot = Snapshot::default();
        for channel in &mut self.channels {
            channel.now = None;
        }
    }

    /// File every report a remote worker has sent. Returns whether anything changed.
    fn drain(&mut self) -> bool {
        let Feed::Remote(remote) = &self.feed else {
            return false;
        };
        let reports: Vec<Report> = remote.reports.try_iter().collect();
        let changed = !reports.is_empty();
        for report in reports {
            match report {
                Report::Frame(snapshot) => self.apply(*snapshot),
                Report::Link(Link::Live) => {} // the next frame says so
                Report::Link(link) => {
                    self.link = link;
                    self.silence();
                }
            }
        }
        changed
    }

    /// A live remote that has gone quiet for three intervals is stale: its
    /// readings are cleared rather than shown as current.
    fn check_stale(&mut self) -> bool {
        let Feed::Remote(remote) = &self.feed else {
            return false;
        };
        let limit = (remote.interval * 3).max(Duration::from_secs(6));
        let quiet = self.last_seen.is_none_or(|at| at.elapsed() > limit);
        if self.link == Link::Live && quiet {
            self.link = Link::Stale;
            self.silence();
            return true;
        }
        false
    }
}

/// The host strip: every machine, its link, and which one is on screen.
struct HostBar<'a> {
    hosts: Vec<(&'a str, &'a Link)>,
    selected: usize,
}

impl HostBar<'_> {
    fn link_colour(link: &Link) -> Rgb {
        match link {
            Link::Live => (110, 220, 110),
            Link::Connecting => (240, 200, 90),
            Link::Stale => (200, 150, 90),
            Link::Down(_) => (255, 95, 95),
        }
    }

    /// Draw the strip from `col` on `row`.
    fn draw(&self, canvas: &mut PixelCanvas, mut col: i64, row: i64) {
        for (i, (name, link)) in self.hosts.iter().enumerate() {
            let selected = i == self.selected;
            let marker = if selected { "▸" } else { " " };
            canvas.label(col, row, marker, (63, 235, 255));
            canvas.label(
                col + 1,
                row,
                &link.glyph().to_string(),
                Self::link_colour(link),
            );
            let tint = if selected { (235, 242, 250) } else { MUTED };
            canvas.label(col + 3, row, name, tint);
            col += 5 + name.chars().count() as i64;
        }
    }

    /// The selected host's trouble, if any, for the status line.
    fn trouble(&self) -> String {
        match self.hosts.get(self.selected) {
            Some((name, link)) if **link != Link::Live => format!("{name}: {} · ", link.describe()),
            _ => String::new(),
        }
    }
}

pub fn run(options: Options) -> std::io::Result<()> {
    let federation: Option<Federation> = if options.demo {
        None // the demo is synthetic and never connects anywhere
    } else {
        hosts::load(options.hosts.as_deref()).map_err(std::io::Error::other)?
    };
    // Without a hosts file there is one unnamed source and no host strip:
    // exactly the single-machine view.
    let (local_name, remotes) = match federation {
        None => (Some(String::new()), Vec::new()),
        Some(f) => (f.local, f.hosts),
    };

    let (tx, rx) = mpsc::channel();
    // nvidia-smi takes tens of milliseconds; polling off-thread keeps frames even.
    let on_screen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let sensors = local_name.is_some().then(|| {
        let sensor_sight = on_screen.clone();
        std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            let (mut prev_cpu, mut rates) = ((0u64, 0u64), RateState::default());
            let mut gpu_at: Option<Instant> = None;
            loop {
                let gpus = sensor_sight.load(Ordering::Relaxed)
                    || gpu_at.is_none_or(|at| at.elapsed() >= BACKGROUND_GPU_POLL);
                if gpus {
                    gpu_at = Some(Instant::now());
                }
                if tx
                    .send(poll_sensors(&mut prev_cpu, &mut rates, gpus))
                    .is_err()
                {
                    break;
                }
                // `unpark` when the view comes back on screen cuts the wait short.
                std::thread::park_timeout(POLL);
            }
        })
    });
    let wake_sensors = || {
        if let Some(sensors) = &sensors {
            sensors.thread().unpark();
        }
    };

    let mut sources: Vec<Source> = Vec::new();
    if let Some(name) = local_name {
        // Demo always shows the multi-GPU layout, even on a host without cards.
        let mut local = Source::new(name, Feed::Local, if options.demo { 3 } else { 0 });
        // Start even if the first poll is slow; late cards join via `ensure_cards`.
        local.apply(rx.recv_timeout(Duration::from_secs(3)).unwrap_or_default());
        if options.demo {
            demo_prefill(&mut local.channels);
        }
        sources.push(local);
    }
    sources.extend(
        remotes
            .into_iter()
            .map(|host| Source::new(host.name.clone(), Feed::Remote(Remote::spawn(host)), 0)),
    );
    let local = sources.iter().position(|s| matches!(s.feed, Feed::Local));
    let federated = sources.len() > 1 || local.is_none();
    let mut selected = 0usize;

    let _terminal = crate::Terminal::enter()?;
    let mut sight = crate::Sight::new();
    let mut buffer = crate::FrameBuffer::default();
    let mut camera = Camera::default();
    let mut flat = false;
    let mut slot_started = Instant::now();
    let mut last_frame = Instant::now();
    // While holding, the picture only changes with new data, input or the slow
    // slide of the time axis, so redraw twice a second instead of every frame.
    let mut dirty = true;
    let mut drawn = Instant::now();
    loop {
        let frame_started = Instant::now();
        if let Some(i) = local {
            while let Ok(fresh) = rx.try_recv() {
                sources[i].apply(fresh);
                dirty |= i == selected;
            }
        }
        for (i, source) in sources.iter_mut().enumerate() {
            let changed = source.drain() | source.check_stale();
            // A link change redraws the strip, whichever host it is.
            dirty |= changed && (i == selected || federated);
        }
        if slot_started.elapsed().as_secs_f64() >= SLOT_SECS {
            slot_started += Duration::from_secs_f64(SLOT_SECS);
            for channel in sources.iter_mut().flat_map(|s| s.channels.iter_mut()) {
                channel.commit_slot();
            }
            // The next slot starts empty: re-applying the last poll would
            // count one spike in two slots.
            dirty = true;
        }
        let back = sight.tick();
        let visible = sight.visible();
        if back || visible != on_screen.load(std::sync::atomic::Ordering::Relaxed) {
            on_screen.store(visible, std::sync::atomic::Ordering::Relaxed);
            if visible {
                wake_sensors(); // fresh readings straight away
                dirty = true;
            }
        }
        // The flight animates only while the view has the keyboard; visible
        // but unfocused it holds still and redraws twice a second.
        let flying = camera.fly && sight.focused();
        if flying {
            camera.t += last_frame.elapsed().as_secs_f64().min(0.25);
        }
        last_frame = Instant::now();
        let dims = crate::term_dims();
        // Off screen nothing is drawn; history keeps filling from the sensor
        // thread. A new or resized pane is always drawn once, so it is never blank.
        if buffer.dims != dims
            || visible && (flying || dirty || drawn.elapsed() >= Duration::from_millis(500))
        {
            let scroll = slot_started.elapsed().as_secs_f64() / SLOT_SECS;
            let bar = federated.then(|| HostBar {
                hosts: sources.iter().map(|s| (s.name.as_str(), &s.link)).collect(),
                selected,
            });
            let shown = &sources[selected];
            buffer.present_rows(
                render(
                    &shown.channels,
                    &shown.snapshot,
                    &camera,
                    scroll,
                    dims,
                    flat,
                    bar.as_ref(),
                ),
                dims,
            )?;
            dirty = false;
            drawn = Instant::now();
        }

        // Off screen the loop wakes once a second, only to file sensor readings.
        let budget = match (visible, flying) {
            (false, _) => Duration::from_secs(1),
            (true, true) => Duration::from_millis(FRAME_MS).saturating_sub(frame_started.elapsed()),
            (true, false) => Duration::from_millis(250),
        };
        if !event::poll(budget)? {
            continue;
        }
        let input = event::read()?;
        if let Event::FocusGained | Event::FocusLost = input {
            if sight.focus(input == Event::FocusGained) {
                on_screen.store(true, std::sync::atomic::Ordering::Relaxed);
                wake_sensors();
            }
            dirty = true;
        }
        if let Event::Key(key) = input {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            dirty = true;
            let hosts = sources.len();
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(())
                }
                KeyCode::Tab => selected = (selected + 1) % hosts,
                KeyCode::BackTab => selected = (selected + hosts - 1) % hosts,
                KeyCode::Char(digit @ '1'..='9') => {
                    let index = digit as usize - '1' as usize;
                    if index < hosts {
                        selected = index;
                    }
                }
                KeyCode::Char(' ') => camera.fly = !camera.fly,
                KeyCode::Left => camera.yaw = (camera.yaw - 0.08).max(-0.9),
                KeyCode::Right => camera.yaw = (camera.yaw + 0.08).min(0.9),
                KeyCode::Up => camera.lift = (camera.lift + 2.0).min(40.0),
                KeyCode::Down => camera.lift = (camera.lift - 2.0).max(-14.0),
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    camera.dolly = (camera.dolly * 0.9).max(0.45)
                }
                KeyCode::Char('-') => camera.dolly = (camera.dolly * 1.1).min(2.5),
                KeyCode::Char('0') => {
                    camera = Camera {
                        t: camera.t,
                        fly: camera.fly,
                        ..Camera::default()
                    }
                }
                KeyCode::Char('f') => flat = !flat,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn gpus_stay_separate_and_tolerate_missing_fields() {
        let gpus =
            parse_gpus("32, 0, 2255, 49140\n46, [N/A], 31092, 49140\n\n42, 97, 2706, 49140\n");
        assert_eq!(gpus.len(), 3);
        assert_eq!(gpus[1].temp, Some(46.0));
        assert_eq!(gpus[1].util, None);
        assert!((gpus[1].used_gib.unwrap() - 30.36).abs() < 0.01);

        let snapshot = Snapshot {
            gpus,
            ..Snapshot::default()
        };
        let mut channels = channels_for(3);
        poll_into_channels(&mut channels, &snapshot);
        let now = |kind, card| {
            channels
                .iter()
                .find(|c| c.kind == kind && c.gpu == Some(card))
                .and_then(|c| c.now)
        };
        assert_eq!(now(Kind::GpuTemp, 0), Some(32.0));
        assert_eq!(now(Kind::GpuTemp, 2), Some(42.0));
        assert_eq!(now(Kind::GpuUtil, 1), None);
        assert_eq!(now(Kind::GpuUtil, 2), Some(97.0));
        let vram = channels
            .iter()
            .find(|c| c.kind == Kind::Vram && c.gpu == Some(1))
            .unwrap();
        assert_eq!(vram.format(vram.now.unwrap()), "30/48G");
        assert!((vram.norm(vram.now.unwrap()) - 0.633).abs() < 0.01);
    }

    #[test]
    fn throughput_scale_keeps_small_traffic_visible() {
        let net = Channel::new(Kind::Net, None);
        assert_eq!(net.norm(0.0), 0.0);
        assert!(
            net.norm(12.0) > 0.3,
            "12 MB/s must rise clearly off the floor"
        );
        assert!(net.norm(12.0) < net.norm(120.0) && net.norm(120.0) < net.norm(1250.0));
        assert_eq!(net.norm(99_999.0), 1.0);
    }

    #[test]
    fn peak_hold_commits_the_slot_maximum_and_bounds_history() {
        let mut channel = Channel::new(Kind::Load, None);
        assert!(!channel.has_data());
        for v in [10.0, 80.0, 30.0] {
            channel.poll(v);
        }
        channel.commit_slot();
        assert_eq!(channel.history.back(), Some(&80.0));
        for _ in 0..HISTORY + 5 {
            channel.poll(1.0);
            channel.commit_slot();
        }
        assert_eq!(channel.history.len(), HISTORY);
    }

    #[test]
    fn silent_slots_become_gaps_and_late_cards_stay_aligned() {
        let mut channels = channels_for(1);
        let reading = |gpus| Snapshot {
            gpus,
            power_w: Some(300.0),
            ..Snapshot::default()
        };
        let card = Gpu {
            temp: Some(50.0),
            ..Gpu::default()
        };
        for gpus in [vec![card], vec![], vec![card]] {
            poll_into_channels(&mut channels, &reading(gpus));
            for channel in &mut channels {
                channel.commit_slot();
            }
        }
        let find = |channels: &[Channel], kind, gpu| {
            channels
                .iter()
                .position(|c| c.kind == kind && c.gpu == gpu)
                .unwrap()
        };
        let temp = &channels[find(&channels, Kind::GpuTemp, Some(0))];
        assert_eq!(
            temp.history.len(),
            3,
            "a silent slot still occupies its place"
        );
        assert!(temp.history[1].is_nan() && temp.history[2] == 50.0);
        assert_eq!(
            channels[find(&channels, Kind::Power, None)].history.len(),
            3
        );

        // The card answered, then fell silent: its live value clears.
        poll_into_channels(&mut channels, &reading(vec![]));
        assert_eq!(channels[find(&channels, Kind::GpuTemp, Some(0))].now, None);

        // A second card appears later and joins with matching history length.
        ensure_cards(&mut channels, 2);
        let late = &channels[find(&channels, Kind::GpuTemp, Some(1))];
        assert_eq!(late.history.len(), 3);
        assert!(!late.has_data());
        assert_eq!(
            channels[find(&channels, Kind::GpuTemp, Some(0))].history[2],
            50.0
        );
        ensure_cards(&mut channels, 1); // never drops cards
        assert_eq!(
            channels.iter().filter(|c| c.kind == Kind::GpuTemp).count(),
            2
        );
    }

    #[test]
    fn every_ridge_is_labelled_at_any_size_without_panicking() {
        let mut channels = channels_for(3);
        demo_prefill(&mut channels);
        let snapshot = Snapshot::default();
        for channel in &mut channels {
            let last = *channel.history.back().unwrap();
            channel.poll(last);
        }
        let plain = |rows: &[String]| {
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
        };
        // Body rows only: the header mentions IO and NET by name.
        let named = |rows: &[String], channels: &[Channel]| {
            let body = plain(&rows[1..rows.len() - 1]);
            channels
                .iter()
                .filter(|c| !body.contains(&c.label()))
                .map(Channel::label)
                .collect::<Vec<_>>()
        };
        for scroll in [0.0, 0.5, 0.999] {
            let rows = render(
                &channels,
                &snapshot,
                &Camera::default(),
                scroll,
                (165, 35),
                false,
                None,
            );
            assert_eq!(rows.len(), 35);
            assert_eq!(named(&rows, &channels), Vec::<String>::new());
            let text = plain(&rows);
            assert!(text.contains("now") && text.contains("-1h"));
        }
        // Zoom and steering limits, many cards, small and huge panes: every
        // ridge keeps its name, or the sparkline view says how many are hidden.
        let mut many = channels_for(8);
        demo_prefill(&mut many);
        for channel in &mut many {
            let last = *channel.history.back().unwrap();
            channel.poll(last);
        }
        let cameras = [
            (0.0, 0.0, 1.0),
            (0.0, 0.0, 0.45),
            (0.0, 0.0, 2.5),
            (0.9, 40.0, 1.0),
            (-0.9, -14.0, 1.0),
            (0.9, 0.0, 0.45),
        ];
        for set in [&channels, &many] {
            for dims in [
                (80, 20),
                (80, 24),
                (100, 30),
                (165, 35),
                (80, 60),
                (200, 60),
                (512, 180),
            ] {
                for (yaw, lift, dolly) in cameras {
                    let camera = Camera {
                        yaw,
                        lift,
                        dolly,
                        ..Camera::default()
                    };
                    let rows = render(set, &snapshot, &camera, 0.3, dims, false, None);
                    assert_eq!(rows.len(), dims.1);
                    let missing = named(&rows, set);
                    assert!(
                        missing.is_empty() || plain(&rows).contains("more channels"),
                        "{dims:?} yaw {yaw} dolly {dolly}: unnamed ridges {missing:?}"
                    );
                }
            }
        }
        for dims in [(1, 1), (30, 8), (47, 27)] {
            assert_eq!(
                render(
                    &channels,
                    &snapshot,
                    &Camera::default(),
                    0.3,
                    dims,
                    false,
                    None
                )
                .len(),
                dims.1
            );
        }
        let flat = plain(&render(
            &channels,
            &snapshot,
            &Camera::default(),
            0.3,
            (47, 27),
            false,
            None,
        ));
        assert!(flat.contains("GPU1%") && flat.contains('▁'));
        render(
            &[],
            &snapshot,
            &Camera::default(),
            0.0,
            (165, 35),
            false,
            None,
        );
    }

    fn plain_text(rows: &[String]) -> String {
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

    #[test]
    fn amd_cpu_temperature_prefers_core_dies_over_the_control_value() {
        let t = |pairs: &[(&str, f64)]| {
            amd_cpu_temp(
                &pairs
                    .iter()
                    .map(|(l, v)| (l.to_string(), *v))
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            t(&[("Tctl", 70.0), ("Tccd1", 61.0), ("Tccd2", 64.5)]),
            Some(64.5)
        );
        assert_eq!(t(&[("Tctl", 80.0), ("Tdie", 70.0)]), Some(70.0));
        assert_eq!(t(&[("Tctl", 55.0)]), Some(55.0));
        assert_eq!(t(&[]), None);
    }

    #[test]
    fn the_host_strip_names_every_host_and_explains_an_empty_one() {
        let (live, down) = (
            Link::Live,
            Link::Down("ssh failed: Permission denied".into()),
        );
        let bar = HostBar {
            hosts: vec![("here", &live), ("lab", &down)],
            selected: 1,
        };
        let empty = channels_for(0);
        for (dims, flat) in [((165, 35), false), ((60, 16), false), ((165, 35), true)] {
            let text = plain_text(&render(
                &empty,
                &Snapshot::default(),
                &Camera::default(),
                0.0,
                dims,
                flat,
                Some(&bar),
            ));
            assert!(
                text.contains("● here") && text.contains("▸! lab"),
                "{dims:?}\n{text}"
            );
            assert!(
                text.contains("lab: ssh failed: Permission denied"),
                "{dims:?}"
            );
            assert!(text.contains("Tab host"));
        }
        // A healthy selected host shows no trouble text, and no strip without a bar.
        let mut channels = channels_for(1);
        demo_prefill(&mut channels);
        let healthy = HostBar {
            hosts: vec![("here", &live), ("lab", &down)],
            selected: 0,
        };
        let text = plain_text(&render(
            &channels,
            &Snapshot::default(),
            &Camera::default(),
            0.0,
            (165, 35),
            false,
            Some(&healthy),
        ));
        assert!(!text.contains("here:") && !text.contains("lab:"));
        let alone = plain_text(&render(
            &channels,
            &Snapshot::default(),
            &Camera::default(),
            0.0,
            (165, 35),
            false,
            None,
        ));
        assert!(!alone.contains("Tab host") && alone.contains("height + glow"));
    }

    #[test]
    fn a_quiet_remote_turns_stale_and_stops_showing_old_readings() {
        let host = hosts::Host {
            name: "quiet".into(),
            argv: vec![
                "sh".into(),
                "-c".into(),
                "echo '{\"v\":1,\"snapshot\":{\"mem_pct\":55}}'; sleep 30".into(),
            ],
            interval: Duration::from_millis(500),
        };
        let mut source = Source::new("quiet".into(), Feed::Remote(Remote::spawn(host)), 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while source.snapshot.mem_pct.is_none() && Instant::now() < deadline {
            source.drain();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(source.link, Link::Live);
        let mem = |s: &Source| s.channels.iter().find(|c| c.kind == Kind::Mem).unwrap().now;
        assert_eq!(mem(&source), Some(55.0));
        assert!(!source.check_stale(), "fresh readings are not stale");
        source.last_seen = Some(Instant::now() - Duration::from_secs(7));
        assert!(source.check_stale());
        assert_eq!(source.link, Link::Stale);
        assert_eq!(mem(&source), None, "a stale host shows no live value");
        assert!(
            source.channels.iter().any(Channel::has_data),
            "history is kept"
        );
    }

    #[test]
    fn a_snapshot_survives_the_probe_wire() {
        let snapshot = Snapshot {
            pkg: vec![61.5],
            gpus: vec![Gpu {
                temp: Some(67.0),
                util: None,
                used_gib: Some(22.5),
                total_gib: Some(24.0),
            }],
            mem_pct: Some(f64::NAN), // JSON has no NaN: it arrives as no reading
            net_mbps: Some(3.25),
            ..Snapshot::default()
        };
        let line = serde_json::to_string(&hosts::Frame {
            v: hosts::PROBE_VERSION,
            snapshot,
        })
        .unwrap();
        let back: hosts::Frame = serde_json::from_str(&line).unwrap();
        assert_eq!(back.snapshot.pkg, vec![61.5]);
        assert_eq!(back.snapshot.gpus[0].used_gib, Some(22.5));
        assert_eq!(back.snapshot.gpus[0].util, None);
        assert_eq!(back.snapshot.mem_pct, None);
        assert_eq!(back.snapshot.net_mbps, Some(3.25));
    }
}
