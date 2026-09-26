//! Profile application: an ordered pipeline over what the machine has.
//!
//! Power mode first, because firmware applies the mode's own limits and fan
//! curves when it changes; then firmware limits once the new mode has settled;
//! then CPU and GPU. Fans follow through the fan engine. A newer apply
//! supersedes one still running, and every step reports whether it applied,
//! was skipped (and why) or failed.

use oma_hw::asusd::{self, PlatformProfile, PlatformProxy};
use oma_hw::helper::Controller;
use oma_hw::model::{FirmwareAttr, GpuVendor, HardwareModel, Owner, ARMOURY};
use oma_hw::profile::{match_power_mode, Profile};
use oma_hw::SystemInventory;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Why a profile is being applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Manual,
    Automation,
    /// Again after resume, a charger change or a graphics switch.
    Reapply,
    /// The power mode was switched outside a profile (a keyboard shortcut,
    /// `powerprofilesctl`, a bar widget): the profile that carries it follows.
    PowerMode,
}

pub struct Context {
    pub inv: Option<Arc<SystemInventory>>,
    pub model: Option<Arc<HardwareModel>>,
    /// The latest apply's number; this one stops when it's no longer the latest.
    pub generation: Arc<AtomicU64>,
    pub this: u64,
}

impl Context {
    fn superseded(&self) -> bool {
        self.generation.load(Ordering::SeqCst) != self.this
    }
}

/// What an apply did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<String>,
    pub superseded: bool,
    /// NVIDIA settings were skipped only until the dGPU is ready.
    pub nvidia_deferred: bool,
}

impl Report {
    /// One line for a toast: `Ok` unless something failed.
    pub fn summary(&self, profile: &str) -> Result<String, String> {
        let skipped = if self.skipped.is_empty() { String::new() } else { format!(" · skipped {}", self.skipped.join(", ")) };
        match (self.applied.is_empty(), self.failed.is_empty()) {
            (true, true) => Ok(format!("{profile}: already in place{skipped}")),
            (false, true) => Ok(format!("{profile} applied: {}{skipped}", self.applied.join(", "))),
            (true, false) => Err(format!("{profile} failed: {}{skipped}", self.failed.join("; "))),
            (false, false) => Err(format!("{profile} partly applied ({}); failed: {}{skipped}", self.applied.join(", "), self.failed.join("; "))),
        }
    }
}

pub async fn apply_profile(p: Profile, cx: Context) -> Report {
    let ctl = Controller::connect().await;
    let mut r = Report::default();
    macro_rules! checkpoint {
        () => {
            if cx.superseded() {
                r.superseded = true;
                return r;
            }
        };
    }
    checkpoint!();

    // Who owns power modes, and so firmware limits, where asusd keeps its own
    // copy: the model's answer. It can still be building in the first seconds
    // after launch; a profile applied then asks the daemons directly, in the
    // same order, rather than drop its power mode or go behind asusd's back.
    let (owner, modes) = match &cx.model {
        Some(m) => (m.controls.power_owner, m.controls.power_modes.clone()),
        None if p.cpu.power_mode.is_some() || !p.asusd.is_empty() => fallback_power_modes().await,
        None => (None, Vec::new()),
    };

    // 1. Power mode, through whichever component owns it here.
    let mut mode_changed = false;
    if let Some(wanted) = &p.cpu.power_mode {
        match match_power_mode(wanted, &modes) {
            None if modes.is_empty() => r.skipped.push(format!("power mode {wanted} (nothing controls power modes here)")),
            None => r.skipped.push(format!("power mode {wanted} (not on this machine)")),
            Some(mode) => match set_power_mode(owner, mode, &ctl).await {
                Ok(true) => {
                    mode_changed = true;
                    r.applied.push(format!("power mode {mode}"));
                }
                Ok(false) => {}
                Err(e) => r.failed.push(format!("power mode: {e}")),
            },
        }
    }

    // 2. Firmware limits, once the firmware has put the new mode's own in place.
    let limits: Vec<(&String, i64)> = p.asusd.iter().filter_map(|(name, v)| v.trim().parse().ok().map(|n| (name, n))).collect();
    if !limits.is_empty() {
        if mode_changed {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            checkpoint!();
        }
        let via_asusd = owner == Some(Owner::Asusd);
        for (name, wanted) in limits {
            let Some(mut attr) = FirmwareAttr::read_live(name) else {
                r.skipped.push(format!("{name} (not on this machine)"));
                continue;
            };
            attr.owned_by = cx.model.as_ref().and_then(|m| m.attribute(name)).and_then(|a| a.owned_by);
            match attr.accept(wanted) {
                Err(why) => r.skipped.push(why),
                Ok(v) if attr.current == Some(v) => {}
                Ok(v) => match write_attr(name, v, via_asusd, &ctl).await {
                    Ok(()) => r.applied.push(format!("{} {v}", asusd::attr_label(name))),
                    Err(e) => r.failed.push(format!("{name}: {e}")),
                },
            }
        }
    }
    checkpoint!();

    // 3. CPU governor / EPP / boost / limits.
    if let Some(cs) = &p.cpu.control {
        let info = cx.model.as_ref().map(|m| m.cpu.clone()).or_else(|| cx.inv.as_ref().map(|i| i.cpu.clone())).unwrap_or_else(oma_hw::cpu::cpu_info);
        let mut target = cs.clone();
        if !info.available_governors.contains(&target.governor) {
            target.governor = info.available_governors.first().cloned().unwrap_or_default();
        }
        if let Some(epp) = &target.epp {
            // A one-entry list is the governor pinning EPP, not the CPU's choices.
            if info.available_epp.len() > 1 && !info.available_epp.contains(epp) {
                // Say so rather than silently writing less than the profile asks for.
                r.skipped.push(format!("EPP {epp} (this CPU offers {})", info.available_epp.join(" ")));
                target.epp = None;
            }
        }
        let plan = oma_hw::cpu::plan_writes(&info, &target);
        match ctl.write_batch(&plan).await {
            Ok(errs) if errs.is_empty() => {
                // Read back what the kernel now reports, so the summary says what
                // is in place rather than what was sent.
                let live = tokio::task::spawn_blocking(oma_hw::cpu::control_state).await.ok();
                match live.and_then(|l| cpu_readback_mismatch(&target, &l)) {
                    None => r.applied.push(format!("CPU {}{}", target.governor, target.epp.as_ref().map(|e| format!("/{e}")).unwrap_or_default())),
                    Some(why) => r.failed.push(format!("CPU: written, but the kernel reports {why}")),
                }
            }
            Ok(errs) => r.failed.push(format!("CPU: {}", errs.first().map(|(_, e)| e.clone()).unwrap_or_default())),
            Err(e) => r.failed.push(format!("CPU: {e}")),
        }
    }
    checkpoint!();

    // 4. GPUs.
    nvidia_step(&p, &ctl, &mut r).await;
    if let Some(level) = &p.gpu.amd_perf_level {
        let slots: Vec<String> = cx.model.as_ref().map(|m| m.gpus.iter().filter(|g| g.vendor == GpuVendor::Amd).filter_map(|g| g.pci_slot.clone()).collect()).unwrap_or_default();
        if slots.is_empty() {
            r.skipped.push("AMD GPU (none here)".into());
        }
        for slot in slots {
            match ctl.write(format!("/sys/bus/pci/devices/{slot}/power_dpm_force_performance_level"), level).await {
                Ok(()) => r.applied.push(format!("AMD GPU {level}")),
                Err(e) => r.failed.push(format!("AMD GPU: {e}")),
            }
        }
    }

    checkpoint!();

    // 5. Lighting, on the devices this machine has.
    if !p.lighting.zones.is_empty() {
        let conn = zbus::Connection::system().await.ok();
        let mut openrgb: Option<Result<Vec<oma_hw::rgb::RgbDevice>, String>> = None;
        for (key, mode) in &p.lighting.zones {
            if let Some(name) = key.strip_prefix("openrgb:") {
                if openrgb.is_none() {
                    openrgb = Some(oma_hw::rgb::devices().await.map_err(|_| "OpenRGB isn't running".to_string()));
                }
                match openrgb.as_ref() {
                    Some(Ok(devices)) => match oma_hw::lighting::apply_openrgb(devices, name, mode).await {
                        Ok(()) => r.applied.push(format!("{name} {}", oma_hw::lighting::describe(mode))),
                        Err(e) => r.skipped.push(e),
                    },
                    _ => r.skipped.push(format!("{name} (OpenRGB isn't running)")),
                }
            } else if let Some(device) = cx.model.as_ref().and_then(|m| m.lighting.iter().find(|d| d.id.as_str() == key)) {
                let Some(conn) = &conn else {
                    r.failed.push(format!("{}: no system bus", device.label));
                    continue;
                };
                match oma_hw::lighting::apply(conn, device, mode, p.lighting.brightness_for(key), p.accent).await {
                    Ok(()) => r.applied.push(format!("{} {}", device.label, oma_hw::lighting::describe_on(device, mode))),
                    Err(e) => r.failed.push(e),
                }
            } else {
                r.skipped.push(format!("{key} (not on this machine)"));
            }
        }
    }

    // 6. Graphics mode: switching can log you out or need a reboot, so a
    //    profile never does it on its own.
    if let Some(mode) = &p.gfx_mode {
        r.skipped.push(format!("graphics mode {mode} (switch it on the ASUS page)"));
    }
    r
}

/// The profile's NVIDIA settings, where the dGPU can take them now.
async fn nvidia_step(p: &Profile, ctl: &Controller, r: &mut Report) {
    use crate::telemetry::DgpuGateState;
    let Some(nv) = &p.gpu.nvidia else { return };
    if !oma_hw::nvidia::awake() {
        // Waking the dGPU just to set limits would cost battery; they go in on the next apply with it awake.
        r.skipped.push("NVIDIA (asleep or off)".into());
        return;
    }
    // supergfxd kills whatever holds the dGPU while it switches, the helper included.
    match crate::telemetry::dgpu_gate() {
        DgpuGateState::Open => {}
        DgpuGateState::ModeOff => {
            r.skipped.push("NVIDIA (the graphics mode doesn't use it)".into());
            return;
        }
        DgpuGateState::Settling | DgpuGateState::Unknown => {
            r.skipped.push("NVIDIA (graphics settling; applied once it has)".into());
            r.nvidia_deferred = true;
            return;
        }
    }
    let nv = profile_nvidia_control(nv);
    match ctl.nvidia_apply(0, &nv).await {
        Ok(errs) if errs.is_empty() => r.applied.push("NVIDIA".into()),
        Ok(errs) => r.failed.push(format!("NVIDIA: {}", errs.iter().map(|(s, e)| format!("{s} ({e})")).collect::<Vec<_>>().join(", "))),
        Err(e) => r.failed.push(format!("NVIDIA: {e}")),
    }
}

/// The profile's NVIDIA settings alone: what an apply deferred while the dGPU settled.
pub async fn apply_nvidia(p: Profile) -> Report {
    let ctl = Controller::connect().await;
    let mut r = Report::default();
    nvidia_step(&p, &ctl, &mut r).await;
    r
}

/// What differs between the CPU state a profile asked for and what the
/// kernel reports afterwards, if anything. Only what was asked for is
/// compared: an EPP the profile leaves alone is not a mismatch.
fn cpu_readback_mismatch(wanted: &oma_hw::cpu::CpuControlState, live: &oma_hw::cpu::CpuControlState) -> Option<String> {
    if !wanted.governor.is_empty() && live.governor != wanted.governor {
        return Some(format!("governor {}", live.governor));
    }
    if let (Some(w), Some(l)) = (&wanted.epp, &live.epp)
        && w != l
    {
        return Some(format!("EPP {l}"));
    }
    if let (Some(w), Some(l)) = (wanted.boost, live.boost)
        && w != l
    {
        return Some(format!("boost {}", if l { "on" } else { "off" }));
    }
    None
}

/// A profile describes a complete GPU state: when it names no power limit it
/// means the card's stock limit, not "whatever the previous profile left".
/// Otherwise Quiet's 300 W would follow you into Gaming.
fn profile_nvidia_control(nv: &oma_hw::nvidia::NvidiaControl) -> oma_hw::nvidia::NvidiaControl {
    let mut nv = nv.clone();
    if nv.power_limit_w.is_none() {
        nv.reset_power_limit = true;
    }
    nv
}

/// Power modes without a model, in the order the model gives power modes an
/// owner: asusd if it answers, then power-profiles-daemon, then the ACPI
/// platform profile in sysfs, else nothing. Stops at the first that answers,
/// and each question is bounded so a hung daemon can't stall the apply.
async fn fallback_power_modes() -> (Option<Owner>, Vec<String>) {
    async fn within<T, E>(f: impl std::future::Future<Output = Result<T, E>>) -> Option<T> {
        tokio::time::timeout(Duration::from_secs(2), f).await.ok()?.ok()
    }
    if let Some(c) = within(zbus::Connection::system()).await {
        if let Some(p) = within(PlatformProxy::new(&c)).await
            && let Some(choices) = within(p.platform_profile_choices()).await.filter(|v| !v.is_empty())
        {
            return (Some(Owner::Asusd), choices.into_iter().map(|m| PlatformProfile::from_u32(m).label().to_string()).collect());
        }
        if let Some(s) = within(oma_hw::ppd::state(&c)).await.filter(|s| !s.profiles.is_empty()) {
            return (Some(Owner::PowerProfilesDaemon), s.profiles.into_iter().map(|p| p.name).collect());
        }
    }
    match oma_hw::sysfs::read_string("/sys/firmware/acpi/platform_profile_choices").map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>()) {
        Some(choices) if !choices.is_empty() => (Some(Owner::Sysfs), choices),
        _ => (None, Vec::new()),
    }
}

/// Select `mode` through its owner. `Ok(false)` when it was already selected.
async fn set_power_mode(owner: Option<Owner>, mode: &str, ctl: &Controller) -> Result<bool, String> {
    let system = || async { zbus::Connection::system().await.map_err(|e| e.to_string()) };
    match owner {
        Some(Owner::Asusd) => {
            let profile = PlatformProfile::from_label(mode).ok_or_else(|| format!("asusd has no {mode} mode"))?;
            let c = system().await?;
            let p = PlatformProxy::new(&c).await.map_err(|e| e.to_string())?;
            if p.platform_profile().await.ok() == Some(profile as u32) {
                return Ok(false);
            }
            p.set_platform_profile(profile as u32).await.map(|()| true).map_err(|e| e.to_string())
        }
        Some(Owner::PowerProfilesDaemon) => {
            let c = system().await?;
            if oma_hw::ppd::state(&c).await.is_ok_and(|s| s.active == mode) {
                return Ok(false);
            }
            oma_hw::ppd::set_active(&c, mode).await.map(|()| true).map_err(|e| e.to_string())
        }
        Some(Owner::Sysfs) => {
            let path = "/sys/firmware/acpi/platform_profile";
            if oma_hw::sysfs::read_string(path).as_deref() == Some(mode) {
                return Ok(false);
            }
            ctl.write(path, mode).await.map(|()| true).map_err(|e| e.to_string())
        }
        _ => Err("nothing controls power modes here".into()),
    }
}

/// Write a firmware attribute: through asusd when it runs, so its stored copy
/// stays in step; else straight to sysfs through the helper.
async fn write_attr(name: &str, value: i64, via_asusd: bool, ctl: &Controller) -> Result<(), String> {
    if via_asusd {
        let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
        let v = i32::try_from(value).map_err(|e| e.to_string())?;
        asusd::set_armoury_attr(&c, name, v).await.map_err(|e| e.to_string())
    } else {
        ctl.write(format!("{ARMOURY}/{name}/current_value"), value.to_string()).await.map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readback_compares_only_what_was_asked() {
        use oma_hw::cpu::CpuControlState;
        let wanted = CpuControlState { governor: "powersave".into(), epp: Some("balance_performance".into()), boost: Some(true), ..Default::default() };
        let same = CpuControlState { governor: "powersave".into(), epp: Some("balance_performance".into()), boost: Some(true), ..Default::default() };
        assert_eq!(cpu_readback_mismatch(&wanted, &same), None);
        let epp_off = CpuControlState { epp: Some("power".into()), ..same.clone() };
        assert_eq!(cpu_readback_mismatch(&wanted, &epp_off).as_deref(), Some("EPP power"));
        let no_epp_asked = CpuControlState { epp: None, ..wanted.clone() };
        assert_eq!(cpu_readback_mismatch(&no_epp_asked, &epp_off), None, "an EPP the profile leaves alone is not a mismatch");
        let boost_off = CpuControlState { boost: Some(false), ..same.clone() };
        assert_eq!(cpu_readback_mismatch(&wanted, &boost_off).as_deref(), Some("boost off"));
    }

    #[test]
    fn profile_without_limit_means_stock_limit() {
        use oma_hw::nvidia::NvidiaControl;
        let none = NvidiaControl::default();
        assert!(profile_nvidia_control(&none).reset_power_limit);
        let some = NvidiaControl { power_limit_w: Some(300), ..Default::default() };
        let got = profile_nvidia_control(&some);
        assert!(!got.reset_power_limit);
        assert_eq!(got.power_limit_w, Some(300));
    }

    #[test]
    fn summaries_say_what_happened() {
        let mut r = Report::default();
        assert_eq!(r.summary("Quiet"), Ok("Quiet: already in place".into()));
        r.applied.push("power mode Quiet".into());
        r.skipped.push("NVIDIA (asleep or off)".into());
        assert_eq!(r.summary("Quiet"), Ok("Quiet applied: power mode Quiet · skipped NVIDIA (asleep or off)".into()));
        r.failed.push("CPU: denied".into());
        assert_eq!(r.summary("Quiet"), Err("Quiet partly applied (power mode Quiet); failed: CPU: denied · skipped NVIDIA (asleep or off)".into()));
    }
}
