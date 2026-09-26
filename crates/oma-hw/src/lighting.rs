//! Showing a profile's lighting on the devices the model found: asusd's Aura
//! keyboard and Slash bar, and OpenRGB devices by name.

use crate::asusd::{AuraEffect, AuraProxy, SlashProxy};
use crate::knowledge;
use crate::model::{LightingBackend, LightingDevice};
use crate::profile::{LightingMode, Rgb};
use crate::rgb::RgbDevice;
use serde::Serialize;

/// What an Aura device should do for a profile's lighting.
#[derive(Debug, Clone, PartialEq)]
pub enum AuraAction {
    /// Show this effect (and the profile's brightness).
    Effect(AuraEffect),
    /// Brightness to zero.
    Off,
}

/// What a device is showing now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum LightState {
    /// `listed` is the mode as the device lists it; `level` one of its brightness levels.
    Aura { listed: u32, colour: Rgb, level: u32 },
    Slash { enabled: bool, mode: u8, brightness: u8, options: Vec<(SlashOption, bool)> },
}

/// When the Slash bar shows (device settings, not part of a profile).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SlashOption {
    Boot,
    Shutdown,
    Sleep,
    Battery,
    LidClosed,
    BatteryWarning,
}

impl SlashOption {
    pub const ALL: [SlashOption; 6] = [Self::Boot, Self::Shutdown, Self::Sleep, Self::Battery, Self::LidClosed, Self::BatteryWarning];

    pub fn label(self) -> &'static str {
        match self {
            Self::Boot => "On boot",
            Self::Shutdown => "On shutdown",
            Self::Sleep => "While asleep",
            Self::Battery => "On battery",
            Self::LidClosed => "Lid closed",
            Self::BatteryWarning => "Low-battery warning",
        }
    }

    async fn get(self, p: &SlashProxy<'_>) -> zbus::Result<bool> {
        match self {
            Self::Boot => p.show_on_boot().await,
            Self::Shutdown => p.show_on_shutdown().await,
            Self::Sleep => p.show_on_sleep().await,
            Self::Battery => p.show_on_battery().await,
            Self::LidClosed => p.show_on_lid_closed().await,
            Self::BatteryWarning => p.show_battery_warning().await,
        }
    }

    async fn set(self, p: &SlashProxy<'_>, v: bool) -> zbus::Result<()> {
        match self {
            Self::Boot => p.set_show_on_boot(v).await,
            Self::Shutdown => p.set_show_on_shutdown(v).await,
            Self::Sleep => p.set_show_on_sleep(v).await,
            Self::Battery => p.set_show_on_battery(v).await,
            Self::LidClosed => p.set_show_on_lid_closed(v).await,
            Self::BatteryWarning => p.set_show_battery_warning(v).await,
        }
    }
}

/// An Aura effect in a mode the device lists. `LedModeData` takes the listed
/// number (Pulse is 10); only `AllModeData` numbers modes by position.
fn effect(listed: u32, c1: Rgb, c2: Rgb) -> AuraEffect {
    AuraEffect { mode: listed, zone: 0, colour1: (c1.r, c1.g, c1.b), colour2: (c2.r, c2.g, c2.b), speed: "Med".into(), direction: "Right".into() }
}

/// Map a lighting mode onto the firmware modes this Aura device lists.
/// Effects with no colour of their own (Pulse, Comet…) use `accent`.
pub fn aura_plan(device: &LightingDevice, mode: &LightingMode, accent: Rgb) -> Result<AuraAction, String> {
    let has = |listed: u32| device.modes.contains(&listed);
    let black = Rgb::new(0, 0, 0);
    match mode {
        LightingMode::Off => Ok(AuraAction::Off),
        LightingMode::Static(c) if has(0) => Ok(AuraAction::Effect(effect(0, *c, black))),
        LightingMode::Breathing(c) if has(1) => Ok(AuraAction::Effect(effect(1, *c, black))),
        LightingMode::Rainbow if has(2) || has(3) => Ok(AuraAction::Effect(effect(if has(2) { 2 } else { 3 }, black, black))),
        LightingMode::Firmware(m) if has(*m) => Ok(AuraAction::Effect(effect(*m, accent, black))),
        other => Err(format!("{} can't show {}", device.label, describe_on(device, other))),
    }
}

/// The device's brightness level for a profile brightness in percent.
pub fn aura_level(levels: &[u32], percent: u8) -> Option<u32> {
    let mut levels = levels.to_vec();
    levels.sort_unstable();
    let top = levels.len().checked_sub(1)?;
    let i = (f64::from(percent.min(100)) / 100.0 * top as f64).round() as usize;
    levels.get(i).copied()
}

/// The percent that selects `level` (the inverse of [`aura_level`]).
pub fn aura_percent(levels: &[u32], level: u32) -> u8 {
    let mut levels = levels.to_vec();
    levels.sort_unstable();
    match (levels.iter().position(|l| *l == level), levels.len().checked_sub(1)) {
        (Some(i), Some(top)) if top > 0 => (i as f64 / top as f64 * 100.0).round() as u8,
        _ => 100,
    }
}

/// Names for brightness levels: Off/Low/Med/High when there are four, else numbers.
pub fn level_names(levels: &[u32]) -> Vec<String> {
    const NAMES: [&str; 4] = ["Off", "Low", "Med", "High"];
    let mut sorted = levels.to_vec();
    sorted.sort_unstable();
    levels
        .iter()
        .map(|l| match sorted.iter().position(|s| s == l) {
            Some(i) if sorted.len() == NAMES.len() => NAMES[i].to_string(),
            _ => l.to_string(),
        })
        .collect()
}

/// Name of a device's own mode or animation.
pub fn mode_name(device: &LightingDevice, m: u32) -> String {
    match device.backend {
        LightingBackend::AsusdAura { .. } => knowledge::aura_mode_name(m).to_string(),
        LightingBackend::AsusdSlash { .. } => knowledge::slash_mode_name(m).map(str::to_string).unwrap_or_else(|| format!("Animation {m}")),
    }
}

/// A short description for reports: "static #82FB9C", "effect 16".
pub fn describe(mode: &LightingMode) -> String {
    match mode {
        LightingMode::Off => "off".into(),
        LightingMode::Static(c) => format!("static {}", c.hex()),
        LightingMode::Breathing(c) => format!("breathing {}", c.hex()),
        LightingMode::Rainbow => "rainbow".into(),
        LightingMode::Thermal { .. } => "thermal glow".into(),
        LightingMode::Direct(_) => "per-LED colours".into(),
        LightingMode::Firmware(m) => format!("effect {m}"),
    }
}

/// [`describe`], naming the device's own effects: "Bounce", "Pulse".
pub fn describe_on(device: &LightingDevice, mode: &LightingMode) -> String {
    match (mode, &device.backend) {
        (LightingMode::Firmware(m), _) => mode_name(device, *m),
        (LightingMode::Static(_), LightingBackend::AsusdSlash { .. }) => "Static".into(),
        _ => describe(mode),
    }
}

/// What a device shows when lit: its effect, whatever its brightness.
pub fn lit_mode(state: &LightState) -> LightingMode {
    match *state {
        LightState::Aura { listed: 0, colour, .. } => LightingMode::Static(colour),
        LightState::Aura { listed: 1, colour, .. } => LightingMode::Breathing(colour),
        LightState::Aura { listed, .. } => LightingMode::Firmware(listed),
        LightState::Slash { mode, .. } => LightingMode::Firmware(u32::from(mode)),
    }
}

/// What a device shows now, as the profile lighting that would show it.
pub fn mode_of(state: &LightState) -> LightingMode {
    match state {
        LightState::Aura { level: 0, .. } | LightState::Slash { enabled: false, .. } => LightingMode::Off,
        lit => lit_mode(lit),
    }
}

/// Slash brightness byte for a percent; never 0, which asusd swaps for its own default.
fn slash_byte(percent: u8) -> u8 {
    ((f64::from(percent.min(100)) * 2.55).round() as u8).max(1)
}

/// Brightness of a state in percent.
pub fn percent_of(device: &LightingDevice, state: &LightState) -> u8 {
    match *state {
        LightState::Aura { level, .. } => aura_percent(&device.brightness_levels, level),
        LightState::Slash { brightness, .. } => (f64::from(brightness) / 2.55).round() as u8,
    }
}

async fn aura(conn: &zbus::Connection, path: &str) -> Result<AuraProxy<'static>, String> {
    let e = |e: zbus::Error| e.to_string();
    AuraProxy::builder(conn).path(path.to_string()).map_err(e)?.cache_properties(zbus::proxy::CacheProperties::No).build().await.map_err(e)
}

async fn slash(conn: &zbus::Connection, path: &str) -> Result<SlashProxy<'static>, String> {
    let e = |e: zbus::Error| e.to_string();
    SlashProxy::builder(conn).path(path.to_string()).map_err(e)?.cache_properties(zbus::proxy::CacheProperties::No).build().await.map_err(e)
}

/// Read what a device the model found is showing.
pub async fn read(conn: &zbus::Connection, device: &LightingDevice) -> Result<LightState, String> {
    let e = |e: zbus::Error| e.to_string();
    match &device.backend {
        LightingBackend::AsusdAura { path } => {
            let p = aura(conn, path).await?;
            let data = p.led_mode_data().await.map_err(e)?;
            let (r, g, b) = data.colour1;
            Ok(LightState::Aura { listed: data.mode, colour: Rgb::new(r, g, b), level: p.brightness().await.map_err(e)? })
        }
        LightingBackend::AsusdSlash { path } => {
            let p = slash(conn, path).await?;
            let mut options = Vec::new();
            for o in SlashOption::ALL {
                if let Ok(v) = o.get(&p).await {
                    options.push((o, v));
                }
            }
            Ok(LightState::Slash { enabled: p.enabled().await.map_err(e)?, mode: p.mode().await.map_err(e)?, brightness: p.brightness().await.map_err(e)?, options })
        }
    }
}

/// Read every device the model found; devices that don't answer are left out.
pub async fn read_all(devices: &[LightingDevice]) -> Vec<(String, LightState)> {
    let Ok(conn) = zbus::Connection::system().await else { return Vec::new() };
    let mut out = Vec::new();
    for d in devices {
        match read(&conn, d).await {
            Ok(s) => out.push((d.id.to_string(), s)),
            Err(e) => tracing::debug!(device = %d.id, error = %e, "lighting state unreadable"),
        }
    }
    out
}

/// Show `mode` on a device the model found. `brightness` in percent, `None`
/// to leave it; `accent` colours effects that have no colour of their own.
pub async fn apply(conn: &zbus::Connection, device: &LightingDevice, mode: &LightingMode, brightness: Option<u8>, accent: Rgb) -> Result<(), String> {
    let e = |e: zbus::Error| e.to_string();
    match &device.backend {
        LightingBackend::AsusdAura { path } => {
            let p = aura(conn, path).await?;
            match aura_plan(device, mode, accent)? {
                AuraAction::Off => p.set_brightness(0).await.map_err(e),
                AuraAction::Effect(effect) => {
                    p.set_led_mode_data(effect).await.map_err(e)?;
                    // An effect on a dark keyboard should be seen: brightest level unless the profile says.
                    let level = match brightness.and_then(|pct| aura_level(&device.brightness_levels, pct)).filter(|l| *l > 0) {
                        Some(l) => Some(l),
                        None if p.brightness().await.ok() == Some(0) => device.brightness_levels.iter().max().copied(),
                        None => None,
                    };
                    match level {
                        Some(l) => p.set_brightness(l).await.map_err(e),
                        None => Ok(()),
                    }
                }
            }
        }
        LightingBackend::AsusdSlash { path } => {
            let p = slash(conn, path).await?;
            let animation = match mode {
                LightingMode::Off => return p.set_enabled(false).await.map_err(e),
                LightingMode::Firmware(m) => *m,
                LightingMode::Static(_) => knowledge::SLASH_STATIC,
                other => return Err(format!("{} can't show {}", device.label, describe(other))),
            };
            let animation = u8::try_from(animation).ok().filter(|a| device.modes.contains(&u32::from(*a))).ok_or_else(|| format!("{} has no animation {animation}", device.label))?;
            p.set_mode(animation).await.map_err(e)?;
            if let Some(pct) = brightness {
                p.set_brightness(slash_byte(pct)).await.map_err(e)?;
            }
            p.set_enabled(true).await.map_err(e)
        }
    }
}

/// Set a device's brightness in percent, leaving what it shows.
pub async fn set_brightness(conn: &zbus::Connection, device: &LightingDevice, percent: u8) -> Result<(), String> {
    let e = |e: zbus::Error| e.to_string();
    match &device.backend {
        LightingBackend::AsusdAura { path } => {
            let level = aura_level(&device.brightness_levels, percent).ok_or_else(|| format!("{} has no brightness levels", device.label))?;
            aura(conn, path).await?.set_brightness(level).await.map_err(e)
        }
        LightingBackend::AsusdSlash { path } => slash(conn, path).await?.set_brightness(slash_byte(percent)).await.map_err(e),
    }
}

/// Set a Slash device option.
pub async fn set_slash_option(conn: &zbus::Connection, device: &LightingDevice, option: SlashOption, on: bool) -> Result<(), String> {
    match &device.backend {
        LightingBackend::AsusdSlash { path } => option.set(&slash(conn, path).await?, on).await.map_err(|e| e.to_string()),
        _ => Err(format!("{} isn't a Slash bar", device.label)),
    }
}

/// Show `mode` on every OpenRGB device called `name`. Profiles key lighting
/// by name, and identical parts share one (a kit of DIMMs lists each stick
/// under the same name), so all of them take it, not only the first listed.
pub async fn apply_openrgb(devices: &[RgbDevice], name: &str, mode: &LightingMode) -> Result<(), String> {
    let matching: Vec<&RgbDevice> = devices.iter().filter(|d| d.name == name).collect();
    if matching.is_empty() {
        return Err(format!("{name} (not connected)"));
    }
    let mut failed = Vec::new();
    for d in matching {
        if let Err(e) = apply_openrgb_one(d, name, mode).await {
            failed.push(e);
        }
    }
    failed.dedup();
    if failed.is_empty() { Ok(()) } else { Err(failed.join("; ")) }
}

async fn apply_openrgb_one(d: &RgbDevice, name: &str, mode: &LightingMode) -> Result<(), String> {
    let named = |words: &[&str]| d.modes.iter().find(|m| words.iter().any(|w| m.name.to_ascii_lowercase().contains(w))).map(|m| m.index);
    let r = match mode {
        LightingMode::Off => crate::rgb::turn_off(d.index).await,
        LightingMode::Static(c) => crate::rgb::set_static(d.index, (c.r, c.g, c.b), true).await,
        LightingMode::Rainbow => match named(&["rainbow", "spectrum"]) {
            Some(m) => crate::rgb::set_mode(d.index, m, true).await,
            None => return Err(format!("{name} has no rainbow effect")),
        },
        LightingMode::Breathing(_) => match named(&["breath"]) {
            Some(m) => crate::rgb::set_mode(d.index, m, true).await,
            None => return Err(format!("{name} has no breathing effect")),
        },
        LightingMode::Firmware(m) => crate::rgb::set_mode(d.index, *m as usize, true).await,
        LightingMode::Direct(colours) => crate::rgb::set_leds(d.index, &colours.iter().map(|c| (c.r, c.g, c.b)).collect::<Vec<_>>()).await,
        LightingMode::Thermal { .. } => return Err("thermal glow runs from the Lighting page".into()),
    };
    r.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeviceId;

    /// The GA403WR's keyboard as asusd 6.4 reports it.
    fn keyboard() -> LightingDevice {
        LightingDevice { id: DeviceId::new("asusd:aura:19b6_3_4"), label: "Keyboard".into(), backend: LightingBackend::AsusdAura { path: "/xyz/ljones/aura/19b6_3_4".into() }, modes: vec![0, 1, 2, 3, 10], brightness_levels: vec![0, 1, 2, 3], leds: None }
    }

    #[test]
    fn lighting_maps_onto_the_modes_a_keyboard_lists() {
        let kb = keyboard();
        let green = Rgb::new(130, 251, 156);
        let accent = Rgb::new(255, 61, 104);
        match aura_plan(&kb, &LightingMode::Static(green), accent) {
            Ok(AuraAction::Effect(e)) => assert_eq!((e.mode, e.colour1), (0, (130, 251, 156))),
            other => panic!("{other:?}"),
        }
        match aura_plan(&kb, &LightingMode::Rainbow, accent) {
            Ok(AuraAction::Effect(e)) => assert_eq!(e.mode, 2),
            other => panic!("{other:?}"),
        }
        // Pulse is written as listed (10; asusd refuses 9 as "incorrect type"), in the accent.
        match aura_plan(&kb, &LightingMode::Firmware(10), accent) {
            Ok(AuraAction::Effect(e)) => assert_eq!((e.mode, e.colour1), (10, (255, 61, 104))),
            other => panic!("{other:?}"),
        }
        assert_eq!(describe_on(&kb, &LightingMode::Firmware(10)), "Pulse");
        assert!(aura_plan(&kb, &LightingMode::Firmware(4), accent).is_err(), "Star isn't listed");
        assert_eq!(aura_plan(&kb, &LightingMode::Off, accent), Ok(AuraAction::Off));
    }

    #[test]
    fn brightness_percent_picks_a_level() {
        assert_eq!(aura_level(&[0, 1, 2, 3], 100), Some(3));
        assert_eq!(aura_level(&[0, 1, 2, 3], 50), Some(2));
        assert_eq!(aura_level(&[0, 1, 2, 3], 0), Some(0));
        assert_eq!(aura_level(&[], 50), None);
        for level in 0..4 {
            assert_eq!(aura_level(&[0, 1, 2, 3], aura_percent(&[0, 1, 2, 3], level)), Some(level), "round trip");
        }
        assert_eq!(level_names(&[0, 1, 2, 3]), ["Off", "Low", "Med", "High"]);
        assert_eq!(level_names(&[0, 1, 2]), ["0", "1", "2"]);
    }

    #[test]
    fn a_devices_state_reads_back_as_profile_lighting() {
        let blue = Rgb::new(46, 138, 230);
        assert_eq!(mode_of(&LightState::Aura { listed: 0, colour: blue, level: 3 }), LightingMode::Static(blue));
        assert_eq!(mode_of(&LightState::Aura { listed: 10, colour: blue, level: 3 }), LightingMode::Firmware(10));
        assert_eq!(mode_of(&LightState::Aura { listed: 0, colour: blue, level: 0 }), LightingMode::Off);
        assert_eq!(mode_of(&LightState::Slash { enabled: true, mode: 16, brightness: 255, options: Vec::new() }), LightingMode::Firmware(16));
        assert_eq!(mode_of(&LightState::Slash { enabled: false, mode: 16, brightness: 255, options: Vec::new() }), LightingMode::Off);
    }
}
