//! Gaming profile model: a named bundle of CPU, GPU, cooling, lighting and
//! platform settings, plus the automation rules that select profiles.

use crate::cpu::CpuControlState;
use crate::model::HardwareModel;
use crate::nvidia::NvidiaControl;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A fan curve in device-agnostic terms: (°C, duty %) points.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FanCurve {
    pub points: Vec<(f64, f64)>,
    /// Which temperature drives the curve.
    pub source: TempSource,
    /// Hysteresis / smoothing in °C.
    pub hysteresis_c: f64,
    /// Minimum duty ever applied (protects pumps).
    pub min_duty: f64,
    /// Seconds over which duty ramps (0 = instant).
    pub ramp_s: f64,
}

impl FanCurve {
    /// Interpolate duty (0..=100) for a temperature.
    pub fn duty_at(&self, temp: f64) -> f64 {
        let pts = &self.points;
        if pts.is_empty() {
            return self.min_duty.max(0.0);
        }
        let mut sorted: Vec<(f64, f64)> = pts.clone();
        sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        if temp <= sorted[0].0 {
            return sorted[0].1.max(self.min_duty);
        }
        for w in sorted.windows(2) {
            let (t0, d0) = w[0];
            let (t1, d1) = w[1];
            if temp <= t1 {
                let f = if (t1 - t0).abs() < f64::EPSILON { 1.0 } else { (temp - t0) / (t1 - t0) };
                return (d0 + f * (d1 - d0)).max(self.min_duty).clamp(0.0, 100.0);
            }
        }
        sorted.last().map(|p| p.1).unwrap_or(100.0).max(self.min_duty).clamp(0.0, 100.0)
    }

    /// The curve as `n` points, duties from [`duty_at`](Self::duty_at) so the
    /// minimum duty holds: its own temperatures when it already has `n`
    /// points, else `n` evenly spaced across its range.
    pub fn resample(&self, n: usize) -> Vec<(f64, f64)> {
        let mut temps: Vec<f64> = self.points.iter().map(|p| p.0).collect();
        temps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if temps.len() != n {
            let (lo, hi) = (temps.first().copied().unwrap_or(30.0), temps.last().copied().unwrap_or(90.0));
            temps = (0..n).map(|i| if n < 2 { lo } else { lo + (hi - lo) * i as f64 / (n - 1) as f64 }).collect();
        }
        temps.into_iter().map(|t| (t, self.duty_at(t))).collect()
    }

    pub fn silent() -> Self {
        Self { points: vec![(30.0, 20.0), (50.0, 30.0), (65.0, 45.0), (75.0, 70.0), (85.0, 100.0)], source: TempSource::CpuTctl, hysteresis_c: 2.0, min_duty: 20.0, ramp_s: 4.0 }
    }
    pub fn balanced() -> Self {
        Self { points: vec![(30.0, 30.0), (50.0, 40.0), (60.0, 55.0), (70.0, 75.0), (80.0, 100.0)], source: TempSource::CpuTctl, hysteresis_c: 1.5, min_duty: 25.0, ramp_s: 3.0 }
    }
    pub fn performance() -> Self {
        Self { points: vec![(30.0, 40.0), (45.0, 55.0), (55.0, 70.0), (65.0, 85.0), (75.0, 100.0)], source: TempSource::CpuTctl, hysteresis_c: 1.0, min_duty: 35.0, ramp_s: 2.0 }
    }
    pub fn coolant() -> Self {
        Self { points: vec![(25.0, 30.0), (30.0, 40.0), (35.0, 60.0), (40.0, 85.0), (45.0, 100.0)], source: TempSource::Coolant, hysteresis_c: 0.5, min_duty: 30.0, ramp_s: 5.0 }
    }
    pub fn pump() -> Self {
        Self { points: vec![(25.0, 60.0), (32.0, 70.0), (38.0, 85.0), (42.0, 100.0)], source: TempSource::Coolant, hysteresis_c: 0.5, min_duty: 60.0, ramp_s: 6.0 }
    }
}

/// Where a curve reads its temperature from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum TempSource {
    #[default]
    CpuTctl,
    CpuPackage,
    Gpu,
    Coolant,
    Vrm,
    Motherboard,
    /// Max of CPU and GPU: the usual case-fan driver.
    CpuGpuMax,
    /// Arbitrary hwmon input: (driver name, temp label).
    Hwmon { driver: String, label: String },
}

impl TempSource {
    pub fn label(&self) -> String {
        match self {
            Self::CpuTctl => "CPU (Tctl)".into(),
            Self::CpuPackage => "CPU package".into(),
            Self::Gpu => "GPU".into(),
            Self::Coolant => "Coolant".into(),
            Self::Vrm => "VRM".into(),
            Self::Motherboard => "Motherboard".into(),
            Self::CpuGpuMax => "CPU / GPU max".into(),
            Self::Hwmon { driver, label } => format!("{driver}: {label}"),
        }
    }
}

/// A fan output, by the id the hardware model gives it (`asusd:fan:CPU`,
/// `superio:pwm2`, `ryujin:pump`...). Profiles keep assignments for outputs a
/// machine doesn't have, so they carry between machines; those are not driven.
pub type FanTarget = crate::model::DeviceId;

/// How a fan is controlled inside a profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FanMode {
    /// Leave to firmware / driver default.
    Auto,
    /// Fixed duty percent.
    Fixed(f64),
    /// Software curve evaluated by OmaAsus (or handed to CoolerControl).
    Curve(FanCurve),
    /// Hardware Smart Fan IV curve written into the Super I/O (nct6775 only).
    HardwareCurve(FanCurve),
}

/// A fan output with the mode assigned to it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FanAssignment {
    pub target: FanTarget,
    pub mode: FanMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CoolingSettings {
    pub fans: Vec<FanAssignment>,
    /// Zero-RPM allowed for GPU fans (NVIDIA default policy).
    pub gpu_zero_rpm: bool,
}

impl CoolingSettings {
    pub fn set(&mut self, target: FanTarget, mode: FanMode) {
        match self.fans.iter_mut().find(|f| f.target == target) {
            Some(f) => f.mode = mode,
            None => self.fans.push(FanAssignment { target, mode }),
        }
    }
    pub fn get(&self, target: &FanTarget) -> Option<&FanMode> {
        self.fans.iter().find(|f| &f.target == target).map(|f| &f.mode)
    }
    pub fn remove(&mut self, target: &FanTarget) {
        self.fans.retain(|f| &f.target != target);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CpuSettings {
    pub control: Option<CpuControlState>,
    /// Pin the game to physical cores of CCD0 (Zen 4 gaming trick) via cgroup/cpuset.
    pub prefer_ccd0: bool,
    /// Power mode to select, by the machine's name for it ("Quiet", "Balanced",
    /// "power-saver"...). Applied through whichever component owns power modes;
    /// a name from another machine maps to its nearest mode ([`match_power_mode`]).
    #[serde(default)]
    pub power_mode: Option<String>,
    /// Config v1: power-profiles-daemon profile. Read only, to migrate into `power_mode`.
    #[serde(default, skip_serializing)]
    pub ppd_profile: Option<String>,
    /// Config v1: `platform_profile` value. Read only, to migrate into `power_mode`.
    #[serde(default, skip_serializing)]
    pub platform_profile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct GpuSettings {
    pub nvidia: Option<NvidiaControl>,
    /// amdgpu `power_dpm_force_performance_level`.
    pub amd_perf_level: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
    pub fn hex(&self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LightingMode {
    Off,
    Static(Rgb),
    Breathing(Rgb),
    Rainbow,
    /// Colour follows a temperature source between two colours.
    Thermal { source: TempSource, cool: Rgb, hot: Rgb, min_c: f64, max_c: f64 },
    /// Direct per-LED colours (OpenRGB direct mode).
    Direct(Vec<Rgb>),
    /// One of the device's own effects, by its number (an asusd Aura mode as
    /// asusd lists it, a Slash animation, an OpenRGB mode index).
    Firmware(u32),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LightingSettings {
    /// Keyed by device: a model `DeviceId` (`asusd:slash`) or `openrgb:<name>`.
    pub zones: BTreeMap<String, LightingMode>,
    /// Brightness for every device in percent; 0 leaves each as it is.
    pub brightness: u8,
    /// Brightness per device in percent, over `brightness`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub device_brightness: BTreeMap<String, u8>,
}

impl LightingSettings {
    /// The brightness to set on a device, if the profile sets one.
    pub fn brightness_for(&self, key: &str) -> Option<u8> {
        self.device_brightness.get(key).copied().or((self.brightness > 0).then_some(self.brightness))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LcdSettings {
    /// Ryujin LCD / LiveDash content.
    pub content: LcdContent,
    pub brightness: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum LcdContent {
    #[default]
    Firmware,
    Temps,
    Clocks,
    Image(String),
    Gif(String),
    Text(String),
}

/// What a profile is for, so rules and shortcuts find "the quiet one" or "the
/// gaming one" without relying on names.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProfileRole {
    Quiet,
    Balanced,
    Performance,
}

/// A full gaming profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub id: uuid::Uuid,
    pub name: String,
    #[serde(default)]
    pub role: Option<ProfileRole>,
    pub icon: String,
    pub accent: Rgb,
    pub cpu: CpuSettings,
    pub gpu: GpuSettings,
    pub cooling: CoolingSettings,
    pub lighting: LightingSettings,
    pub lcd: LcdSettings,
    /// Laptop-only knobs applied through asusd, kept generic.
    pub asusd: BTreeMap<String, String>,
    /// supergfxctl mode name to request (laptops).
    pub gfx_mode: Option<String>,
    /// CoolerControl Mode to activate with this profile (when CoolerControl owns fans).
    #[serde(default)]
    pub cc_mode: Option<String>,
    pub builtin: bool,
}

impl Profile {
    /// Whether this profile's power mode is `live`, one of the machine's
    /// `choices` (a profile made under power-profiles-daemon names its
    /// modes; asusd's are matched by kind).
    pub fn carries_power_mode(&self, live: &str, choices: &[String]) -> bool {
        self.cpu.power_mode.as_deref().and_then(|w| match_power_mode(w, choices)).is_some_and(|m| m.eq_ignore_ascii_case(live))
    }

    pub fn new(name: &str) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            name: name.into(),
            role: None,
            icon: "bolt".into(),
            accent: Rgb::new(255, 64, 96),
            cpu: CpuSettings::default(),
            gpu: GpuSettings::default(),
            cooling: CoolingSettings::default(),
            lighting: LightingSettings::default(),
            lcd: LcdSettings::default(),
            asusd: BTreeMap::new(),
            gfx_mode: None,
            cc_mode: None,
            builtin: false,
        }
    }
}

/// When the automatic engine should switch to a profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Trigger {
    /// A GameMode client registered.
    GameMode,
    /// A fullscreen window that looks like a game is focused.
    FullscreenGame,
    /// A window with this class (exact or glob) is focused.
    WindowClass(String),
    /// A process with this executable name is running.
    Process(String),
    /// CPU package temperature above a threshold for N seconds.
    CpuHot { above_c: f64, for_s: u32 },
    /// GPU above a temperature.
    GpuHot { above_c: f64, for_s: u32 },
    /// Time window (24h, local), e.g. quiet at night.
    Time { from: (u8, u8), to: (u8, u8) },
    /// System idle for N seconds (no input, via Hyprland idle / swayidle hook).
    Idle { for_s: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub id: uuid::Uuid,
    pub name: String,
    pub enabled: bool,
    pub trigger: Trigger,
    pub profile: uuid::Uuid,
    /// Higher wins when several rules match.
    pub priority: i32,
    /// Seconds to keep the profile after the trigger clears (debounce).
    pub hold_s: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Mode {
    /// The user picks the profile.
    #[default]
    Manual,
    /// Rules pick the profile; `default_profile` otherwise.
    Automatic,
}

/// Who drives the fans.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum FanOwner {
    /// CoolerControl when its daemon is running, otherwise OmaAsus.
    #[default]
    Auto,
    OmaAsus,
    CoolerControl,
    /// Leave fans to firmware / other tools.
    None,
}

/// Current config schema. v1: fixed fan names, `ppd_profile`, lighting keyed
/// by OpenRGB device name, profiles known by name.
pub const SCHEMA: u32 = 2;

fn schema_v1() -> u32 {
    1
}

/// Everything persisted for the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default = "schema_v1")]
    pub schema_version: u32,
    pub mode: Mode,
    #[serde(default)]
    pub fan_owner: FanOwner,
    pub active_profile: uuid::Uuid,
    pub default_profile: uuid::Uuid,
    pub profiles: Vec<Profile>,
    pub rules: Vec<Rule>,
    pub coolercontrol: CoolerControlAuth,
    pub overlay: OverlaySettings,
    pub telemetry_hz: u32,
    /// Keep a StatusNotifierItem in the bar so closing the window leaves the
    /// daemon (automation, fan engine, overlay) running.
    #[serde(default = "default_true")]
    pub tray_enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CoolerControlAuth {
    pub enabled: bool,
    pub url: String,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OverlaySettings {
    pub anchor: String,
    pub width: u32,
    pub height: u32,
    pub margin: u32,
    pub opacity: f32,
    pub hud_enabled: bool,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        Self { anchor: "right".into(), width: 560, height: 1040, margin: 16, opacity: 0.97, hud_enabled: true }
    }
}

impl Config {
    /// A config with no profiles yet: they are generated from the hardware
    /// model once detection has run ([`Config::generated`]).
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA,
            mode: Mode::Manual,
            fan_owner: FanOwner::Auto,
            active_profile: uuid::Uuid::nil(),
            default_profile: uuid::Uuid::nil(),
            profiles: Vec::new(),
            rules: Vec::new(),
            coolercontrol: CoolerControlAuth { enabled: false, url: "http://localhost:11987".into(), password: None },
            overlay: OverlaySettings::default(),
            telemetry_hz: 2,
            tray_enabled: true,
        }
    }

    /// Profiles for a fresh install, from what the machine has: one per power
    /// mode it offers, each changing only that. Fans stay with the firmware and
    /// nothing else is forced; users build on these.
    pub fn generated(model: &HardwareModel) -> Self {
        let mut c = Self::empty();
        let modes = &model.controls.power_modes;
        let profiles: Vec<Profile> = if modes.is_empty() {
            vec![Profile { builtin: true, role: Some(ProfileRole::Balanced), ..Profile::new("Default") }]
        } else {
            modes
                .iter()
                .map(|mode| {
                    let role = power_kind(mode);
                    let (icon, accent) = match role {
                        Some(ProfileRole::Quiet) => ("moon", Rgb::new(96, 140, 255)),
                        Some(ProfileRole::Performance) => ("gamepad", Rgb::new(255, 64, 96)),
                        _ => ("scale", Rgb::new(64, 224, 180)),
                    };
                    let mut p = Profile::new(&display_name(mode));
                    p.builtin = true;
                    p.role = role;
                    p.icon = icon.into();
                    p.accent = accent;
                    p.cpu.power_mode = Some(mode.clone());
                    p
                })
                .collect()
        };
        let balanced = profiles.iter().find(|p| p.role == Some(ProfileRole::Balanced)).unwrap_or(&profiles[0]).id;
        let current = model.controls.power_mode.as_deref();
        c.active_profile = profiles.iter().find(|p| current.is_some() && p.cpu.power_mode.as_deref() == current).map(|p| p.id).unwrap_or(balanced);
        c.default_profile = balanced;
        if let Some(perf) = profiles.iter().find(|p| p.role == Some(ProfileRole::Performance)) {
            c.rules = vec![
                Rule { id: uuid::Uuid::new_v4(), name: "GameMode active".into(), enabled: true, trigger: Trigger::GameMode, profile: perf.id, priority: 100, hold_s: 20 },
                Rule { id: uuid::Uuid::new_v4(), name: "Fullscreen game".into(), enabled: true, trigger: Trigger::FullscreenGame, profile: perf.id, priority: 50, hold_s: 20 },
            ];
        }
        c.profiles = profiles;
        c
    }

    /// Bring an older config up to [`SCHEMA`]. Fan targets are already read as
    /// model ids; this moves the rest. Returns whether anything changed.
    pub fn migrate(&mut self) -> bool {
        if self.schema_version >= SCHEMA {
            return false;
        }
        for p in &mut self.profiles {
            // v1 kept power-profiles-daemon and platform_profile names apart.
            let v1_mode = p.cpu.ppd_profile.take().or(p.cpu.platform_profile.take());
            if p.cpu.power_mode.is_none() {
                p.cpu.power_mode = v1_mode;
            }
            // v1 lighting was keyed by OpenRGB device name.
            p.lighting.zones = std::mem::take(&mut p.lighting.zones).into_iter().map(|(k, v)| (if k.contains(':') { k } else { format!("openrgb:{k}") }, v)).collect();
            // v1 built-ins were known by name.
            if p.role.is_none() && p.builtin {
                p.role = match p.name.to_ascii_lowercase().as_str() {
                    "silent" => Some(ProfileRole::Quiet),
                    "balanced" => Some(ProfileRole::Balanced),
                    "gaming" | "turbo" => Some(ProfileRole::Performance),
                    _ => None,
                };
            }
        }
        self.schema_version = SCHEMA;
        true
    }

    pub fn profile(&self, id: uuid::Uuid) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// The first profile with this role.
    pub fn profile_by_role(&self, role: ProfileRole) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.role == Some(role))
    }

    /// The profile to follow a power mode set outside a profile apply (a
    /// keyboard shortcut, `powerprofilesctl`, a bar widget): the default
    /// profile when it carries `live`, else the first in order that does.
    /// `None` when no profile carries it.
    pub fn profile_for_power_mode(&self, live: &str, choices: &[String]) -> Option<&Profile> {
        self.profile(self.default_profile).filter(|p| p.carries_power_mode(live, choices)).or_else(|| self.profiles.iter().find(|p| p.carries_power_mode(live, choices)))
    }
}

/// The kind of a power mode name, across asusd, power-profiles-daemon and sysfs names.
pub fn power_kind(name: &str) -> Option<ProfileRole> {
    match name.to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
        "quiet" | "silent" | "power-saver" | "low-power" | "cool" => Some(ProfileRole::Quiet),
        "balanced" | "balanced-performance" => Some(ProfileRole::Balanced),
        "performance" | "turbo" | "max-power" => Some(ProfileRole::Performance),
        _ => None,
    }
}

/// The machine's power mode that best matches `wanted`: the same name, else
/// the same kind, so a profile made under power-profiles-daemon still means
/// something where asusd owns power modes.
pub fn match_power_mode<'a>(wanted: &str, choices: &'a [String]) -> Option<&'a str> {
    if let Some(c) = choices.iter().find(|c| c.eq_ignore_ascii_case(wanted)) {
        return Some(c);
    }
    let kind = power_kind(wanted)?;
    choices.iter().find(|c| power_kind(c) == Some(kind)).map(String::as_str)
}

/// "power-saver" → "Power saver", "quiet" → "Quiet".
fn display_name(mode: &str) -> String {
    let spaced = mode.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}
