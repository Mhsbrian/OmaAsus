//! The hardware model: what this machine has, in stable, backend-neutral
//! terms. [`HardwareModel::build`] turns a [`RawInventory`] into it without
//! any I/O, so it is tested against captures of real machines.

use crate::asusd::PlatformProfile;
use crate::capture::{PciDevice, RawInventory};
use crate::detect::{self, Platform};
use crate::hwmon::HwmonDevice;
use crate::knowledge::{self, Identity, Overrides, Source};
use crate::lianli::HubKind;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

/// A stable, readable identifier for a fan output, sensor, GPU or lighting
/// device: the same hardware gets the same id across boots (no `hwmonN`
/// numbers), e.g. `asusd:fan:CPU`, `superio:pwm2`, `hwmon:k10temp:Tctl`.
/// Two identical sensors on one machine are told apart by a `#2` suffix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct DeviceId(pub String);

impl DeviceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for DeviceId {
    /// Reads ids, and the fixed fan names of config v1 as the ids the model
    /// gives the same outputs.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Id(String),
            Legacy(Legacy),
        }
        #[derive(serde::Deserialize)]
        enum Legacy {
            SuperIo(u32),
            LianLiChannel(u8),
            CoolerControl { device_uid: String, channel: String },
        }
        Ok(DeviceId(match Repr::deserialize(d)? {
            Repr::Id(s) => match s.as_str() {
                "RyujinPump" => "ryujin:pump".into(),
                "RyujinInternalFan" => "ryujin:block-fan".into(),
                "RyujinExternalFans" => "ryujin:radiator".into(),
                "NvidiaFans" => "nvidia:0:fans".into(),
                _ => s,
            },
            Repr::Legacy(Legacy::SuperIo(n)) => format!("superio:pwm{n}"),
            Repr::Legacy(Legacy::LianLiChannel(c)) => format!("lianli:0:ch{c}"),
            Repr::Legacy(Legacy::CoolerControl { device_uid, channel }) => format!("cc:{device_uid}:{channel}"),
        }))
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.0)
    }
}

/// What a sensor measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SensorRole {
    CpuTemp,
    CpuCore,
    IgpuTemp,
    DgpuTemp,
    GpuHotspot,
    Coolant,
    Vrm,
    Board,
    Storage,
    Memory,
    Wireless,
    PackagePower,
    GpuPower,
    FanSpeed,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SensorKind {
    Temperature,
    Power,
    Fan,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sensor {
    pub id: DeviceId,
    pub label: String,
    pub role: SensorRole,
    pub kind: SensorKind,
    pub driver: String,
    /// The attribute to read.
    pub input: PathBuf,
}

/// Which temperature a hardware curve follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CurveTemp {
    /// Chosen per curve (`pwmN_temp_sel`).
    Selectable,
    /// Fixed by the firmware (the fan's own CPU or GPU sensor).
    Firmware,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CurveSpec {
    pub points: u32,
    pub temp: CurveTemp,
}

/// What handing an output back to the hardware means.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum Release {
    /// Restore the `pwmN_enable` mode saved before taking control.
    RestoreMode,
    /// The device's own automatic behaviour (NVIDIA auto policy, Lian Li PWM
    /// sync, firmware curves switched off).
    Auto,
    /// A fixed duty, for devices with no automatic behaviour once driven.
    SafeFixed(f64),
}

/// The temperature a new software curve for an output should follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CurveInput {
    Cpu,
    Gpu,
    /// The hotter of the two: the usual case-fan driver.
    CpuOrGpu,
    Coolant,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FanCaps {
    /// Takes a fixed duty or a software curve.
    pub duty: bool,
    /// A curve the hardware runs itself.
    pub firmware_curve: Option<CurveSpec>,
    /// Lowest duty ever sent, in percent.
    pub min_duty: f64,
    pub release: Release,
    /// Speed at 100 %, for showing speed as a share of it.
    pub max_rpm: Option<u32>,
    pub curve_input: CurveInput,
}

/// How an output is driven.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum FanBackend {
    /// A hwmon PWM output (Super I/O header, AIO) at `dir/pwm{index}`.
    Hwmon { dir: PathBuf, index: u32 },
    /// A firmware curve stored and applied by asusd, per platform profile.
    AsusdCurve { fan: String },
    /// A firmware curve written straight to `asus_custom_fan_curve` (no asusd).
    AsusCurve { dir: PathBuf, index: u32, driver: String },
    Nvidia { gpu: u32, fans: u32 },
    LianLi { path: String, channel: u8 },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FanOutput {
    pub id: DeviceId,
    pub label: String,
    /// The fan-speed sensor reading this output's fan.
    pub tach: Option<DeviceId>,
    pub caps: FanCaps,
    pub backend: FanBackend,
    /// Curves the firmware stores for it, by power mode (asusd), as detected.
    pub firmware_curves: BTreeMap<String, StoredCurve>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredCurve {
    /// The custom curve is in use rather than the firmware's own.
    pub enabled: bool,
    /// (°C, duty %).
    pub points: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum GpuVendor {
    Amd,
    Nvidia,
    Intel,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum GpuPower {
    Active,
    /// Runtime-suspended (D3); reading it through the driver would wake it.
    Suspended,
    /// Removed from the bus (supergfxd Integrated mode, dGPU disabled).
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Gpu {
    pub id: DeviceId,
    pub vendor: GpuVendor,
    pub name: String,
    pub integrated: bool,
    pub power: GpuPower,
    pub pci_slot: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum LightingBackend {
    AsusdAura { path: String },
    AsusdSlash { path: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LightingDevice {
    pub id: DeviceId,
    pub label: String,
    pub backend: LightingBackend,
    /// Firmware modes, as the backend numbers them.
    pub modes: Vec<u32>,
    pub brightness_levels: Vec<u32>,
    pub leds: Option<u8>,
}

/// Which component a control goes through, so exactly one thing drives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Owner {
    Asusd,
    PowerProfilesDaemon,
    Sysfs,
    Supergfxd,
    CoolerControl,
    OmaAsus,
    Firmware,
}

/// A firmware attribute (asus-armoury). Values and ranges come from sysfs:
/// they change with AC state, and asusd's cached copies can be stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareAttr {
    pub name: String,
    pub current: Option<i64>,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub choices: Vec<i64>,
    /// The firmware accepts changes (through asusd or the helper).
    pub writable: bool,
    /// Another component owns it; show it read-only.
    pub owned_by: Option<Owner>,
}

/// Where the kernel's ASUS firmware attributes live.
pub const ARMOURY: &str = "/sys/class/firmware-attributes/asus-armoury/attributes";

impl FirmwareAttr {
    /// Build from an attribute's files; `file(name)` gives a file's value
    /// (if readable) and permission bits.
    fn from_files(name: &str, owned_by: Option<Owner>, file: impl Fn(&str) -> Option<(Option<String>, u32)>) -> Option<Self> {
        let (_, mode) = file("current_value")?;
        let num = |f: &str| file(f).and_then(|(v, _)| v).and_then(|v| v.trim().parse::<i64>().ok());
        let choices = file("possible_values").and_then(|(v, _)| v).map(|v| v.split(';').filter_map(|x| x.trim().parse().ok()).collect()).unwrap_or_default();
        Some(Self { name: name.to_string(), current: num("current_value"), min: num("min_value"), max: num("max_value"), choices, writable: mode & 0o200 != 0, owned_by })
    }

    /// Read an attribute as it is now. Ranges change with AC state, so
    /// applies use this rather than what detection saw.
    pub fn read_live(name: &str) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::path::Path::new(ARMOURY).join(name);
        Self::from_files(name, None, |f| {
            let path = dir.join(f);
            let meta = std::fs::metadata(&path).ok()?;
            Some((crate::sysfs::read_string(&path), meta.permissions().mode() & 0o777))
        })
    }

    /// The value to write for `wanted` (clamped to the range), or why it
    /// can't be written.
    pub fn accept(&self, wanted: i64) -> Result<i64, String> {
        if let Some(owner) = self.owned_by {
            return Err(format!("{} (managed by {owner:?})", self.name));
        }
        if !self.writable {
            return Err(format!("{} (read-only)", self.name));
        }
        if !self.choices.is_empty() {
            return if self.choices.contains(&wanted) { Ok(wanted) } else { Err(format!("{} (takes {:?}, not {wanted})", self.name, self.choices)) };
        }
        Ok(match (self.min, self.max) {
            (Some(lo), Some(hi)) if lo <= hi => wanted.clamp(lo, hi),
            _ => wanted,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PlatformControls {
    pub power_modes: Vec<String>,
    pub power_mode: Option<String>,
    pub power_owner: Option<Owner>,
    pub charge_limit: Option<u8>,
    pub attributes: Vec<FirmwareAttr>,
    pub gpu_modes: Vec<String>,
    pub gpu_mode: Option<String>,
    pub gpu_owner: Option<Owner>,
}

/// A knowledge fact the model used, and where it came from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Note {
    pub what: String,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HardwareModel {
    pub identity: Identity,
    pub platform: Platform,
    pub cpu: crate::cpu::CpuInfo,
    pub fans: Vec<FanOutput>,
    pub sensors: Vec<Sensor>,
    pub gpus: Vec<Gpu>,
    pub lighting: Vec<LightingDevice>,
    pub controls: PlatformControls,
    pub fan_owner: Owner,
    pub notes: Vec<Note>,
}

/// Who drives the fans: CoolerControl while its daemon runs (it drives every
/// fan it knows), else asusd where it stores the curves, else OmaAsus where
/// an output takes a duty, else the firmware.
fn fan_owner(fans: &[FanOutput], coolercontrold: bool) -> Owner {
    if coolercontrold {
        Owner::CoolerControl
    } else if fans.iter().any(|f| matches!(f.backend, FanBackend::AsusdCurve { .. })) {
        Owner::Asusd
    } else if fans.iter().any(|f| f.caps.duty) {
        Owner::OmaAsus
    } else {
        Owner::Firmware
    }
}

impl HardwareModel {
    /// [`Self::fan_owner`] with CoolerControl's daemon running or not: the
    /// field says what detection found, and the daemon starts and stops.
    pub fn fan_owner_now(&self, coolercontrold: bool) -> Owner {
        fan_owner(&self.fans, coolercontrold)
    }

    pub fn build(raw: &RawInventory, overrides: &Overrides) -> Self {
        let identity = Identity::from_dmi(&raw.system.dmi, overrides);
        let mut notes = Vec::new();
        let sensors = sensors(raw);
        let mut fans = fans(raw, &identity, overrides, &sensors, &mut notes);
        let gpus = gpus(raw);
        let lighting = lighting(raw, &identity, &mut notes);
        let controls = controls(raw);

        for f in &mut fans {
            if let Some(v) = overrides.min_duty.get(&f.id.0) {
                f.caps.min_duty = v.clamp(0.0, 100.0);
                notes.push(Note { what: format!("{}: minimum duty {v:.0} %", f.id), source: Source::User });
            }
        }
        let fan_owner = fan_owner(&fans, raw.system.daemons.coolercontrold);

        let mut model = Self { identity, platform: raw.system.platform.clone(), cpu: raw.system.cpu.clone(), fans, sensors, gpus, lighting, controls, fan_owner, notes };
        let hidden: BTreeSet<&str> = overrides.hide.iter().map(String::as_str).collect();
        if !hidden.is_empty() {
            model.fans.retain(|f| !hidden.contains(f.id.0.as_str()));
            model.sensors.retain(|s| !hidden.contains(s.id.0.as_str()));
            model.gpus.retain(|g| !hidden.contains(g.id.0.as_str()));
            model.lighting.retain(|l| !hidden.contains(l.id.0.as_str()));
        }
        model
    }

    pub fn fan(&self, id: &str) -> Option<&FanOutput> {
        self.fans.iter().find(|f| f.id.0 == id)
    }

    pub fn sensor(&self, id: &str) -> Option<&Sensor> {
        self.sensors.iter().find(|s| s.id.0 == id)
    }

    /// The first sensor with this role.
    pub fn sensor_for(&self, role: SensorRole) -> Option<&Sensor> {
        self.sensors.iter().find(|s| s.role == role)
    }

    pub fn attribute(&self, name: &str) -> Option<&FirmwareAttr> {
        self.controls.attributes.iter().find(|a| a.name == name)
    }
}

fn sensors(raw: &RawInventory) -> Vec<Sensor> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for d in &raw.system.hwmon {
        let igpu = raw.system.amd_gpus.iter().any(|g| g.is_integrated && g.device_path == d.device_path);
        let mut push = |label: &str, kind: SensorKind, role: SensorRole, input: &PathBuf| {
            let base = format!("hwmon:{}:{}", d.name, label);
            let (mut id, mut n) = (base.clone(), 2);
            while !seen.insert(id.clone()) {
                id = format!("{base}#{n}");
                n += 1;
            }
            out.push(Sensor { id: DeviceId(id), label: label.to_string(), role, kind, driver: d.name.clone(), input: input.clone() });
        };
        for t in d.temps.iter().filter(|t| !knowledge::sensor_hidden(&d.name, &t.label)) {
            push(&t.label, SensorKind::Temperature, knowledge::sensor_role(&d.name, &t.label, igpu), &t.input);
        }
        for p in &d.powers {
            push(&p.label, SensorKind::Power, knowledge::sensor_role(&d.name, &p.label, igpu), &p.input);
        }
        for f in &d.fans {
            push(&f.label, SensorKind::Fan, SensorRole::FanSpeed, &f.input);
        }
    }
    out
}

fn tach(sensors: &[Sensor], driver: &str, label: &str) -> Option<DeviceId> {
    sensors.iter().find(|s| s.kind == SensorKind::Fan && s.driver == driver && s.label == label).map(|s| s.id.clone())
}

fn fans(raw: &RawInventory, id: &Identity, o: &Overrides, sensors: &[Sensor], notes: &mut Vec<Note>) -> Vec<FanOutput> {
    let mut out = Vec::new();
    // Fans whose firmware curves asusd stores and applies per platform profile.
    let asusd_fans: Vec<&str> = raw.asusd.as_ref().filter(|a| a.objects.has_fan_curves).and_then(|a| a.fan_curves.values().next()).map(|curves| curves.iter().map(|c| c.fan.as_str()).collect()).unwrap_or_default();

    for d in &raw.system.hwmon {
        for p in &d.pwms {
            if p.has_duty {
                out.push(pwm_output(d, p, id, sensors, notes));
            } else if let (Some(curve), Some(fan)) = (&p.auto_curve, knowledge::firmware_fan_name(&d.name, p.index)) {
                // A fan only its firmware drives: through asusd when it manages
                // the curves, else straight to sysfs.
                let tach = knowledge::curve_tach_driver(&d.name)
                    .and_then(|t| raw.system.hwmon.iter().find(|x| x.name == t))
                    .and_then(|t| t.fans.iter().find(|f| f.index == p.index).and_then(|f| tach(sensors, &t.name, &f.label)));
                let (fan_id, backend) = if asusd_fans.contains(&fan) {
                    (format!("asusd:fan:{fan}"), FanBackend::AsusdCurve { fan: fan.to_string() })
                } else {
                    (format!("asuscurve:pwm{}", p.index), FanBackend::AsusCurve { dir: d.path.clone(), index: p.index, driver: d.name.clone() })
                };
                let max_rpm = knowledge::fan_max_rpm(id, fan, o);
                if let Some((rpm, source)) = max_rpm {
                    notes.push(Note { what: format!("{fan_id}: maximum speed {rpm} rpm"), source });
                }
                let label = match fan {
                    "MID" => "Mid fan".to_string(),
                    f => format!("{f} fan"),
                };
                // What asusd stores for this fan per power mode, to start editing from.
                let firmware_curves: BTreeMap<String, StoredCurve> = raw
                    .asusd
                    .iter()
                    .flat_map(|a| a.fan_curves.iter())
                    .filter_map(|(mode, curves)| curves.iter().find(|c| c.fan == fan).map(|c| (PlatformProfile::from_u32(*mode).label().to_string(), StoredCurve { enabled: c.enabled, points: c.points() })))
                    .collect();
                out.push(FanOutput {
                    id: DeviceId(fan_id),
                    label,
                    tach,
                    caps: FanCaps {
                        duty: false,
                        firmware_curve: Some(CurveSpec { points: curve.points, temp: CurveTemp::Firmware }),
                        min_duty: 0.0,
                        release: Release::Auto,
                        max_rpm: max_rpm.map(|m| m.0),
                        curve_input: if fan == "GPU" { CurveInput::Gpu } else { CurveInput::Cpu },
                    },
                    backend,
                    firmware_curves,
                });
            }
        }
    }

    for g in raw.nvidia.iter().filter(|g| g.num_fans > 0) {
        out.push(FanOutput {
            id: DeviceId(format!("nvidia:{}:fans", g.index)),
            label: format!("{} fans", g.name),
            tach: None,
            caps: FanCaps { duty: true, firmware_curve: None, min_duty: 0.0, release: Release::Auto, max_rpm: None, curve_input: CurveInput::Gpu },
            backend: FanBackend::Nvidia { gpu: g.index, fans: g.num_fans },
            firmware_curves: BTreeMap::new(),
        });
    }

    let mut hub_paths = BTreeSet::new();
    let hubs: Vec<(&detect::HidDevice, HubKind)> =
        raw.system.hid.iter().filter(|h| h.vendor_id == detect::pid::ENE && hub_paths.insert(h.path.clone())).filter_map(|h| HubKind::from_pid(h.product_id).map(|k| (h, k))).collect();
    for (n, (hub, kind)) in hubs.iter().enumerate() {
        for c in 1..=knowledge::lianli_channels(kind) {
            out.push(FanOutput {
                id: DeviceId(format!("lianli:{n}:ch{c}")),
                label: if hubs.len() > 1 { format!("Lian Li hub {} channel {c}", n + 1) } else { format!("Lian Li channel {c}") },
                tach: None,
                caps: FanCaps { duty: true, firmware_curve: None, min_duty: 0.0, release: Release::Auto, max_rpm: None, curve_input: CurveInput::CpuOrGpu },
                backend: FanBackend::LianLi { path: hub.path.clone(), channel: c },
                firmware_curves: BTreeMap::new(),
            });
        }
    }
    out
}

/// A PWM output that takes a duty: Super I/O header, AIO, or anything else hwmon exposes.
fn pwm_output(d: &HwmonDevice, p: &crate::hwmon::PwmChannel, id: &Identity, sensors: &[Sensor], notes: &mut Vec<Note>) -> FanOutput {
    let quirk = knowledge::output_quirk(&d.name, p.index);
    let (out_id, label) = if let Some(q) = quirk {
        (q.id.to_string(), q.label.to_string())
    } else if d.is_super_io() {
        let header = knowledge::fan_header(id, p.index);
        if let Some((name, source)) = header {
            notes.push(Note { what: format!("superio:pwm{}: header {name}", p.index), source });
        }
        (format!("superio:pwm{}", p.index), header.map(|(h, _)| format!("{h} (pwm{})", p.index)).unwrap_or_else(|| format!("Fan header {}", p.index)))
    } else {
        (format!("hwmon:{}:pwm{}", d.name, p.index), format!("{} pwm{}", d.friendly_name(), p.index))
    };
    if let Some(q) = quirk {
        notes.push(Note { what: format!("{out_id}: minimum {:.0} %, released to {:?}", q.min_duty, q.release), source: q.source });
    }
    // Only Super I/O curves whose mode is known are programmed by OmaAsus (Smart Fan IV).
    let firmware_curve = p.auto_curve.as_ref().filter(|_| d.is_super_io() && knowledge::smart_fan_mode(&d.name).is_some()).map(|c| CurveSpec { points: c.points, temp: if c.has_temp_sel { CurveTemp::Selectable } else { CurveTemp::Firmware } });
    let release = quirk.map(|q| q.release).unwrap_or(if p.has_enable {
        Release::RestoreMode
    } else {
        // No automatic mode to return to: full speed is the only state known to be safe.
        Release::SafeFixed(100.0)
    });
    FanOutput {
        id: DeviceId(out_id),
        label,
        tach: d.fans.iter().find(|f| f.index == p.index).and_then(|f| tach(sensors, &d.name, &f.label)),
        caps: FanCaps { duty: true, firmware_curve, min_duty: quirk.map(|q| q.min_duty).unwrap_or(0.0), release, max_rpm: None, curve_input: quirk.map(|q| q.curve_input).unwrap_or(CurveInput::CpuOrGpu) },
        backend: FanBackend::Hwmon { dir: d.path.clone(), index: p.index },
        firmware_curves: BTreeMap::new(),
    }
}

fn pci_power(p: &PciDevice) -> GpuPower {
    match p.runtime_status.as_deref() {
        Some("suspended") => GpuPower::Suspended,
        _ => GpuPower::Active,
    }
}

fn gpus(raw: &RawInventory) -> Vec<Gpu> {
    let mut out: Vec<Gpu> = raw
        .pci_display
        .iter()
        .map(|p| {
            let vendor = match p.vendor.as_str() {
                "0x1002" => GpuVendor::Amd,
                "0x10de" => GpuVendor::Nvidia,
                "0x8086" => GpuVendor::Intel,
                _ => GpuVendor::Other,
            };
            let amd = raw.system.amd_gpus.iter().find(|g| g.pci_slot == p.slot);
            let bus = p.slot.split_once(':').map(|(_, rest)| rest.to_ascii_lowercase()).unwrap_or_default();
            let nvidia = raw.nvidia.iter().find(|n| n.pci_bus_id.to_ascii_lowercase().ends_with(&bus));
            let name = amd.map(|g| g.name.clone()).or_else(|| nvidia.map(|n| n.name.clone())).unwrap_or_else(|| format!("{vendor:?} GPU"));
            Gpu { id: DeviceId(format!("gpu:{}", p.slot)), vendor, name, integrated: amd.map(|g| g.is_integrated).unwrap_or(vendor == GpuVendor::Intel), power: pci_power(p), pci_slot: Some(p.slot.clone()) }
        })
        .collect();
    // A dGPU that supergfxd powered off is gone from the bus but still there.
    if let Some(gfx) = &raw.supergfx
        && gfx.vendor.eq_ignore_ascii_case("nvidia") && !out.iter().any(|g| g.vendor == GpuVendor::Nvidia)
    {
        out.push(Gpu { id: DeviceId::new("gpu:nvidia"), vendor: GpuVendor::Nvidia, name: "NVIDIA GPU".into(), integrated: false, power: GpuPower::Off, pci_slot: None });
    }
    out
}

fn lighting(raw: &RawInventory, id: &Identity, notes: &mut Vec<Note>) -> Vec<LightingDevice> {
    let Some(a) = &raw.asusd else { return Vec::new() };
    let mut out: Vec<LightingDevice> = a
        .aura
        .iter()
        .map(|aura| LightingDevice {
            id: DeviceId(format!("asusd:aura:{}", aura.path.rsplit('/').next().unwrap_or_default())),
            label: knowledge::aura_device_label(aura.device_type).to_string(),
            backend: LightingBackend::AsusdAura { path: aura.path.clone() },
            modes: aura.supported_basic_modes.clone(),
            brightness_levels: aura.supported_brightness.clone(),
            leds: None,
        })
        .collect();
    if let Some(s) = &a.slash {
        let leds = knowledge::slash_leds(id);
        if let Some((n, source)) = leds {
            notes.push(Note { what: format!("asusd:slash: {n} LEDs"), source });
        }
        // Brightness is continuous (0-255), so no levels; animations from knowledge.
        let modes = knowledge::SLASH_MODES.iter().map(|(m, _)| *m).collect();
        out.push(LightingDevice { id: DeviceId::new("asusd:slash"), label: "Slash".into(), backend: LightingBackend::AsusdSlash { path: s.path.clone() }, modes, brightness_levels: Vec::new(), leds: leds.map(|l| l.0) });
    }
    out
}

fn controls(raw: &RawInventory) -> PlatformControls {
    let mut c = PlatformControls::default();
    let sysfs_value = |path: &str| raw.sysfs.get(path).and_then(|a| a.value.clone());

    // Power mode: asusd owns the platform profile when it runs, then
    // power-profiles-daemon, then the kernel's sysfs switch.
    if let Some(p) = raw.asusd.as_ref().and_then(|a| a.platform.as_ref()).filter(|p| !p.choices.is_empty()) {
        c.power_modes = p.choices.iter().map(|v| PlatformProfile::from_u32(*v).label().to_string()).collect();
        c.power_mode = p.profile.map(|v| PlatformProfile::from_u32(v).label().to_string());
        c.power_owner = Some(Owner::Asusd);
        c.charge_limit = p.charge_limit;
    } else if let Some(ppd) = raw.ppd.as_ref().filter(|p| !p.profiles.is_empty()) {
        c.power_modes = ppd.profiles.iter().map(|x| x.name.clone()).collect();
        c.power_mode = Some(ppd.active.clone());
        c.power_owner = Some(Owner::PowerProfilesDaemon);
    } else if !raw.system.platform_profile_choices.is_empty() {
        c.power_modes = raw.system.platform_profile_choices.clone();
        c.power_mode = sysfs_value("/sys/firmware/acpi/platform_profile");
        c.power_owner = Some(Owner::Sysfs);
    }
    if c.charge_limit.is_none() {
        c.charge_limit = raw.sysfs.iter().find(|(p, _)| p.starts_with("/sys/class/power_supply/") && p.ends_with("/charge_control_end_threshold")).and_then(|(_, a)| a.value.as_deref()?.parse().ok());
    }

    let gpu_owner = raw.supergfx.as_ref().map(|_| Owner::Supergfxd);
    for name in &raw.system.asus_armoury_attrs {
        let owned_by = if knowledge::is_gpu_switch(name) { gpu_owner } else { None };
        let file = |f: &str| raw.sysfs.get(&format!("{ARMOURY}/{name}/{f}")).map(|a| (a.value.clone(), a.mode));
        c.attributes.extend(FirmwareAttr::from_files(name, owned_by, file));
    }

    if let Some(g) = &raw.supergfx {
        c.gpu_modes = g.supported.iter().map(|m| m.label().to_string()).collect();
        c.gpu_mode = Some(g.mode.label().to_string());
        c.gpu_owner = Some(Owner::Supergfxd);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Assignment {
        target: DeviceId,
    }

    fn read(toml_text: &str) -> String {
        toml::from_str::<Assignment>(toml_text).expect(toml_text).target.0
    }

    #[test]
    fn firmware_attributes_accept_only_what_they_can_take() {
        let ppt = FirmwareAttr { name: "ppt_pl1_spl".into(), current: Some(35), min: Some(15), max: Some(35), choices: vec![], writable: true, owned_by: None };
        assert_eq!(ppt.accept(80), Ok(35), "clamped to the range in force (battery here)");
        assert_eq!(ppt.accept(20), Ok(20));
        let od = FirmwareAttr { name: "panel_overdrive".into(), current: Some(1), min: None, max: None, choices: vec![0, 1], writable: true, owned_by: None };
        assert_eq!(od.accept(0), Ok(0));
        assert!(od.accept(2).is_err());
        let ro = FirmwareAttr { writable: false, ..od.clone() };
        assert!(ro.accept(0).is_err());
        let mux = FirmwareAttr { name: "gpu_mux_mode".into(), owned_by: Some(Owner::Supergfxd), ..od };
        assert!(mux.accept(0).unwrap_err().contains("Supergfxd"));
    }

    #[test]
    fn v1_fan_names_read_as_model_ids() {
        assert_eq!(read(r#"target = "asusd:fan:CPU""#), "asusd:fan:CPU");
        assert_eq!(read(r#"target = "RyujinPump""#), "ryujin:pump");
        assert_eq!(read(r#"target = "RyujinExternalFans""#), "ryujin:radiator");
        assert_eq!(read(r#"target = "NvidiaFans""#), "nvidia:0:fans");
        assert_eq!(read("[target]\nSuperIo = 3"), "superio:pwm3");
        assert_eq!(read("[target]\nLianLiChannel = 2"), "lianli:0:ch2");
        assert_eq!(read("[target.CoolerControl]\ndevice_uid = \"abc\"\nchannel = \"fan1\""), "cc:abc:fan1");
    }
}
