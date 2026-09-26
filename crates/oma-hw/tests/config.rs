//! Config schema migration, and profiles generated from a machine's model.

use oma_hw::capture::RawInventory;
use oma_hw::knowledge::Overrides;
use oma_hw::model::HardwareModel;
use oma_hw::profile::{Config, ProfileRole, SCHEMA, match_power_mode};
use std::path::Path;

fn fixture(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(rel)).expect(rel)
}

/// The v1 config the previous release wrote on first run: built-in desktop
/// profiles with fixed fan names and power-profiles-daemon names.
#[test]
fn v1_config_migrates() {
    let mut c: Config = toml::from_str(&fixture("configs/v1-g14-defaults.toml")).expect("v1 parses");
    assert_eq!(c.schema_version, 1);
    let targets: Vec<&str> = c.profiles[0].cooling.fans.iter().map(|f| f.target.as_str()).collect();
    assert_eq!(targets, ["ryujin:pump", "ryujin:radiator", "lianli:0:ch1"], "fan names read as model ids");
    assert!(c.profiles[3].cooling.fans.iter().any(|f| f.target.as_str() == "nvidia:0:fans"));

    assert!(c.migrate());
    assert_eq!(c.schema_version, SCHEMA);
    let summary: Vec<(&str, Option<&str>, Option<ProfileRole>)> = c.profiles.iter().map(|p| (p.name.as_str(), p.cpu.power_mode.as_deref(), p.role)).collect();
    assert_eq!(
        summary,
        [
            ("Silent", Some("power-saver"), Some(ProfileRole::Quiet)),
            ("Balanced", Some("balanced"), Some(ProfileRole::Balanced)),
            ("Gaming", Some("performance"), Some(ProfileRole::Performance)),
            ("Turbo", Some("performance"), Some(ProfileRole::Performance)),
        ]
    );
    assert!(!c.migrate(), "already current");

    // The migrated config round-trips, without the v1 fields.
    let text = toml::to_string_pretty(&c).expect("serialises");
    assert!(!text.contains("ppd_profile") && !text.contains("RyujinPump"));
    let again: Config = toml::from_str(&text).expect("v2 parses");
    assert_eq!(again, c);
}

#[test]
fn generated_profiles_follow_the_machine() {
    let raw: RawInventory = serde_json::from_str(&fixture("ga403wr/raw-inventory.json")).expect("fixture");
    let c = Config::generated(&HardwareModel::build(&raw, &Overrides::default()));
    assert_eq!(c.schema_version, SCHEMA);
    let summary: Vec<(&str, Option<&str>, Option<ProfileRole>)> = c.profiles.iter().map(|p| (p.name.as_str(), p.cpu.power_mode.as_deref(), p.role)).collect();
    assert_eq!(
        summary,
        [("Quiet", Some("Quiet"), Some(ProfileRole::Quiet)), ("Balanced", Some("Balanced"), Some(ProfileRole::Balanced)), ("Performance", Some("Performance"), Some(ProfileRole::Performance))]
    );
    for p in &c.profiles {
        assert!(p.cooling.fans.is_empty() && p.cpu.control.is_none() && p.gpu.nvidia.is_none(), "{} forces nothing but its power mode", p.name);
    }
    // The machine was in Quiet when captured.
    assert_eq!(c.profile(c.active_profile).map(|p| p.name.as_str()), Some("Quiet"));
    assert_eq!(c.profile(c.default_profile).map(|p| p.name.as_str()), Some("Balanced"));
    assert!(!c.rules.is_empty());
    assert!(c.rules.iter().all(|r| c.profile(r.profile).and_then(|p| p.role) == Some(ProfileRole::Performance)));
}

#[test]
fn power_modes_carry_across_machines() {
    let asusd = ["Quiet".to_string(), "Balanced".into(), "Performance".into()];
    let ppd = ["power-saver".to_string(), "balanced".into(), "performance".into()];
    assert_eq!(match_power_mode("power-saver", &asusd), Some("Quiet"));
    assert_eq!(match_power_mode("Quiet", &ppd), Some("power-saver"));
    assert_eq!(match_power_mode("Turbo", &asusd), Some("Performance"));
    assert_eq!(match_power_mode("balanced", &asusd), Some("Balanced"));
    assert_eq!(match_power_mode("Custom", &asusd), None);
}

/// A power mode set outside a profile (a keyboard shortcut, `powerprofilesctl`)
/// is followed by the profile that carries it: the default profile when it
/// does, else the first in order.
#[test]
fn a_power_mode_set_elsewhere_names_the_profile_that_follows_it() {
    let mut c: Config = toml::from_str(&fixture("configs/v1-g14-defaults.toml")).expect("v1 parses");
    c.migrate();
    c.default_profile = c.profiles[1].id;
    let ppd = ["power-saver".to_string(), "balanced".into(), "performance".into()];
    fn name(p: Option<&oma_hw::profile::Profile>) -> Option<&str> {
        p.map(|p| p.name.as_str())
    }
    assert_eq!(name(c.profile_for_power_mode("power-saver", &ppd)), Some("Silent"));
    // Gaming and Turbo both carry performance: the first in order, unless the default is one of them.
    assert_eq!(name(c.profile_for_power_mode("performance", &ppd)), Some("Gaming"));
    c.default_profile = c.profiles[3].id;
    assert_eq!(name(c.profile_for_power_mode("performance", &ppd)), Some("Turbo"));
    // The same profiles mean something where asusd names the modes.
    let asusd = ["Quiet".to_string(), "Balanced".into(), "Performance".into()];
    assert!(c.profiles[0].carries_power_mode("Quiet", &asusd));
    assert!(!c.profiles[0].carries_power_mode("Balanced", &asusd));
    assert_eq!(name(c.profile_for_power_mode("Quiet", &asusd)), Some("Silent"));
    assert_eq!(name(c.profile_for_power_mode("Custom", &asusd)), None, "no profile carries it");
}
