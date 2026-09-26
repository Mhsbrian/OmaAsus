//! Settings: helper installation, CoolerControl credentials, overlay layout,
//! Hyprland hotkey snippet, LiveDash OLED test, diagnostics.

use crate::app::{App, Message};
use crate::pages::cpu::{slider_style, toggle};
use crate::theme::{self, size, space};
use crate::widgets;
use iced::widget::{column, container, row, scrollable, slider, Column, Row};
use iced::{Element, Length};
use oma_hw::knowledge::Source;
use oma_hw::model::{Owner, SensorRole};

/// Width of the hardware summary's labels ("Power modes" in tracked capitals).
const LABEL_W: f32 = 124.0;

/// What a component is called in the hardware summary.
fn owner_name(o: Owner) -> &'static str {
    match o {
        Owner::Asusd => "asusd",
        Owner::PowerProfilesDaemon => "power-profiles-daemon",
        Owner::Sysfs => "the kernel",
        Owner::Supergfxd => "supergfxd",
        Owner::CoolerControl => "CoolerControl",
        Owner::OmaAsus => "OmaAsus",
        Owner::Firmware => "firmware",
    }
}

/// Sensor roles worth naming in the summary.
fn role_name(r: SensorRole) -> Option<&'static str> {
    Some(match r {
        SensorRole::CpuTemp => "CPU",
        SensorRole::IgpuTemp => "iGPU",
        SensorRole::DgpuTemp => "dGPU",
        SensorRole::Coolant => "coolant",
        SensorRole::Vrm => "VRM",
        SensorRole::Board => "board",
        SensorRole::Storage => "storage",
        SensorRole::Memory => "memory",
        SensorRole::PackagePower => "package power",
        SensorRole::GpuPower => "GPU power",
        SensorRole::FanSpeed => "fan speed",
        SensorRole::CpuCore | SensorRole::GpuHotspot | SensorRole::Wireless | SensorRole::Other => return None,
    })
}

/// Where a knowledge fact came from.
fn source_name(s: &Source) -> String {
    match s {
        Source::Verified(w) => format!("verified: {w}"),
        Source::Reference(w) => format!("from {w}"),
        Source::User => "your quirks.toml".into(),
    }
}

/// The marketing name in a PCI description: "… [Radeon 880M / 890M] (rev c1)" → "Radeon 880M / 890M".
fn gpu_name(name: &str) -> &str {
    name.rsplit_once('[').and_then(|(_, r)| r.split_once(']')).map_or(name, |(n, _)| n)
}

#[derive(Debug, Clone)]
pub enum SettingsMsg {
    InstallHelper,
    RecheckHelper,
    CcUrl(String),
    CcPassword(String),
    CcTest,
    OverlayAnchor(String),
    OverlayWidth(f64),
    OverlayHeight(f64),
    OverlayOpacity(f64),
    OverlayMargin(f64),
    HudToggle(bool),
    TrayToggle(bool),
    OledText,
    OledHwMonitor,
    TelemetryHz(f64),
}

pub fn view(app: &App) -> Element<'_, Message> {
    let p = app.palette;
    let inv = app.inventory.as_ref();
    let helper_ok = app.controller_ready;

    let helper = widgets::card(
        p,
        column![
            row![widgets::title(p, "Privileged helper"), widgets::hfill(), if helper_ok { widgets::pill(p, "installed & running", p.ok) } else { widgets::pill(p, "not running", p.warn) }].align_y(iced::Alignment::Center),
            widgets::dim(p, "Fans, CPU governor, GPU limits and lighting controllers live behind root-only sysfs/hidraw nodes. OmaAsus ships a small system service (com.omaasus.Helper1) gated by polkit: routine tuning is allowed for the active session, overclocking offsets and raw device writes ask for your password once."),
            row![
                widgets::btn(p, if helper_ok { "Reinstall helper" } else { "Install helper (pkexec)" }, widgets::ButtonKind::Primary, Some(Message::Settings(SettingsMsg::InstallHelper))),
                widgets::btn(p, "Re-check", widgets::ButtonKind::Ghost, Some(Message::Settings(SettingsMsg::RecheckHelper))),
            ]
            .spacing(space::SM),
            widgets::mono(p, app.helper_log.clone(), size::CAPTION),
        ]
        .spacing(space::MD),
    )
    .width(Length::Fill);

    let text_style = move |_: &iced::Theme, _: iced::widget::text_input::Status| iced::widget::text_input::Style { background: iced::Background::Color(p.glass), border: iced::Border { color: p.line_strong, width: 1.0, radius: theme::radius::SM.into() }, icon: p.text_dim, placeholder: p.text_faint, value: p.text, selection: p.accent_soft };
    let cc_detected = inv.map(|i| i.daemons.coolercontrold).unwrap_or(false);
    let cc = widgets::card(
        p,
        column![
            row![widgets::title(p, "CoolerControl"), widgets::hfill(), if app.cc_connected { widgets::pill(p, "connected", p.ok) } else if cc_detected { widgets::pill(p, "detected — sign in", p.warn) } else { widgets::pill(p, "not installed", p.text_dim) }].align_y(iced::Alignment::Center),
            widgets::dim(p, "If CoolerControl runs on this machine, OmaAsus can delegate fan curves to it and switch its Modes per profile. Enter the daemon URL and the CCAdmin password (default: coolAdmin)."),
            row![
                iced::widget::text_input("http://localhost:11987", &app.config.coolercontrol.url).on_input(|s| Message::Settings(SettingsMsg::CcUrl(s))).font(theme::font::MONO).size(size::BODY).style(text_style).width(Length::Fixed(260.0)),
                iced::widget::text_input("password", app.config.coolercontrol.password.as_deref().unwrap_or("")).secure(true).on_input(|s| Message::Settings(SettingsMsg::CcPassword(s))).font(theme::font::MONO).size(size::BODY).style(text_style).width(Length::Fixed(200.0)),
                widgets::btn(p, "Connect", widgets::ButtonKind::Primary, Some(Message::Settings(SettingsMsg::CcTest))),
            ]
            .spacing(space::SM)
            .wrap(),
        ]
        .spacing(space::MD),
    )
    .width(Length::Fill);

    let o = &app.config.overlay;
    let anchor_btn = |label: &str, a: &'static str| widgets::btn(p, label, if o.anchor == a { widgets::ButtonKind::Primary } else { widgets::ButtonKind::Ghost }, Some(Message::Settings(SettingsMsg::OverlayAnchor(a.into()))));
    let overlay = widgets::card(
        p,
        column![
            widgets::title(p, "Overlay"),
            widgets::dim(p, "The overlay is a layer-shell panel with compositor blur, toggled from anywhere with `omaasus toggle`. Bind it in Hyprland:"),
            widgets::mono(p, "o.bind(\"SUPER + F12\", \"OmaAsus overlay\", hl.dsp.exec({ cmd = \"omaasus toggle\" }))", size::SMALL),
            widgets::dim(p, "(classic syntax: bind = SUPER, F12, exec, omaasus toggle). Add `omaasus --overlay` to autostart so the daemon is always ready."),
            row![widgets::eyebrow(p, "Anchor"), anchor_btn("Right", "right"), anchor_btn("Left", "left"), anchor_btn("Top center", "center")].spacing(space::SM).align_y(iced::Alignment::Center),
            // Two by two, so each label keeps room for its value at any width.
            row![
                column![row![widgets::eyebrow(p, "Width"), widgets::hfill(), widgets::mono(p, format!("{} px", o.width), size::SMALL)], slider(360.0..=900.0, o.width as f64, |v| Message::Settings(SettingsMsg::OverlayWidth(v))).step(10.0).style(slider_style(p))].spacing(space::XS).width(Length::Fill),
                // The panel fits its content; this only caps it (0 = the space under the bar).
                column![row![widgets::eyebrow(p, "Max height"), widgets::hfill(), widgets::mono(p, if o.height == 0 { "screen".into() } else { format!("{} px", o.height) }, size::SMALL)], slider(0.0..=1600.0, o.height as f64, |v| Message::Settings(SettingsMsg::OverlayHeight(v))).step(50.0).style(slider_style(p))].spacing(space::XS).width(Length::Fill),
            ]
            .spacing(space::LG),
            row![
                column![row![widgets::eyebrow(p, "Margin"), widgets::hfill(), widgets::mono(p, format!("{} px", o.margin), size::SMALL)], slider(0.0..=64.0, o.margin as f64, |v| Message::Settings(SettingsMsg::OverlayMargin(v))).step(2.0).style(slider_style(p))].spacing(space::XS).width(Length::Fill),
                column![row![widgets::eyebrow(p, "Opacity"), widgets::hfill(), widgets::mono(p, format!("{:.0}%", o.opacity * 100.0), size::SMALL)], slider(0.5..=1.0, o.opacity as f64, |v| Message::Settings(SettingsMsg::OverlayOpacity(v))).step(0.02).style(slider_style(p))].spacing(space::XS).width(Length::Fill),
            ]
            .spacing(space::LG),
            toggle(p, "Compact HUD strip when a game is fullscreen", o.hud_enabled, true, |b| Message::Settings(SettingsMsg::HudToggle(b))),
            toggle(p, "Tray icon in the bar (closing the window keeps OmaAsus running)", app.config.tray_enabled, true, |b| Message::Settings(SettingsMsg::TrayToggle(b))),
            widgets::dim(p, match (&app.tray, app.config.tray_enabled) { (Some(_), _) => "Tray: registered with the bar. Click it for the panel, middle-click for the window, right-click for profiles.", (None, true) => "Tray: waiting for a StatusNotifier host (Omarchy's bar provides one).", (None, false) => "Tray: off. Closing the window exits OmaAsus unless the overlay is open." }),
            row![widgets::eyebrow(p, "Telemetry rate"), widgets::hfill(), widgets::mono(p, format!("{} Hz", app.config.telemetry_hz), size::SMALL)],
            slider(1.0..=5.0, app.config.telemetry_hz as f64, |v| Message::Settings(SettingsMsg::TelemetryHz(v))).step(1.0).style(slider_style(p)),
        ]
        .spacing(space::MD),
    )
    .width(Length::Fill);

    let has_oled = inv.map(|i| i.features.livedash_oled).unwrap_or(false);
    let oled = widgets::card(
        p,
        column![
            row![widgets::title(p, "LiveDash OLED"), widgets::hfill(), widgets::pill(p, "detected", p.ok)].align_y(iced::Alignment::Center),
            widgets::dim(p, "ROG Extreme boards carry a 2\" OLED (and the AniMe Matrix on newer models). OmaAsus can push a two-line text readout (CPU/GPU temperature) in the chip's text mode — experimental, requires the helper."),
            row![
                widgets::btn(p, "Show live temps on OLED", widgets::ButtonKind::Primary, has_oled.then_some(Message::Settings(SettingsMsg::OledText))),
                widgets::btn(p, "Back to hardware monitor", widgets::ButtonKind::Ghost, has_oled.then_some(Message::Settings(SettingsMsg::OledHwMonitor))),
            ]
            .spacing(space::SM),
        ]
        .spacing(space::MD),
    )
    .width(Length::Fill);

    // What the hardware model found, and the knowledge it used.
    let diag: Element<Message> = match (inv, app.model.as_deref()) {
        (Some(i), Some(m)) => {
            let f = &i.features;
            let line = |what: &str, value: String| -> Element<Message> { row![container(widgets::eyebrow(p, what)).width(Length::Fixed(LABEL_W)), widgets::body(p, value)].spacing(space::MD).align_y(iced::Alignment::Center).into() };
            let or_none = |v: Vec<String>| if v.is_empty() { "none found".to_string() } else { v.join(", ") };
            let cpu_caps: Vec<&str> = [("EPP", f.cpu_epp), ("boost control", f.cpu_boost), ("SMT", f.cpu_smt)].into_iter().filter(|(_, on)| *on).map(|(n, _)| n).collect();
            // Most telling first.
            let order = [SensorRole::CpuTemp, SensorRole::IgpuTemp, SensorRole::DgpuTemp, SensorRole::Coolant, SensorRole::Vrm, SensorRole::Board, SensorRole::Memory, SensorRole::Storage, SensorRole::PackagePower, SensorRole::GpuPower, SensorRole::FanSpeed];
            let roles: Vec<&str> = order.into_iter().filter(|r| m.sensors.iter().any(|s| s.role == *r)).filter_map(role_name).collect();
            let mut lights: Vec<String> = m.lighting.iter().map(|d| d.label.clone()).collect();
            if !app.rgb_devices.is_empty() {
                lights.push(format!("{} through OpenRGB", app.rgb_devices.len()));
            }
            let mut rows = vec![
                line("Machine", format!("{} · board {} · BIOS {}", m.identity.product, m.identity.board, m.identity.bios)),
                line("CPU", if cpu_caps.is_empty() { m.cpu.model.clone() } else { format!("{} · {}", m.cpu.model, cpu_caps.join(", ")) }),
                line("GPUs", or_none(m.gpus.iter().map(|g| format!("{} ({}, {})", gpu_name(&g.name), if g.integrated { "integrated" } else { "discrete" }, format!("{:?}", g.power).to_lowercase())).collect())),
                line("Fans", if m.fans.is_empty() { "none controllable".into() } else { format!("{} · through {}", m.fans.iter().map(|f| f.label.as_str()).collect::<Vec<_>>().join(", "), owner_name(m.fan_owner_now(cc_detected))) }),
                line("Sensors", format!("{} · {}", m.sensors.len(), roles.join(", "))),
                line("Sources", app.snapshot.as_ref().map(|s| if s.sources.is_empty() { "none sampled yet".to_string() } else { s.sources.iter().map(|h| if h.healthy { h.name.clone() } else { format!("{} (silent {}s)", h.name, h.stalled_s.unwrap_or(0)) }).collect::<Vec<_>>().join(", ") }).unwrap_or_else(|| "waiting for the first frame".into())),
                line("Lighting", or_none(lights)),
            ];
            if let Some(o) = m.controls.power_owner {
                rows.push(line("Power modes", format!("{} · through {}", m.controls.power_modes.join(" / "), owner_name(o))));
            }
            if let (Some(o), Some(mode)) = (m.controls.gpu_owner, &m.controls.gpu_mode) {
                rows.push(line("Graphics", format!("{mode} · through {}", owner_name(o))));
            }
            if let Some(c) = m.controls.charge_limit {
                rows.push(line("Battery", format!("charge limit {c}%")));
            }
            let flag = |name: &str, on: bool| widgets::pill(p, name, if on { p.ok } else { p.text_faint });
            let services: Vec<Element<Message>> = vec![flag("asusd", f.asusd), flag("supergfxd", f.supergfxd), flag("power-profiles-daemon", i.daemons.power_profiles_daemon), flag("CoolerControl", f.coolercontrol), flag("OpenRGB", f.openrgb), flag("liquidctl", f.liquidctl), flag("GameMode", i.daemons.gamemode)];
            rows.push(row![container(widgets::eyebrow(p, "Services")).width(Length::Fixed(LABEL_W)), Row::with_children(services).spacing(space::XS).wrap()].spacing(space::MD).into());
            let mut col = Column::with_children(rows).spacing(space::SM);
            if !m.notes.is_empty() {
                col = col.push(widgets::eyebrow(p, "Knowledge used"));
                for n in &m.notes {
                    col = col.push(widgets::dim(p, format!("{} · {}", n.what, source_name(&n.source))));
                }
            }
            col.push(widgets::dim(p, format!("kernel {} · config {}", i.kernel, crate::config_store::path().display()))).into()
        }
        _ => widgets::dim(p, "Detecting…"),
    };
    let diagnostics = widgets::card(p, column![widgets::title(p, "Detected hardware"), diag].spacing(space::MD)).width(Length::Fill);

    // CoolerControl and the OLED only where they are (or CoolerControl was set up).
    let mut left = column![helper].spacing(space::LG);
    if cc_detected || app.cc_connected || app.config.coolercontrol.password.is_some() {
        left = left.push(cc);
    }
    if has_oled {
        left = left.push(oled);
    }
    let left = scrollable(left).height(Length::Fill);
    let right = scrollable(column![overlay, diagnostics].spacing(space::LG)).height(Length::Fill);
    column![
        widgets::headline(p, "Settings"),
        row![container(left).width(Length::FillPortion(1)), container(right).width(Length::FillPortion(1))].spacing(space::LG).height(Length::Fill),
    ]
    .spacing(space::LG)
    .height(Length::Fill)
    .into()
}
