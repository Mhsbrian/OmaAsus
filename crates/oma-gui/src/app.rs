//! Application state, message loop, and multi-surface routing
//! (overlay layer-shell panel + normal window from one process).

use crate::pages::cooling::CoolingMsg;
use crate::pages::cpu::CpuMsg;
use crate::pages::gpu::GpuMsg;
use crate::pages::lighting::LightingMsg;
use crate::pages::profiles::ProfilesMsg;
use crate::pages::settings::SettingsMsg;
use crate::pages::asus::{AsusMsg, AsusState};
use crate::pages::automation::AutomationMsg;
use crate::automation::{AutoEvent, AutoState};
use crate::pages::Page;
use crate::theme::{self, Palette, size, space};
use crate::widgets;
use crate::{ipc, telemetry, tray};
use crate::widgets::reveal;
use crate::widgets::icons::{self, Icon};
use iced::widget::{column, container, row, shader, stack};
use iced::window::Id;
use iced::{Background, Element, Length, Subscription, Task, Theme};
use iced_exwlshell::actions::IcedXdgWindowSettings;
use iced_exwlshell::daemon;
use iced_exwlshell::reexport::{Anchor, BlurOption, KeyboardInteractivity, Layer, LayerSize, NewLayerShellSettings, OutputOption, PixelSize};
use iced_exwlshell::settings::{LayerShellSettings, Settings, StartMode};
use iced_exwlshell::to_exwlshell_message;
use oma_hw::profile::{Config, FanMode, FanOwner, FanTarget, ProfileRole};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

pub const HISTORY: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Overlay,
    Window,
}

#[derive(Default)]
pub struct History {
    pub cpu_load: VecDeque<f32>,
    pub cpu_temp: VecDeque<f32>,
    pub gpu_load: VecDeque<f32>,
    pub gpu_temp: VecDeque<f32>,
    pub gpu_power: VecDeque<f32>,
    pub coolant: VecDeque<f32>,
    /// Package power (RAPL, or an APU's SoC power).
    pub power: VecDeque<f32>,
}

/// A missing reading is stored as NaN: charts draw a gap there, never zero.
fn push(v: &mut VecDeque<f32>, x: Option<f32>) {
    if v.len() >= HISTORY {
        v.pop_front();
    }
    v.push_back(x.unwrap_or(f32::NAN));
}

pub struct App {
    pub palette: Palette,
    pub theme_name: String,
    pub page: Page,
    pub config: Config,
    pub snapshot: Option<Arc<telemetry::Snapshot>>,
    /// When the last frame arrived: fan control only trusts a fresh one.
    pub snapshot_at: Option<std::time::Instant>,
    pub hist: History,
    pub inventory: Option<Arc<oma_hw::SystemInventory>>,
    /// What this machine has: detection plus knowledge.
    pub model: Option<Arc<oma_hw::model::HardwareModel>>,
    pub inventory_title: String,
    pub controller_ready: bool,
    pub surfaces: HashMap<Id, Surface>,
    pub overlay_only: bool,
    pub toast: Option<(String, bool)>,
    pub toast_at: Option<std::time::Instant>,
    pub cpu_edit: oma_hw::cpu::CpuControlState,
    pub cpu_synced: bool,
    pub gpu_edit: oma_hw::nvidia::NvidiaControl,
    pub gpu_dirty: bool,
    pub nvidia_info: Option<Arc<oma_hw::nvidia::NvidiaInfo>>,
    pub cooling_sel: Option<FanTarget>,
    pub cc: Option<oma_hw::coolercontrol::CoolerControl>,
    pub cc_connected: bool,
    pub cc_modes: Vec<oma_hw::coolercontrol::CcMode>,
    pub fan_backend: Option<Arc<crate::fans::FanBackend>>,
    pub fan_engine: oma_hw::fanengine::FanEngine,
    pub fan_errors: u32,
    pub rgb_server: bool,
    pub rgb_devices: Vec<oma_hw::rgb::RgbDevice>,
    pub rgb_sel: usize,
    pub rgb_hex: String,
    pub rgb_thermal: bool,
    pub rgb_installed: bool,
    /// What each lighting device in the model shows now, by device id.
    pub lights: std::collections::BTreeMap<String, oma_hw::lighting::LightState>,
    /// A model lighting device picked on the Lighting page (else an OpenRGB one).
    pub light_sel: Option<String>,
    /// Slash brightness while its slider is dragged.
    pub slash_drag: Option<u8>,
    pub profile_sel: Option<uuid::Uuid>,
    pub auto_state: AutoState,
    pub helper_log: String,
    pub asus: AsusState,
    pub t0: std::time::Instant,
    pub now: std::time::Instant,
    pub smooth: Smooth,
    /// Live tray item, once the bar's StatusNotifierWatcher accepted it.
    pub tray: Option<tray::TrayHandle>,
    /// A StatusNotifier host is actually showing the item right now.
    pub tray_hosted: bool,
    tray_synced: tray::TrayState,
    /// Drop-down animation of the overlay panel.
    overlay_phase: Option<OverlayPhase>,
    /// Shutting down: fans are being handed back, the engine must not retake them.
    quitting: bool,
    /// A theme change under way: the palette crosses from the first to the
    /// second over `THEME_FADE`, so the window turns with the desktop.
    theme_fade: Option<(Palette, Palette, std::time::Instant)>,
    /// power-profiles-daemon, for the panel's power-mode row when asusd isn't the owner.
    pub ppd: Option<oma_hw::ppd::PpdState>,
    /// Charge-limit slider position while it is being dragged in the panel.
    pub quick_charge: Option<f64>,
    /// A firmware setting's slider while it is dragged (ASUS page).
    pub attr_drag: Option<(String, f64)>,
    /// A graphics mode waiting for the user to confirm the switch.
    pub gfx_confirm: Option<oma_hw::supergfx::GfxMode>,
    /// Logical height free under the bars on the focused output (Hyprland).
    screen_h: Option<f32>,
    /// Current panel surface height and anchor, for resizing it to its content.
    overlay_h: u32,
    overlay_anchor: Anchor,
    /// Number of the latest profile apply; older ones stop when they see it change.
    apply_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Which apply runs, which waits, and what the last one did.
    pub coord: crate::coordinator::Coordinator,
    last_reapply: Option<std::time::Instant>,
    /// Deferred NVIDIA settings are waiting for the dGPU to be ready.
    dgpu_wait: bool,
}

/// The overlay panel slides down from the bar when it opens and folds back up
/// before its surface is destroyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayPhase {
    Opening(std::time::Instant),
    Open,
    Closing(std::time::Instant),
}

/// How long OmaAsus stays away from the dGPU when a graphics switch starts or
/// before it asks for one; waiting for the switch to finish extends it. (On
/// the GA403WR a sleep hook's switch killed OmaAsus ~4 s after the system
/// came back, for holding the dGPU.)
const DGPU_HOLD_OFF: std::time::Duration = std::time::Duration::from_secs(20);

/// Stay off the dGPU until supergfxd's switch has finished. It has no
/// completion signal, so it is polled until nothing is pending or the mode is
/// the pending one; an error keeps it waiting, and the wait is bounded.
fn wait_for_switch() -> Task<Message> {
    Task::perform(
        async {
            let started = std::time::Instant::now();
            let conn = zbus::Connection::system().await.ok();
            // A logout wait (30 s in supergfxd) plus the switch itself.
            while started.elapsed() < std::time::Duration::from_secs(90) {
                // Longer than a slow pass (two calls, 3 s timeout each), so the hold never lapses.
                telemetry::hold_off_dgpu(std::time::Duration::from_secs(8));
                if let Some(c) = &conn
                    && let Ok((mode, pending)) = oma_hw::supergfx::switch_state(c).await
                    && oma_hw::supergfx::switch_done(mode, pending)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        },
        |()| Message::GraphicsSettled,
    )
}

/// Whether supergfxd runs, and its mode when it answers: where it runs, the
/// dGPU is left alone until it has said.
fn probe_graphics() -> Task<Message> {
    Task::perform(
        async {
            let Ok(c) = zbus::Connection::system().await else { return (false, None) };
            if !oma_hw::supergfx::present(&c).await {
                return (false, None);
            }
            (true, oma_hw::supergfx::state(&c).await.ok().map(|s| s.mode))
        },
        |(running, mode)| Message::GfxProbe(running, mode),
    )
}

const OVERLAY_OPEN: std::time::Duration = std::time::Duration::from_millis(280);
const OVERLAY_CLOSE: std::time::Duration = std::time::Duration::from_millis(170);
/// How long a theme change takes to cross the window.
const THEME_FADE: std::time::Duration = std::time::Duration::from_millis(350);

/// Eased display values so gauges glide instead of stepping.
#[derive(Debug, Default, Clone, Copy)]
pub struct Smooth {
    pub cpu_t: f32,
    pub gpu_t: f32,
    pub coolant: f32,
    pub gpu_w: f32,
    /// A discrete GPU's power while it is awake, else the package's.
    pub power_w: f32,
    /// Most package power seen, to scale it when no limit is reported.
    pub power_peak: f32,
    /// The fastest fan.
    pub fan_rpm: f32,
    pub cpu_load: f32,
    pub gpu_load: f32,
    pub heat: f32,
    pub load: f32,
}

/// Ease `cur` towards a reading. No reading (None) means unavailable, shown
/// as such at once; the first reading after a gap is taken as is.
fn ease(cur: &mut f32, target: Option<f32>, k: f32) {
    match target {
        None => *cur = f32::NAN,
        Some(t) if !cur.is_finite() => *cur = t,
        Some(t) => *cur += (t - *cur) * k,
    }
}

/// The larger of two readings, ignoring ones that are missing.
fn finite_max(a: f32, b: f32) -> Option<f32> {
    match (a.is_finite(), b.is_finite()) {
        (true, true) => Some(a.max(b)),
        (true, false) => Some(a),
        (false, true) => Some(b),
        (false, false) => None,
    }
}

/// The curve temperature source matching what an output cools.
fn temp_source_for(input: Option<oma_hw::model::CurveInput>) -> oma_hw::profile::TempSource {
    use oma_hw::model::CurveInput;
    use oma_hw::profile::TempSource;
    match input {
        Some(CurveInput::Coolant) => TempSource::Coolant,
        Some(CurveInput::Gpu) => TempSource::Gpu,
        Some(CurveInput::CpuOrGpu) => TempSource::CpuGpuMax,
        _ => TempSource::CpuTctl,
    }
}

fn load_ppd() -> Task<Message> {
    Task::perform(
        async {
            let c = zbus::Connection::system().await.ok()?;
            oma_hw::ppd::state(&c).await.ok()
        },
        Message::PpdLoaded,
    )
}

/// Logical height left under the bars on the focused output, from Hyprland.
fn load_screen() -> Task<Message> {
    Task::perform(
        async {
            let monitors = oma_hw::hypr::request_json("monitors").await.ok()?;
            let m = monitors.as_array()?.iter().find(|m| m["focused"].as_bool() == Some(true))?;
            let scale = m["scale"].as_f64().filter(|s| *s > 0.0)?;
            // Transforms 1, 3, 5 and 7 turn the output by 90°.
            let rotated = m["transform"].as_i64().is_some_and(|t| t % 2 == 1);
            let h = m[if rotated { "width" } else { "height" }].as_f64()? / scale;
            let reserved = m["reserved"].as_array()?;
            let edge = |i: usize| reserved.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
            Some((h - edge(1) - edge(3)) as f32)
        },
        Message::ScreenSpace,
    )
}

#[to_exwlshell_message(multi)]
#[derive(Debug, Clone)]
pub enum Message {
    Telemetry(telemetry::Event),
    Ipc(ipc::Command),
    Hardware(Arc<oma_hw::capture::RawInventory>),
    Controller(bool),
    Navigate(Page),
    Cpu(CpuMsg),
    Gpu(GpuMsg),
    NvidiaInfo(Option<Arc<oma_hw::nvidia::NvidiaInfo>>),
    Cooling(CoolingMsg),
    CcReady(bool, Vec<oma_hw::coolercontrol::CcMode>),
    FanBackend(Arc<crate::fans::FanBackend>),
    FanResult(oma_hw::fanengine::Command, Result<(), String>),
    /// The fan engine's own clock, independent of telemetry frames.
    FanSafety(std::time::Instant),
    Lighting(LightingMsg),
    Profiles(ProfilesMsg),
    Automation(AutomationMsg),
    Auto(AutoEvent),
    Settings(SettingsMsg),
    FontLoaded(bool),
    Tick(std::time::Instant),
    Asus(AsusMsg),
    AsusLoaded(AsusState),
    Quick(crate::pages::quick::QuickMsg),
    PpdLoaded(Option<oma_hw::ppd::PpdState>),
    ScreenSpace(Option<f32>),
    HelperInstalled(Result<String, String>),
    RgbDevices(Result<Vec<oma_hw::rgb::RgbDevice>, String>),
    ApplyProfile(uuid::Uuid),
    Applied(Result<String, String>),
    /// Automation picked a profile.
    AutoApply(uuid::Uuid),
    /// Apply the active profile again (resume, charger, graphics switch).
    Reapply(&'static str),
    /// A graphics switch supergfxd was running has finished (or stopped being awaited).
    GraphicsSettled,
    /// Whether supergfxd runs, and its mode when it answered (startup, and
    /// again while it runs without answering).
    GfxProbe(bool, Option<oma_hw::supergfx::GfxMode>),
    GfxRetry,
    /// Whether the dGPU became ready for NVIDIA settings an apply deferred.
    DgpuReady(bool),
    NvidiaApplied(String, crate::apply::Report),
    /// An apply reported back: its generation, so a replaced one is discarded.
    ProfileApplied(u64, String, crate::apply::Origin, crate::apply::Report),
    System(crate::events::Event),
    ToggleOverlay,
    OpenWindow,
    Tray(tray::Event),
    /// Omarchy applied a theme: the desktop's palette and its name.
    ThemeChanged(Palette, String),
    /// Whether an OpenRGB server answers (probed off the UI thread): its
    /// devices are fetched when it does.
    RgbServer(bool),
    /// The same, noted without fetching devices.
    RgbServerSeen(bool),
    /// The held OpenRGB connection: made (or the server's device list
    /// changed), or lost because the server exited.
    RgbLink(bool),
    /// The server's devices, listed after the link was made: the active
    /// profile's lighting goes back on them.
    RgbUp(Result<Vec<oma_hw::rgb::RgbDevice>, String>),
    /// The active profile's OpenRGB lighting was put back: what failed.
    LightingRestored(Vec<String>),
    /// `omaasus reload-theme`: read the theme again now.
    ReloadTheme,
    /// The runtime destroyed a surface (compositor close, `RemoveWindow`).
    SurfaceClosed(Id),
    /// Leave for good: close every surface, drop the tray item, exit.
    Quit,
    DismissToastNoop,
    Close(Id),
    DismissToast,
}

pub fn run(overlay_only: bool) -> Result<(), iced_exwlshell::Error> {
    // Load the bundled fonts straight into the font system the widgets shape
    // with, so family lookups resolve no matter how the shell wires fonts.
    {
        let mut fs = iced::advanced::graphics::text::font_system().write().expect("font system");
        fs.load_font(std::borrow::Cow::Borrowed(theme::font::SANS_BYTES));
        fs.load_font(std::borrow::Cow::Borrowed(theme::font::MONO_BYTES));
    }
    daemon(move || App::boot(overlay_only), App::namespace, App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .style(App::style)
        .subscription(App::subscription)
        .font(theme::font::SANS_BYTES)
        .font(theme::font::MONO_BYTES)
        .default_font(theme::font::SANS)
        .settings(Settings {
            layer_settings: LayerShellSettings { start_mode: StartMode::Background, ..Default::default() },
            ..Default::default()
        })
        .run()
}

impl App {
    fn boot(overlay_only: bool) -> (Self, Task<Message>) {
        let (config, config_notice) = crate::config_store::load();
        crate::telemetry::set_rate(config.telemetry_hz);
        let (palette, theme_name) = theme::load();
        tracing::info!(theme = %theme_name, "using Omarchy theme");
        let app = Self {
            palette,
            theme_name,
            page: Page::Dashboard,
            config,
            snapshot: None,
            snapshot_at: None,
            hist: History::default(),
            inventory: None,
            model: None,
            inventory_title: "detecting hardware…".into(),
            controller_ready: false,
            surfaces: HashMap::new(),
            overlay_only,
            toast: config_notice.map(|n| (n, false)),
            toast_at: None,
            // Filled from the first telemetry frame: nothing is read here on the UI thread.
            cpu_edit: Default::default(),
            cpu_synced: false,
            gpu_edit: oma_hw::nvidia::NvidiaControl::default(),
            gpu_dirty: false,
            nvidia_info: None,
            cooling_sel: None,
            cc: None,
            cc_connected: false,
            cc_modes: Vec::new(),
            fan_backend: None,
            fan_engine: oma_hw::fanengine::FanEngine::new(std::time::Duration::from_millis(500)),
            fan_errors: 0,
            rgb_server: false,
            rgb_devices: Vec::new(),
            rgb_sel: 0,
            rgb_hex: String::new(),
            rgb_thermal: false,
            rgb_installed: oma_hw::rgb::installed(),
            lights: Default::default(),
            light_sel: None,
            slash_drag: None,
            profile_sel: None,
            auto_state: AutoState::default(),
            helper_log: String::new(),
            asus: AsusState::default(),
            t0: std::time::Instant::now(),
            now: std::time::Instant::now(),
            smooth: Smooth::default(),
            tray: None,
            tray_hosted: false,
            tray_synced: tray::TrayState::default(),
            overlay_phase: None,
            quitting: false,
            theme_fade: None,
            ppd: None,
            quick_charge: None,
            attr_drag: None,
            gfx_confirm: None,
            screen_h: None,
            overlay_h: 0,
            overlay_anchor: Anchor::Right | Anchor::Top,
            apply_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            coord: crate::coordinator::Coordinator::default(),
            last_reapply: None,
            dgpu_wait: false,
        };
        let inv = Task::perform(async { Arc::new(oma_hw::capture::gather().await) }, Message::Hardware);
        let ctl = Task::perform(async { oma_hw::helper::Controller::connect().await.has_helper() }, Message::Controller);
        // Only when the dGPU is awake: NVML would wake a sleeping one.
        let nvi = Task::perform(async { tokio::task::spawn_blocking(|| (oma_hw::nvidia::awake() && telemetry::dgpu_open_ok()).then(|| oma_hw::nvidia::NvidiaGpu::open(0).and_then(|g| g.info()).ok().map(Arc::new)).flatten()).await.unwrap_or(None) }, Message::NvidiaInfo);
        let open = if overlay_only { Task::none() } else { Task::done(Message::OpenWindow) };
        let cc = app.probe_cc(std::time::Duration::ZERO);
        // The OpenRGB probe is a TCP connect: off the UI thread, like every other probe.
        let asus = Task::perform(crate::pages::asus::load(), Message::AsusLoaded);
        // iced_exwlshell's daemon does not apply `Settings::fonts`; the runtime font
        // action does, so bundle-load through tasks (fallback fonts otherwise).
        let fonts = Task::batch([
            iced::font::load(theme::font::SANS_BYTES).map(|r| Message::FontLoaded(r.is_ok())),
            iced::font::load(theme::font::MONO_BYTES).map(|r| Message::FontLoaded(r.is_ok())),
        ]);
        (app, Task::batch([fonts, inv, ctl, nvi, cc, asus, load_ppd(), probe_graphics(), open]))
    }

    fn namespace() -> String {
        "omaasus".into()
    }

    fn title(&self, id: Id) -> Option<String> {
        Some(match self.surfaces.get(&id) {
            Some(Surface::Overlay) => "OmaAsus overlay".into(),
            _ => "OmaAsus".into(),
        })
    }

    fn theme(&self, _id: Id) -> Theme {
        theme::iced_theme(&self.palette)
    }

    fn style(&self, _t: &Theme) -> iced::theme::Style {
        iced::theme::Style { background_color: iced::Color::TRANSPARENT, text_color: self.palette.text }
    }

    fn subscription(&self) -> Subscription<Message> {
        let anim = if self.surfaces.is_empty() { Subscription::none() } else { iced::time::every(std::time::Duration::from_millis(33)).map(Message::Tick) };
        let tray = if self.config.tray_enabled { Subscription::run(tray::stream).map(Message::Tray) } else { Subscription::none() };
        Subscription::batch([
            Subscription::run(telemetry::stream).map(Message::Telemetry),
            Subscription::run(rgb_stream).map(Message::RgbLink),
            Subscription::run(ipc::stream).map(Message::Ipc),
            Subscription::run(crate::automation::stream).map(Message::Auto),
            Subscription::run(crate::events::stream).map(Message::System),
            // Cooling safety runs on its own clock: a sampler that stops
            // delivering frames must not stop the engine's fallbacks.
            iced::time::every(std::time::Duration::from_millis(500)).map(Message::FanSafety),
            // The desktop's theme, as Omarchy applies it.
            Subscription::run(theme::watch).map(|(p, name)| Message::ThemeChanged(p, name)),
            tray,
            iced::window::close_events().map(Message::SurfaceClosed),
            anim,
        ])
    }

    pub fn active_profile(&self) -> Option<&oma_hw::Profile> {
        self.config.profile(self.config.active_profile)
    }

    pub fn effective_fan_owner(&self) -> FanOwner {
        let cc_running = self.inventory.as_ref().map(|i| i.daemons.coolercontrold).unwrap_or(false);
        match self.config.fan_owner {
            FanOwner::Auto => {
                if cc_running { FanOwner::CoolerControl } else { FanOwner::OmaAsus }
            }
            o => o,
        }
    }

    pub fn fan_engine_duty(&self, t: &FanTarget) -> Option<f64> {
        self.fan_engine.current(t)
    }

    fn cc_client(&self) -> oma_hw::coolercontrol::CoolerControl {
        self.cc.clone().unwrap_or_else(|| oma_hw::coolercontrol::CoolerControl::new(&self.config.coolercontrol.url, self.config.coolercontrol.password.clone()))
    }

    /// Whether CoolerControl's API answers, and its Modes, `after` a pause:
    /// its daemon listens a moment after its unit reports active.
    fn probe_cc(&self, after: std::time::Duration) -> Task<Message> {
        let cc = self.cc_client();
        Task::perform(
            async move {
                tokio::time::sleep(after).await;
                if !cc.handshake().await {
                    return (false, Vec::new());
                }
                match cc.modes().await {
                    Ok(m) => (true, m),
                    Err(_) => (false, Vec::new()),
                }
            },
            |(ok, modes)| Message::CcReady(ok, modes),
        )
    }

    fn edit_cooling(&mut self, f: impl FnOnce(&mut oma_hw::profile::CoolingSettings)) {
        let id = self.config.active_profile;
        if let Some(pr) = self.config.profiles.iter_mut().find(|p| p.id == id) {
            f(&mut pr.cooling);
        }
        crate::config_store::save(&self.config);
    }

    /// Apply a profile through the pipeline; a newer apply supersedes this one.
    fn start_apply(&mut self, id: uuid::Uuid, origin: crate::apply::Origin) -> Task<Message> {
        let Some(pr) = self.config.profile(id).cloned() else { return Task::none() };
        let request = crate::coordinator::Request { id, name: pr.name.clone(), origin };
        match self.coord.request(request.clone(), std::time::Instant::now()) {
            crate::coordinator::Action::Start(generation) => self.launch_apply(generation, request),
            crate::coordinator::Action::Queued => {
                // The running apply sees the newer generation and stops at its
                // next checkpoint; this one starts when it reports back.
                self.apply_generation.store(self.coord.latest_generation(), std::sync::atomic::Ordering::SeqCst);
                Task::none()
            }
        }
    }

    /// Run one apply under `generation`: the saved preference changes now, the
    /// hardware result comes back as `ProfileApplied`.
    fn launch_apply(&mut self, generation: u64, request: crate::coordinator::Request) -> Task<Message> {
        let Some(pr) = self.config.profile(request.id).cloned() else {
            // Deleted while it waited: report so, and let the next queued one run.
            let mut report = crate::apply::Report::default();
            report.failed.push(format!("profile {} no longer exists", request.name));
            return match self.coord.finished(generation, report, std::time::Instant::now()) {
                crate::coordinator::Finish::StartNext(generation, next) => self.launch_apply(generation, next),
                _ => Task::none(),
            };
        };
        self.config.active_profile = request.id;
        crate::config_store::save(&self.config);
        // Re-send the new profile's outputs; ones it drops are released by the engine.
        self.fan_engine.invalidate();
        if let Some(be) = &self.fan_backend {
            be.set_power_mode(pr.cpu.power_mode.clone());
        }
        self.apply_generation.store(generation, std::sync::atomic::Ordering::SeqCst);
        let cc = (self.effective_fan_owner() == FanOwner::CoolerControl).then(|| {
            let mode = pr.cc_mode.clone().map(|uid| {
                let name = self.cc_modes.iter().find(|m| m.uid == uid).map(|m| m.name.clone()).unwrap_or_else(|| uid.clone());
                (uid, name)
            });
            crate::apply::CoolerControlFans { client: self.cc_client(), mode }
        });
        let cx = crate::apply::Context { inv: self.inventory.clone(), model: self.model.clone(), generation: self.apply_generation.clone(), this: generation, cc };
        let (name, origin) = (request.name.clone(), request.origin);
        Task::perform(crate::apply::apply_profile(pr, cx), move |report| Message::ProfileApplied(generation, name.clone(), origin, report))
    }

    /// The power mode is `live` now, set outside a profile apply: a keyboard
    /// shortcut, `powerprofilesctl`, a bar widget, or this app's own power-mode
    /// buttons. The profile that carries the mode follows, so the rest of what
    /// the machine runs (CPU, GPU, fans, lighting) matches the mode it is in.
    /// An apply's own write reports here too; while one runs, the mode is its
    /// business, and it stays where the apply leaves it.
    fn follow_power_mode(&mut self, live: &str) -> Task<Message> {
        if matches!(self.coord.status(std::time::Instant::now()), Some(crate::coordinator::Status::Applying { .. })) {
            return Task::none();
        }
        let Some(choices) = self.model.as_ref().map(|m| m.controls.power_modes.clone()) else { return Task::none() };
        if self.active_profile().is_some_and(|p| p.carries_power_mode(live, &choices)) {
            return Task::none();
        }
        match self.config.profile_for_power_mode(live, &choices).map(|p| (p.id, p.name.clone())) {
            Some((id, name)) => {
                tracing::info!(mode = live, profile = %name, "power mode changed outside a profile; following it");
                self.start_apply(id, crate::apply::Origin::PowerMode)
            }
            None => {
                tracing::info!(mode = live, "power mode changed outside a profile; no profile carries it");
                Task::none()
            }
        }
    }

    /// Point count of a curve the firmware runs on its own sensor (asusd: 8).
    fn fixed_curve_points(&self, t: &FanTarget) -> Option<usize> {
        let spec = self.model.as_ref()?.fan(t.as_str())?.caps.firmware_curve.clone()?;
        (spec.temp == oma_hw::model::CurveTemp::Firmware).then_some(spec.points as usize)
    }

    /// What the firmware stores for an output under the active profile's power mode.
    fn stored_firmware_curve(&self, t: &FanTarget) -> Option<oma_hw::profile::FanCurve> {
        let m = self.model.as_ref()?;
        let out = m.fan(t.as_str())?;
        let wanted = self.active_profile().and_then(|p| p.cpu.power_mode.clone()).or_else(|| m.controls.power_mode.clone())?;
        let stored = out.firmware_curves.get(oma_hw::profile::match_power_mode(&wanted, &m.controls.power_modes)?)?;
        Some(oma_hw::profile::FanCurve { points: stored.points.clone(), source: temp_source_for(Some(out.caps.curve_input)), hysteresis_c: 0.0, min_duty: 0.0, ramp_s: 0.0 })
    }

    fn selected_target(&self) -> Option<FanTarget> {
        self.cooling_sel.clone().or_else(|| {
            let model = self.model.as_deref()?;
            crate::fans::FanBackend::available(model, self.snapshot.as_deref()).first().map(|a| a.target.clone())
        })
    }

    fn update_cooling(&mut self, m: CoolingMsg) -> Task<Message> {
        let Some(target) = self.selected_target() else { return Task::none() };
        // A new curve follows what the model says this output cools.
        let default_source = temp_source_for(self.model.as_ref().and_then(|m| m.fan(target.as_str())).map(|f| f.caps.curve_input));
        match m {
            CoolingMsg::Select(t) => self.cooling_sel = Some(t),
            CoolingMsg::Owner(o) => {
                self.config.fan_owner = o;
                crate::config_store::save(&self.config);
                // Hand every output back before another owner (or firmware) takes over.
                let cmds = self.fan_engine.release_all(std::time::Instant::now());
                return self.dispatch_fan_cmds(cmds);
            }
            CoolingMsg::Mode(t, kind) => {
                let src = default_source.clone();
                // A firmware curve starts from what the firmware stores for this power mode.
                let stored = self.stored_firmware_curve(&t);
                let points = self.fixed_curve_points(&t);
                self.edit_cooling(|c| {
                    let existing_curve = match c.get(&t) {
                        Some(FanMode::Curve(cv)) | Some(FanMode::HardwareCurve(cv)) => Some(cv.clone()),
                        _ => None,
                    };
                    let mode = match kind {
                        "fixed" => FanMode::Fixed(50.0),
                        "curve" => FanMode::Curve(existing_curve.clone().unwrap_or_else(|| crate::pages::cooling::preset("balanced", src.clone()))),
                        "hw" => {
                            let mut cv = existing_curve.or(stored).unwrap_or_else(|| crate::pages::cooling::preset("balanced", src));
                            if let Some(n) = points.filter(|n| cv.points.len() != *n) {
                                cv.points = cv.resample(n);
                            }
                            FanMode::HardwareCurve(cv)
                        }
                        _ => FanMode::Auto,
                    };
                    c.set(t, mode);
                });
            }
            CoolingMsg::Fixed(v) => self.edit_cooling(|c| c.set(target, FanMode::Fixed(v))),
            CoolingMsg::Curve(ev) => {
                use crate::widgets::curve::CurveEvent;
                // Firmware curves keep their point count: points move, none are added or removed.
                let fixed = self.fixed_curve_points(&target);
                self.edit_cooling(|c| {
                    if let Some(FanMode::Curve(cv)) | Some(FanMode::HardwareCurve(cv)) = c.fans.iter_mut().find(|f| f.target == target).map(|f| &mut f.mode) {
                        match ev {
                            CurveEvent::Move(i, t, d) => {
                                if let Some(pt) = cv.points.get_mut(i) {
                                    *pt = (t, d);
                                }
                            }
                            CurveEvent::Add(t, d) => {
                                if fixed.is_none() && cv.points.len() < 12 {
                                    cv.points.push((t, d));
                                    cv.points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                                }
                            }
                            CurveEvent::Remove(i) => {
                                if fixed.is_none() && cv.points.len() > 2 && i < cv.points.len() {
                                    cv.points.remove(i);
                                }
                            }
                            CurveEvent::Commit => cv.points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap()),
                        }
                    }
                });
            }
            CoolingMsg::Source(src) => self.edit_cooling(|c| {
                if let Some(FanMode::Curve(cv)) | Some(FanMode::HardwareCurve(cv)) = c.fans.iter_mut().find(|f| f.target == target).map(|f| &mut f.mode) {
                    cv.source = src;
                }
            }),
            CoolingMsg::MinDuty(v) => self.edit_cooling(|c| {
                if let Some(FanMode::Curve(cv)) | Some(FanMode::HardwareCurve(cv)) = c.fans.iter_mut().find(|f| f.target == target).map(|f| &mut f.mode) {
                    cv.min_duty = v;
                }
            }),
            CoolingMsg::Ramp(v) => self.edit_cooling(|c| {
                if let Some(FanMode::Curve(cv)) | Some(FanMode::HardwareCurve(cv)) = c.fans.iter_mut().find(|f| f.target == target).map(|f| &mut f.mode) {
                    cv.ramp_s = v;
                }
            }),
            CoolingMsg::Hysteresis(v) => self.edit_cooling(|c| {
                if let Some(FanMode::Curve(cv)) | Some(FanMode::HardwareCurve(cv)) = c.fans.iter_mut().find(|f| f.target == target).map(|f| &mut f.mode) {
                    cv.hysteresis_c = v;
                }
            }),
            CoolingMsg::Preset(name) => {
                let src = default_source;
                let points = self.fixed_curve_points(&target);
                self.edit_cooling(|c| {
                    let hw = matches!(c.get(&target), Some(FanMode::HardwareCurve(_)));
                    let mut cv = crate::pages::cooling::preset(name, src);
                    if let Some(FanMode::Curve(old)) | Some(FanMode::HardwareCurve(old)) = c.get(&target)
                        && !matches!(name, "coolant" | "pump")
                    {
                        cv.source = old.source.clone();
                    }
                    if let Some(n) = points {
                        cv.points = cv.resample(n);
                    }
                    c.set(target, if hw { FanMode::HardwareCurve(cv) } else { FanMode::Curve(cv) });
                });
            }
            CoolingMsg::CcMode(uid) => {
                let id = self.config.active_profile;
                if let Some(pr) = self.config.profiles.iter_mut().find(|p| p.id == id) {
                    pr.cc_mode = if pr.cc_mode.as_deref() == Some(uid.as_str()) { None } else { Some(uid.clone()) };
                }
                crate::config_store::save(&self.config);
                let cc = self.cc_client();
                return Task::perform(async move { cc.activate_mode(&uid).await.map(|_| "CoolerControl mode activated".to_string()).map_err(|e| e.to_string()) }, Message::Applied);
            }
            CoolingMsg::CcRefresh => {
                let cc = self.cc_client();
                return Task::perform(async move { match cc.modes().await { Ok(m) => (true, m), Err(_) => (false, Vec::new()) } }, |(ok, m)| Message::CcReady(ok, m));
            }
        }
        Task::none()
    }

    fn update_quick(&mut self, m: crate::pages::quick::QuickMsg) -> Task<Message> {
        use crate::pages::quick::QuickMsg;
        match m {
            QuickMsg::OpenPage(pg) => {
                self.page = pg;
                Task::batch([Task::done(Message::OpenWindow), self.close_overlay()])
            }
            QuickMsg::ChargeDrag(v) => {
                self.quick_charge = Some(v);
                Task::none()
            }
            QuickMsg::ChargeCommit => match self.quick_charge.take() {
                Some(v) => {
                    self.asus.charge_limit = Some(v as u8);
                    self.update_asus(AsusMsg::ChargeLimit(v))
                }
                None => Task::none(),
            },
            QuickMsg::PowerProfile(name) => Task::perform(
                async move {
                    let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                    oma_hw::ppd::set_active(&c, &name).await.map_err(|e| e.to_string())?;
                    Ok(format!("Power profile: {name}"))
                },
                Message::Applied,
            )
            .chain(load_ppd()),
            QuickMsg::KbdBrightness(v) => self.update_asus(AsusMsg::KbdBrightness(v)),
        }
    }

    /// Wait (bounded) for the dGPU to be ready, then apply the active profile's
    /// NVIDIA settings alone; never more than one wait at a time.
    fn nvidia_when_ready(&mut self) -> Task<Message> {
        if self.dgpu_wait || !self.active_profile().is_some_and(|p| p.gpu.nvidia.is_some()) {
            return Task::none();
        }
        self.dgpu_wait = true;
        Task::perform(
            async {
                // A switch (up to its 90 s wait) and the settle after it.
                for _ in 0..120 {
                    if telemetry::dgpu_open_ok() {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                false
            },
            Message::DgpuReady,
        )
    }

    /// Why the graphics mode can't switch to `mode` right now, if anything.
    fn gfx_blocker(&self, mode: oma_hw::supergfx::GfxMode) -> Option<String> {
        let busy = self.snapshot.as_ref().and_then(|s| s.nvidia.as_ref()).is_some_and(|n| n.util_gpu.unwrap_or(0) > 10 || n.process_count.unwrap_or(0) > 0);
        oma_hw::supergfx::switch_blocker(mode, busy, &oma_hw::supergfx::dgpu_displays())
    }

    fn update_asus(&mut self, m: AsusMsg) -> Task<Message> {
        use oma_hw::asusd::PlatformProxy;
        let reload = || Task::perform(crate::pages::asus::load(), Message::AsusLoaded);
        let run = |f: std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>| Task::perform(f, Message::Applied).chain(Task::perform(crate::pages::asus::load(), Message::AsusLoaded));
        match m {
            AsusMsg::Refresh => reload(),
            AsusMsg::KbdBrightness(v) => {
                let Some(path) = self.asus.kbd.as_ref().map(|k| k.path.clone()) else { return Task::none() };
                if let Some(k) = &mut self.asus.kbd {
                    k.brightness = v;
                }
                // The Lighting page shows the same keyboard.
                for d in self.model.iter().flat_map(|m| &m.lighting) {
                    if matches!(&d.backend, oma_hw::model::LightingBackend::AsusdAura { path: p } if *p == path)
                        && let Some(oma_hw::lighting::LightState::Aura { level, .. }) = self.lights.get_mut(d.id.as_str())
                    {
                        *level = v;
                    }
                }
                run(Box::pin(async move {
                    let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                    oma_hw::asusd::AuraProxy::builder(&c).path(path.as_str()).map_err(|e| e.to_string())?.build().await.map_err(|e| e.to_string())?.set_brightness(v).await.map_err(|e| e.to_string())?;
                    Ok(format!("Keyboard brightness {v}"))
                }))
            }
            AsusMsg::Profile(pp) => run(Box::pin(async move {
                let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                PlatformProxy::new(&c).await.map_err(|e| e.to_string())?.set_platform_profile(pp as u32).await.map_err(|e| e.to_string())?;
                Ok(format!("Platform profile: {}", pp.label()))
            })),
            AsusMsg::NextProfile => run(Box::pin(async move {
                let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                PlatformProxy::new(&c).await.map_err(|e| e.to_string())?.next_platform_profile().await.map_err(|e| e.to_string())?;
                Ok("Platform profile cycled".into())
            })),
            AsusMsg::PptGroup(b) => run(Box::pin(async move {
                let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                PlatformProxy::new(&c).await.map_err(|e| e.to_string())?.set_enable_ppt_group(b).await.map_err(|e| e.to_string())?;
                Ok(format!("PPT group {}", if b { "enabled" } else { "disabled" }))
            })),
            AsusMsg::ChargeLimit(v) => run(Box::pin(async move {
                let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                PlatformProxy::new(&c).await.map_err(|e| e.to_string())?.set_charge_control_end_threshold(v as u8).await.map_err(|e| e.to_string())?;
                Ok(format!("Charge limit {v:.0}%"))
            })),
            AsusMsg::Attr(name, v) => {
                let v = v.round();
                // Shown at once; the reload after the write confirms it.
                if let Some(a) = self.asus.attrs.iter_mut().find(|a| a.name == name) {
                    a.current = Some(v as i64);
                }
                let shown = match oma_hw::asusd::attr_unit(&name) {
                    Some(u) => format!("{v:.0} {u}"),
                    None => format!("{v:.0}"),
                };
                run(Box::pin(async move {
                    let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                    oma_hw::asusd::set_armoury_attr(&c, &name, v as i32).await.map_err(|e| e.to_string())?;
                    Ok(format!("{}: {shown}", oma_hw::asusd::attr_label(&name)))
                }))
            }
            AsusMsg::AttrDrag(name, v) => {
                self.attr_drag = Some((name, v));
                Task::none()
            }
            AsusMsg::AttrRelease => match self.attr_drag.take() {
                Some((name, v)) => self.update_asus(AsusMsg::Attr(name, v)),
                None => Task::none(),
            },
            AsusMsg::AttrRestore(name) => run(Box::pin(async move {
                let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                let path = format!("/xyz/ljones/asus_armoury/{name}");
                oma_hw::asusd::AsusArmouryProxy::builder(&c).path(path).map_err(|e| e.to_string())?.build().await.map_err(|e| e.to_string())?.restore_default().await.map_err(|e| e.to_string())?;
                Ok(format!("{} restored", oma_hw::asusd::attr_label(&name)))
            })),
            AsusMsg::GfxMode(mode) => {
                // Only real changes, and never out from under a display or work on the dGPU.
                if self.asus.gfx.as_ref().map(|g| g.mode) == Some(mode) {
                    self.gfx_confirm = None;
                    return Task::none();
                }
                if let Some(why) = self.gfx_blocker(mode) {
                    self.toast = Some((format!("Graphics → {}: {why}", mode.label()), false));
                    self.toast_at = Some(std::time::Instant::now());
                    return Task::none();
                }
                // The page asks first, saying what the switch involves.
                self.gfx_confirm = Some(mode);
                Task::none()
            }
            AsusMsg::GfxCancel => {
                self.gfx_confirm = None;
                Task::none()
            }
            AsusMsg::GfxConfirm => {
                let Some(mode) = self.gfx_confirm.take() else { return Task::none() };
                // Checked again: something may have started on the dGPU while the question was open.
                if let Some(why) = self.gfx_blocker(mode) {
                    self.toast = Some((format!("Graphics → {}: {why}", mode.label()), false));
                    self.toast_at = Some(std::time::Instant::now());
                    return Task::none();
                }
                // supergfxd kills whatever holds the dGPU while it switches: let go first.
                telemetry::hold_off_dgpu(DGPU_HOLD_OFF);
                run(Box::pin(async move {
                    // The sampler lets go within a tick.
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                    let action = oma_hw::supergfx::set_mode(&c, mode).await.map_err(|e| e.to_string())?;
                    Ok(format!("Graphics → {}: {}", mode.label(), action.label()))
                }))
            }
        }
    }

    fn update_settings(&mut self, m: SettingsMsg) -> Task<Message> {
        match m {
            SettingsMsg::InstallHelper => {
                return Task::perform(
                    async {
                        let (bin, data, script) = crate::install::locate().map_err(|e| e.to_string())?;
                        let out = tokio::process::Command::new("pkexec").arg("bash").arg(&script).arg(&bin).arg(&data).output().await.map_err(|e| e.to_string())?;
                        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                        if out.status.success() { Ok(text) } else { Err(text) }
                    },
                    Message::HelperInstalled,
                );
            }
            SettingsMsg::RecheckHelper => return Task::perform(async { oma_hw::helper::Controller::connect().await.has_helper() }, Message::Controller),
            SettingsMsg::CcUrl(u) => self.config.coolercontrol.url = u,
            SettingsMsg::CcPassword(pw) => self.config.coolercontrol.password = if pw.is_empty() { None } else { Some(pw) },
            SettingsMsg::CcTest => {
                crate::config_store::save(&self.config);
                self.cc = None;
                let cc = self.cc_client();
                return Task::perform(
                    async move {
                        if !cc.handshake().await {
                            return (false, Vec::new());
                        }
                        match cc.login().await.and(cc.modes().await) {
                            Ok(m) => (true, m),
                            Err(_) => (false, Vec::new()),
                        }
                    },
                    |(ok, m)| Message::CcReady(ok, m),
                );
            }
            SettingsMsg::OverlayAnchor(a) => self.config.overlay.anchor = a,
            SettingsMsg::OverlayWidth(w) => self.config.overlay.width = w as u32,
            SettingsMsg::OverlayHeight(h) => self.config.overlay.height = if h < 400.0 { 0 } else { h as u32 },
            SettingsMsg::OverlayMargin(v) => self.config.overlay.margin = v as u32,
            SettingsMsg::OverlayOpacity(v) => self.config.overlay.opacity = v as f32,
            SettingsMsg::HudToggle(b) => self.config.overlay.hud_enabled = b,
            SettingsMsg::TrayToggle(b) => {
                self.config.tray_enabled = b;
                if !b
                    && let Some(h) = self.tray.take()
                {
                    crate::config_store::save(&self.config);
                    return Task::future(async move { h.shutdown().await }).discard();
                }
            }
            SettingsMsg::TelemetryHz(v) => {
                self.config.telemetry_hz = v as u32;
                crate::telemetry::set_rate(self.config.telemetry_hz);
            }
            SettingsMsg::OledText => {
                let snap = self.snapshot.clone();
                return Task::perform(
                    async move {
                        let path = oma_hw::livedash::hid_path().ok_or("LiveDash OLED not found")?;
                        let ctl = oma_hw::helper::Controller::connect().await;
                        // A reading nobody has is shown as such on the OLED, not as 0.
                        let show = |t: Option<f64>| t.map(|t| format!("{t:.0}C")).unwrap_or_else(|| "--".into());
                        let cpu = show(snap.as_ref().and_then(|s| s.cpu.tctl_c));
                        let gpu = show(snap.as_ref().and_then(|s| s.gpu()).and_then(|g| g.temp_c));
                        ctl.hid_write(&path, &oma_hw::livedash::mode_text()).await.map_err(|e| e.to_string())?;
                        ctl.hid_write(&path, &oma_hw::livedash::text("CPU / GPU", &format!("{cpu}  {gpu}"))).await.map_err(|e| e.to_string())?;
                        Ok("OLED text sent".to_string())
                    },
                    Message::Applied,
                );
            }
            SettingsMsg::OledHwMonitor => {
                return Task::perform(
                    async move {
                        let path = oma_hw::livedash::hid_path().ok_or("LiveDash OLED not found")?;
                        let ctl = oma_hw::helper::Controller::connect().await;
                        ctl.hid_write(&path, &oma_hw::livedash::mode_hw_monitor()).await.map_err(|e| e.to_string())?;
                        Ok("OLED back to hardware monitor".to_string())
                    },
                    Message::Applied,
                );
            }
        }
        crate::config_store::save(&self.config);
        Task::none()
    }

    fn update_profiles(&mut self, m: ProfilesMsg) -> Task<Message> {
        match m {
            ProfilesMsg::Select(id) => self.profile_sel = Some(id),
            ProfilesMsg::Apply(id) => return Task::done(Message::ApplyProfile(id)),
            ProfilesMsg::New => {
                let mut pr = oma_hw::Profile::new("New profile");
                if let Some(base) = self.active_profile() {
                    pr.accent = base.accent;
                }
                self.profile_sel = Some(pr.id);
                self.config.profiles.push(pr);
                crate::config_store::save(&self.config);
            }
            ProfilesMsg::Duplicate(id) => {
                if let Some(src) = self.config.profile(id).cloned() {
                    let mut pr = src;
                    pr.id = uuid::Uuid::new_v4();
                    pr.name = format!("{} copy", pr.name);
                    pr.builtin = false;
                    self.profile_sel = Some(pr.id);
                    self.config.profiles.push(pr);
                    crate::config_store::save(&self.config);
                }
            }
            ProfilesMsg::Delete(id) => {
                if self.config.profiles.len() > 1 {
                    self.config.profiles.retain(|p| p.id != id);
                    let fallback = self.config.profiles[0].id;
                    if self.config.active_profile == id {
                        self.config.active_profile = fallback;
                    }
                    if self.config.default_profile == id {
                        self.config.default_profile = fallback;
                    }
                    self.config.rules.retain(|r| r.profile != id);
                    self.profile_sel = Some(fallback);
                    crate::config_store::save(&self.config);
                }
            }
            ProfilesMsg::SetDefault(id) => {
                self.config.default_profile = id;
                crate::config_store::save(&self.config);
            }
            ProfilesMsg::Rename(name) => {
                let id = self.profile_sel.unwrap_or(self.config.active_profile);
                if let Some(pr) = self.config.profiles.iter_mut().find(|p| p.id == id) {
                    pr.name = name;
                }
                crate::config_store::save(&self.config);
            }
            ProfilesMsg::Accent(c) => {
                let id = self.profile_sel.unwrap_or(self.config.active_profile);
                if let Some(pr) = self.config.profiles.iter_mut().find(|p| p.id == id) {
                    pr.accent = c;
                }
                crate::config_store::save(&self.config);
            }
        }
        Task::none()
    }

    fn update_automation(&mut self, m: AutomationMsg) -> Task<Message> {
        use oma_hw::profile::{Rule, Trigger};
        match m {
            AutomationMsg::Mode(mode) => {
                self.config.mode = mode;
                crate::config_store::save(&self.config);
                return self.auto_evaluate();
            }
            AutomationMsg::DefaultProfile(id) => self.config.default_profile = id,
            AutomationMsg::Toggle(id, on) => {
                if let Some(r) = self.config.rules.iter_mut().find(|r| r.id == id) {
                    r.enabled = on;
                }
            }
            AutomationMsg::Delete(id) => self.config.rules.retain(|r| r.id != id),
            AutomationMsg::Add(kind) => {
                let gaming = self.config.profile_by_role(ProfileRole::Performance).map(|p| p.id).unwrap_or(self.config.active_profile);
                let silent = self.config.profile_by_role(ProfileRole::Quiet).map(|p| p.id).unwrap_or(self.config.default_profile);
                let (name, trigger, profile, prio) = match kind {
                    "gamemode" => ("GameMode active", Trigger::GameMode, gaming, 100),
                    "fullscreen" => ("Fullscreen game", Trigger::FullscreenGame, gaming, 50),
                    "class" => ("Window class", Trigger::WindowClass("steam_app_*".into()), gaming, 60),
                    "process" => ("Process", Trigger::Process("gamescope".into()), gaming, 60),
                    "cpuhot" => ("CPU hot", Trigger::CpuHot { above_c: 88.0, for_s: 20 }, silent, 150),
                    "gpuhot" => ("GPU hot", Trigger::GpuHot { above_c: 83.0, for_s: 20 }, silent, 150),
                    _ => ("Night hours", Trigger::Time { from: (23, 0), to: (7, 0) }, silent, 10),
                };
                self.config.rules.push(Rule { id: uuid::Uuid::new_v4(), name: name.into(), enabled: true, trigger, profile, priority: prio, hold_s: 20 });
            }
            AutomationMsg::SetProfile(id, pid) => {
                if let Some(r) = self.config.rules.iter_mut().find(|r| r.id == id) {
                    r.profile = pid;
                }
            }
            AutomationMsg::Priority(id, v) => {
                if let Some(r) = self.config.rules.iter_mut().find(|r| r.id == id) {
                    r.priority = v as i32;
                }
            }
            AutomationMsg::Hold(id, v) => {
                if let Some(r) = self.config.rules.iter_mut().find(|r| r.id == id) {
                    r.hold_s = v as u32;
                }
            }
            AutomationMsg::Text(id, s) => {
                if let Some(r) = self.config.rules.iter_mut().find(|r| r.id == id) {
                    match &mut r.trigger {
                        Trigger::WindowClass(v) | Trigger::Process(v) => *v = s,
                        _ => {}
                    }
                }
            }
            AutomationMsg::Threshold(id, v) => {
                if let Some(r) = self.config.rules.iter_mut().find(|r| r.id == id) {
                    match &mut r.trigger {
                        Trigger::CpuHot { above_c, .. } | Trigger::GpuHot { above_c, .. } => *above_c = v,
                        _ => {}
                    }
                }
            }
        }
        crate::config_store::save(&self.config);
        Task::none()
    }

    /// Evaluate automation rules against the current state; may apply a profile.
    fn auto_evaluate(&mut self) -> Task<Message> {
        if self.config.mode != oma_hw::profile::Mode::Automatic {
            self.auto_state.matched_rule = None;
            return Task::none();
        }
        let snap = self.snapshot.clone();
        let now = std::time::Instant::now();
        let decision = crate::automation::decide(&self.config, &mut self.auto_state, snap.as_deref(), now);
        match decision {
            Some(target) if target != self.config.active_profile => {
                tracing::info!(?target, "automation switching profile");
                Task::done(Message::AutoApply(target))
            }
            _ => Task::none(),
        }
    }

    /// Ask, off the UI thread, whether an OpenRGB server answers.
    fn probe_rgb(then: fn(bool) -> Message) -> Task<Message> {
        Task::perform(async { tokio::task::spawn_blocking(oma_hw::rgb::server_running).await.unwrap_or(false) }, then)
    }

    /// Send the active profile's OpenRGB lighting to the devices just listed.
    fn restore_openrgb_lighting(&self) -> Task<Message> {
        let zones: Vec<(String, oma_hw::profile::LightingMode)> = self
            .active_profile()
            .map(|pr| {
                pr.lighting
                    .zones
                    .iter()
                    .filter(|(_, m)| !matches!(m, oma_hw::profile::LightingMode::Thermal { .. }))
                    .filter_map(|(k, m)| k.strip_prefix("openrgb:").map(|n| (n.to_string(), m.clone())))
                    .collect()
            })
            .unwrap_or_default();
        if zones.is_empty() {
            tracing::info!("the active profile sets no OpenRGB lighting; devices left as they are");
            return Task::none();
        }
        let devices = self.rgb_devices.clone();
        Task::perform(
            async move {
                let mut failed = Vec::new();
                for (name, mode) in zones {
                    if let Err(e) = oma_hw::lighting::apply_openrgb(&devices, &name, &mode).await {
                        failed.push(e);
                    }
                }
                failed
            },
            Message::LightingRestored,
        )
    }

    fn rgb_refresh() -> Task<Message> {
        Task::perform(async { oma_hw::rgb::devices().await.map_err(|e| e.to_string()) }, Message::RgbDevices)
    }

    /// Read what the model's lighting devices show.
    fn load_lights(&self) -> Task<Message> {
        let Some(m) = self.model.clone().filter(|m| !m.lighting.is_empty()) else { return Task::none() };
        Task::perform(async move { oma_hw::lighting::read_all(&m.lighting).await }, |s| Message::Lighting(LightingMsg::Loaded(s)))
    }

    /// Keep lighting for a device in the active profile.
    fn remember_light(&mut self, key: String, mode: oma_hw::profile::LightingMode, brightness: Option<u8>) {
        let id = self.config.active_profile;
        if let Some(pr) = self.config.profiles.iter_mut().find(|p| p.id == id) {
            if let Some(b) = brightness {
                pr.lighting.device_brightness.insert(key.clone(), b);
            }
            pr.lighting.zones.insert(key, mode);
            crate::config_store::save(&self.config);
        }
    }

    fn light_device(&self, id: &str) -> Option<oma_hw::model::LightingDevice> {
        self.model.as_ref()?.lighting.iter().find(|d| d.id.as_str() == id).cloned()
    }

    /// Show `mode` on a lighting device from the model and keep it in the active profile.
    fn show_light(&mut self, id: String, mode: oma_hw::profile::LightingMode) -> Task<Message> {
        let Some(device) = self.light_device(&id) else { return Task::none() };
        // The profile keeps the brightness the device has now unless it already has one.
        let kept = self.active_profile().and_then(|p| p.lighting.device_brightness.get(&id).copied()).filter(|b| *b > 0);
        let now = self.lights.get(&id).map(|s| oma_hw::lighting::percent_of(&device, s)).filter(|b| *b > 0);
        self.remember_light(id.clone(), mode.clone(), if kept.is_none() { now } else { None });
        let brightness = self.active_profile().and_then(|p| p.lighting.brightness_for(&id));
        let accent = self.active_profile().map(|p| p.accent).unwrap_or(oma_hw::profile::Rgb::new(255, 61, 104));
        Task::perform(
            async move {
                let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                oma_hw::lighting::apply(&c, &device, &mode, brightness, accent).await
            },
            Self::after_light,
        )
    }

    fn after_light(r: Result<(), String>) -> Message {
        match r {
            Ok(()) => Message::Lighting(LightingMsg::Reload),
            Err(e) => Message::Applied(Err(e)),
        }
    }

    fn update_lighting(&mut self, m: LightingMsg) -> Task<Message> {
        use oma_hw::lighting::LightState;
        use oma_hw::profile::{LightingMode, Rgb};
        let sel = self.rgb_sel.min(self.rgb_devices.len().saturating_sub(1));
        let dev_index = self.rgb_devices.get(sel).map(|d| d.index);
        let dev_name = self.rgb_devices.get(sel).map(|d| d.name.clone());
        let remember = |app: &mut App, name: Option<String>, mode: LightingMode| {
            if let Some(n) = name {
                app.remember_light(format!("openrgb:{n}"), mode, None);
            }
        };
        let model_ids = |app: &App, colour: bool| -> Vec<String> {
            app.model.iter().flat_map(|m| &m.lighting).filter(|d| !colour || (matches!(d.backend, oma_hw::model::LightingBackend::AsusdAura { .. }) && d.modes.contains(&0))).map(|d| d.id.to_string()).collect()
        };
        match m {
            LightingMsg::Refresh => return Task::batch([Self::probe_rgb(Message::RgbServer), self.load_lights()]),
            LightingMsg::Reload => return self.load_lights(),
            LightingMsg::Loaded(states) => {
                self.lights = states.into_iter().collect();
                // Keep the ASUS page's keyboard brightness in step.
                if let (Some(k), Some(m)) = (&mut self.asus.kbd, &self.model) {
                    for d in &m.lighting {
                        if matches!(&d.backend, oma_hw::model::LightingBackend::AsusdAura { path } if *path == k.path)
                            && let Some(LightState::Aura { level, .. }) = self.lights.get(d.id.as_str())
                        {
                            k.brightness = *level;
                        }
                    }
                }
            }
            LightingMsg::SelectDevice(id) => {
                self.light_sel = Some(id);
                self.slash_drag = None;
            }
            LightingMsg::Show(id, mode) => return self.show_light(id, mode),
            LightingMsg::Level(id, pct) => {
                self.slash_drag = None;
                let (Some(device), Some(state)) = (self.light_device(&id), self.lights.get(&id)) else { return Task::none() };
                let on = match state {
                    LightState::Slash { enabled, .. } => *enabled,
                    LightState::Aura { .. } => pct > 0,
                };
                let mode = if on { oma_hw::lighting::lit_mode(state) } else { LightingMode::Off };
                self.remember_light(id, mode, Some(pct));
                return Task::perform(
                    async move {
                        let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                        oma_hw::lighting::set_brightness(&c, &device, pct).await
                    },
                    Self::after_light,
                );
            }
            LightingMsg::SlashDrag(pct) => self.slash_drag = Some(pct),
            LightingMsg::SlashRelease(id) => {
                if let Some(pct) = self.slash_drag {
                    return self.update_lighting(LightingMsg::Level(id, pct));
                }
            }
            LightingMsg::SlashOption(id, option, on) => {
                let Some(device) = self.light_device(&id) else { return Task::none() };
                if let Some(LightState::Slash { options, .. }) = self.lights.get_mut(&id) {
                    options.iter_mut().filter(|(o, _)| *o == option).for_each(|(_, v)| *v = on);
                }
                return Task::perform(
                    async move {
                        let c = zbus::Connection::system().await.map_err(|e| e.to_string())?;
                        oma_hw::lighting::set_slash_option(&c, &device, option, on).await
                    },
                    Self::after_light,
                );
            }
            LightingMsg::Forget(key) => {
                let id = self.config.active_profile;
                if let Some(pr) = self.config.profiles.iter_mut().find(|p| p.id == id) {
                    pr.lighting.zones.remove(&key);
                    pr.lighting.device_brightness.remove(&key);
                }
                crate::config_store::save(&self.config);
            }
            LightingMsg::StartServer => {
                if let Err(e) = oma_hw::rgb::start_server() {
                    self.toast = Some((format!("Cannot start OpenRGB: {e}"), false));
                    return Task::none();
                }
                return Task::perform(
                    async {
                        for _ in 0..40 {
                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                            if tokio::task::spawn_blocking(oma_hw::rgb::server_running).await.unwrap_or(false) {
                                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                                return oma_hw::rgb::devices().await.map_err(|e| e.to_string());
                            }
                        }
                        Err("OpenRGB server did not come up".into())
                    },
                    Message::RgbDevices,
                );
            }
            LightingMsg::Select(i) => {
                self.rgb_sel = i;
                self.light_sel = None;
            }
            LightingMsg::Hex(s) => {
                // Applied once typing settles, not per keystroke: a colour that
                // sticks is written to the device's own memory.
                self.rgb_hex = s.clone();
                if crate::pages::lighting::parse_hex(&s).is_some() {
                    return Task::perform(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                        s
                    }, |s| Message::Lighting(LightingMsg::HexSettled(s)));
                }
            }
            LightingMsg::HexSettled(s) => {
                if s == self.rgb_hex
                    && let Some(c) = crate::pages::lighting::parse_hex(&s)
                {
                    return self.update_lighting(LightingMsg::Color(c));
                }
            }
            LightingMsg::Color(c) => {
                self.rgb_hex = c.hex();
                if let Some(d) = crate::pages::lighting::selected_model(self).cloned() {
                    let breathing = matches!(self.lights.get(d.id.as_str()), Some(LightState::Aura { listed: 1, level: 1.., .. }));
                    return self.show_light(d.id.to_string(), if breathing { LightingMode::Breathing(c) } else { LightingMode::Static(c) });
                }
                let Some(idx) = dev_index else { return Task::none() };
                remember(self, dev_name, LightingMode::Static(c));
                return Task::batch([Task::perform(async move { oma_hw::rgb::set_static(idx, (c.r, c.g, c.b), true).await.map(|_| String::new()).map_err(|e| e.to_string()) }, |r| match r { Ok(_) => Message::Lighting(LightingMsg::Refresh), Err(e) => Message::Applied(Err(e)) })]);
            }
            LightingMsg::Mode(idx, mode) => {
                let name = self.rgb_devices.iter().find(|d| d.index == idx).map(|d| d.name.clone());
                let is_rainbow = self.rgb_devices.iter().find(|d| d.index == idx).and_then(|d| d.modes.get(mode)).map(|m| m.name.to_ascii_lowercase().contains("rainbow") || m.name.to_ascii_lowercase().contains("spectrum")).unwrap_or(false);
                remember(self, name, if is_rainbow { LightingMode::Rainbow } else { LightingMode::Firmware(mode as u32) });
                return Task::perform(async move { oma_hw::rgb::set_mode(idx, mode, true).await.map_err(|e| e.to_string()) }, |r| match r { Ok(_) => Message::Lighting(LightingMsg::Refresh), Err(e) => Message::Applied(Err(e)) });
            }
            LightingMsg::Off(idx) => {
                let name = self.rgb_devices.iter().find(|d| d.index == idx).map(|d| d.name.clone());
                remember(self, name, LightingMode::Off);
                return Task::perform(async move { oma_hw::rgb::turn_off(idx).await.map_err(|e| e.to_string()) }, |r| match r { Ok(_) => Message::Lighting(LightingMsg::Refresh), Err(e) => Message::Applied(Err(e)) });
            }
            LightingMsg::AllOff => {
                let mut tasks: Vec<Task<Message>> = model_ids(self, false).into_iter().map(|id| self.show_light(id, LightingMode::Off)).collect();
                let idxs: Vec<usize> = self.rgb_devices.iter().map(|d| d.index).collect();
                let names: Vec<String> = self.rgb_devices.iter().map(|d| d.name.clone()).collect();
                for n in names {
                    remember(self, Some(n), LightingMode::Off);
                }
                if !idxs.is_empty() {
                    tasks.push(Task::perform(async move { for i in idxs { let _ = oma_hw::rgb::turn_off(i).await; } Ok::<_, String>(()) }, |_| Message::Lighting(LightingMsg::Refresh)));
                }
                return Task::batch(tasks);
            }
            LightingMsg::SyncAccent => {
                let accent = self.active_profile().map(|p| p.accent).unwrap_or(Rgb::new(255, 61, 104));
                let mut tasks: Vec<Task<Message>> = model_ids(self, true).into_iter().map(|id| self.show_light(id, LightingMode::Static(accent))).collect();
                let idxs: Vec<usize> = self.rgb_devices.iter().map(|d| d.index).collect();
                let names: Vec<String> = self.rgb_devices.iter().map(|d| d.name.clone()).collect();
                for n in names {
                    remember(self, Some(n), LightingMode::Static(accent));
                }
                if !idxs.is_empty() {
                    tasks.push(Task::perform(async move { for i in idxs { let _ = oma_hw::rgb::set_static(i, (accent.r, accent.g, accent.b), true).await; } Ok::<_, String>(()) }, |_| Message::Lighting(LightingMsg::Refresh)));
                }
                return Task::batch(tasks);
            }
            LightingMsg::ThermalToggle => self.rgb_thermal = !self.rgb_thermal,
        }
        Task::none()
    }

    /// Thermal glow: tint all RGB devices from cool→hot by the CPU/GPU max temperature.
    fn rgb_thermal_tick(&mut self, snap: &telemetry::Snapshot) -> Task<Message> {
        if !self.rgb_thermal || !self.rgb_server || !snap.seq.is_multiple_of(4) {
            return Task::none();
        }
        // Without any temperature there is nothing to tint by: leave the lights alone.
        let Some(t) = [snap.cpu.tctl_c, snap.gpu().and_then(|g| g.temp_c)].into_iter().flatten().reduce(f64::max) else { return Task::none() };
        let c = theme::thermal(&self.palette, t, 40.0, 90.0);
        let rgb = ((c.r * 255.0) as u8, (c.g * 255.0) as u8, (c.b * 255.0) as u8);
        let idxs: Vec<usize> = self.rgb_devices.iter().map(|d| d.index).collect();
        // A tint that changes every few seconds runs direct: nothing is written to the devices' memory.
        Task::perform(async move { for i in idxs { let _ = oma_hw::rgb::set_static(i, rgb, false).await; } }, |_| Message::DismissToastNoop)
    }

    /// Run the software fan engine for one telemetry frame.
    fn fan_tick(&mut self, now: std::time::Instant) -> Task<Message> {
        // Commands can only be written once the backend exists; until then the
        // engine must not advance, or it would record duties that never landed.
        // And once the shutdown has handed the fans back, nothing retakes them.
        if self.quitting {
            return Task::none();
        }
        let Some(be) = self.fan_backend.clone() else { return Task::none() };
        if self.effective_fan_owner() != FanOwner::OmaAsus {
            if self.fan_engine.is_idle() {
                return Task::none();
            }
            // Owner changed away from us: release everything, retried until confirmed.
            let cmds = self.fan_engine.evaluate(&Default::default(), &Default::default(), now);
            return self.dispatch_fan_cmds(cmds);
        }
        let Some(pr) = self.active_profile().cloned() else { return Task::none() };
        let temps = crate::fans::control_temps(self.snapshot.as_deref(), self.snapshot_at, now);
        // Drive only outputs this machine has; assignments for absent devices stay in the profile.
        let mut cooling = pr.cooling;
        cooling.fans.retain(|f| be.has(&f.target));
        let cmds = self.fan_engine.evaluate(&cooling, &temps, now);
        self.dispatch_fan_cmds(cmds)
    }

    fn dispatch_fan_cmds(&mut self, cmds: Vec<oma_hw::fanengine::Command>) -> Task<Message> {
        if cmds.is_empty() {
            return Task::none();
        }
        let Some(be) = self.fan_backend.clone() else { return Task::none() };
        Task::batch(cmds.into_iter().map(move |c| {
            let be = be.clone();
            Task::perform(
                async move {
                    let r = be.apply(c.clone()).await;
                    (c, r)
                },
                |(c, r)| Message::FanResult(c, r),
            )
        }))
    }

    /// The one way out: hand every fan back to firmware and persist the
    /// config, then exit.
    fn shutdown(&mut self, reason: &str) -> Task<Message> {
        tracing::info!(reason, "shutting down: releasing fans and saving the config");
        self.quitting = true;
        let mut cmds = self.fan_engine.release_all(std::time::Instant::now());
        // Firmware curves are safe without OmaAsus: leave them in place.
        if let Some(m) = &self.model {
            cmds.retain(|c| m.fan(c.target.as_str()).is_some_and(|f| f.caps.duty));
        }
        crate::config_store::flush();
        let Some(be) = self.fan_backend.clone().filter(|_| !cmds.is_empty()) else { return iced::exit() };
        Task::perform(
            async move {
                for c in cmds {
                    if let Err(e) = be.apply(c.clone()).await {
                        tracing::warn!(target = ?c.target, error = %e, "could not release fan on exit");
                    }
                }
            },
            |_| Message::DismissToastNoop,
        )
        .chain(iced::exit())
    }


    fn overlay_id(&self) -> Option<Id> {
        self.surfaces.iter().find(|(_, s)| **s == Surface::Overlay).map(|(id, _)| *id)
    }

    fn window_id(&self) -> Option<Id> {
        self.surfaces.iter().find(|(_, s)| **s == Surface::Window).map(|(id, _)| *id)
    }

    fn open_overlay(&mut self) -> Task<Message> {
        let anchor = match self.config.overlay.anchor.as_str() {
            "left" => Anchor::Left | Anchor::Top,
            "center" => Anchor::Top,
            _ => Anchor::Right | Anchor::Top,
        };
        // Sized to its content; `fit_overlay` follows the content afterwards.
        let (w, max_h) = self.overlay_bounds();
        let h = crate::pages::quick::layout(self, w as f32, max_h).1 as u32;
        self.overlay_anchor = anchor;
        self.overlay_h = h;
        let o = &self.config.overlay;
        let (id, task) = Message::layershell_open(NewLayerShellSettings {
            size: LayerSize::px(w, h.max(1)),
            layer: Layer::Overlay,
            anchor,
            margin: Some((o.margin as i32, o.margin as i32, o.margin as i32, o.margin as i32)),
            exclusive_zone: Some(0),
            keyboard_interactivity: KeyboardInteractivity::OnDemand,
            output_option: OutputOption::LastOutput,
            blur_option: BlurOption::FullRegion,
            namespace: Some("omaasus-overlay".into()),
            ..Default::default()
        });
        self.surfaces.insert(id, Surface::Overlay);
        self.overlay_phase = Some(OverlayPhase::Opening(std::time::Instant::now()));
        // Fresh platform state for the controls, and the space under the bar.
        Task::batch([task, Task::perform(crate::pages::asus::load(), Message::AsusLoaded), load_ppd(), load_screen()])
    }

    /// Pages this machine has; the ASUS page only with asusd, supergfxd or an ASUS laptop.
    /// Pages for what this machine has; the one showing stays listed.
    pub fn visible_pages(&self) -> Vec<Page> {
        Page::ALL.iter().copied().filter(|pg| *pg == self.page || self.page_available(*pg)).collect()
    }

    /// Whether a page has something to control here. Until detection is done
    /// only pages every machine has are offered, so none disappear later.
    pub fn page_available(&self, pg: Page) -> bool {
        let m = self.model.as_deref();
        match pg {
            Page::Dashboard | Page::Cpu | Page::Profiles | Page::Automation | Page::Settings => true,
            Page::Gpu => m.is_some_and(|m| !m.gpus.is_empty()),
            Page::Cooling => m.is_some_and(|m| !m.fans.is_empty()) || self.cc_connected,
            Page::Lighting => m.is_some_and(|m| !m.lighting.is_empty()) || self.rgb_server || self.rgb_installed,
            // An ASUS laptop without asusd gets the page too: it says what to install.
            Page::Asus => self.asus.asusd.is_some() || self.asus.gfx.is_some() || self.inventory.as_ref().is_some_and(|i| i.platform == oma_hw::Platform::AsusLaptop),
        }
    }

    /// Panel width and the most height it may take: the space under the bar
    /// less margins, capped by the configured maximum (0 = no cap).
    fn overlay_bounds(&self) -> (u32, f32) {
        let o = &self.config.overlay;
        let free = self.screen_h.unwrap_or(900.0) - 2.0 * o.margin as f32;
        let max_h = if o.height > 0 { free.min(o.height as f32) } else { free };
        (o.width.clamp(360, 640), max_h.max(240.0))
    }

    /// Resize the panel when its content changed height.
    fn fit_overlay(&mut self) -> Task<Message> {
        let Some(id) = self.overlay_id() else { return Task::none() };
        let (w, max_h) = self.overlay_bounds();
        let h = crate::pages::quick::layout(self, w as f32, max_h).1 as u32;
        if h == self.overlay_h {
            return Task::none();
        }
        self.overlay_h = h;
        Task::done(Message::LayoutChange { id, anchor: self.overlay_anchor, size: LayerSize::px(w, h.max(1)) })
    }

    /// Fold the panel up; the surface closes when the animation has finished.
    fn close_overlay(&mut self) -> Task<Message> {
        if self.overlay_id().is_none() || matches!(self.overlay_phase, Some(OverlayPhase::Closing(_))) {
            return Task::none();
        }
        self.overlay_phase = Some(OverlayPhase::Closing(std::time::Instant::now()));
        Task::none()
    }

    /// 0 = folded into the bar, 1 = fully revealed.
    fn overlay_progress(&self) -> f32 {
        match self.overlay_phase {
            None | Some(OverlayPhase::Open) => 1.0,
            Some(OverlayPhase::Opening(at)) => reveal::ease_out_cubic(self.now.saturating_duration_since(at).as_secs_f32() / OVERLAY_OPEN.as_secs_f32()),
            Some(OverlayPhase::Closing(at)) => 1.0 - reveal::ease_in_cubic(self.now.saturating_duration_since(at).as_secs_f32() / OVERLAY_CLOSE.as_secs_f32()),
        }
    }

    /// Forget a surface once it is gone. Without a tray to come back from,
    /// losing the main window (with no overlay up) ends the app.
    fn surface_gone(&mut self, id: Id) -> Task<Message> {
        let Some(kind) = self.surfaces.remove(&id) else { return Task::none() };
        if kind == Surface::Overlay {
            self.overlay_phase = None;
        }
        if kind == Surface::Window && !self.overlay_only && self.overlay_id().is_none() && !self.lives_in_tray() {
            return self.shutdown("the main window closed and no tray is showing OmaAsus");
        }
        Task::none()
    }

    /// Whether closing the main window should leave the daemon running: only
    /// when a bar actually shows our tray item, so the user always has a way back.
    fn lives_in_tray(&self) -> bool {
        self.config.tray_enabled && self.tray.is_some() && self.tray_hosted
    }

    fn tray_state(&self) -> tray::TrayState {
        tray::TrayState {
            profiles: self.config.profiles.iter().map(|p| (p.id, p.name.clone())).collect(),
            active: Some(self.config.active_profile),
            overlay_open: self.overlay_id().is_some() && !matches!(self.overlay_phase, Some(OverlayPhase::Closing(_))),
            window_open: self.window_id().is_some(),
            helper: self.controller_ready,
        }
    }

    fn update(&mut self, msg: Message) -> Task<Message> {
        // Animation ticks don't change the panel's content; skip re-fitting on them.
        let refit = !matches!(msg, Message::Tick(_));
        let task = self.update_inner(msg);
        let fit = if refit { self.fit_overlay() } else { Task::none() };
        // Mirror anything the tray menu shows (profiles, open surfaces) after
        // every change, but only push over the bus when it actually differs.
        if let Some(h) = &self.tray {
            let state = self.tray_state();
            if state != self.tray_synced {
                self.tray_synced = state.clone();
                let h = h.clone();
                return Task::batch([task, fit, Task::future(async move { h.sync(state).await }).discard()]);
            }
        }
        Task::batch([task, fit])
    }

    fn update_inner(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::Telemetry(telemetry::Event::Frame(snap)) => {
                push(&mut self.hist.cpu_load, Some(snap.cpu.util_total as f32));
                push(&mut self.hist.cpu_temp, snap.cpu.tctl_c.map(|t| t as f32));
                // The GPU worth showing: an awake discrete card, else the integrated one.
                let gpu = snap.gpu().unwrap_or_default();
                push(&mut self.hist.gpu_load, gpu.load.map(|l| l as f32));
                push(&mut self.hist.gpu_temp, gpu.temp_c.map(|t| t as f32));
                push(&mut self.hist.gpu_power, snap.nvidia.as_ref().and_then(|n| n.power_w).map(|w| w as f32));
                push(&mut self.hist.coolant, snap.coolant_c.map(|t| t as f32));
                push(&mut self.hist.power, snap.package_w().map(|w| w as f32));
                if !self.cpu_synced {
                    self.cpu_edit = snap.cpu_control.clone();
                    self.cpu_synced = true;
                }
                if !self.gpu_dirty
                    && let Some(n) = &snap.nvidia
                {
                    self.gpu_edit.gpc_offset_mhz = n.gpc_offset_mhz;
                    self.gpu_edit.mem_offset_mhz = n.mem_offset_mhz;
                    self.gpu_edit.persistence = n.persistence;
                    self.gpu_edit.power_limit_w = n.power_limit_w.map(|w| w.round() as u32);
                    self.gpu_edit.fan_percent = if n.fan_policy_manual.iter().any(|m| *m) { Some(n.fan_percent.clone()) } else { None };
                }
                // The held connection (`rgb_stream`) says when the server comes
                // or goes: nothing to poll here.
                let rgb_probe = Task::none();
                // The dGPU woke after startup: fetch its details for the GPU page
                // (the sampler only reports it while awake, so this doesn't wake it).
                let nvidia_info = if snap.nvidia.is_some() && self.nvidia_info.is_none() && snap.seq % 20 == 1 && telemetry::dgpu_open_ok() {
                    Task::perform(async { tokio::task::spawn_blocking(|| oma_hw::nvidia::NvidiaGpu::open(0).and_then(|g| g.info()).ok().map(Arc::new)).await.unwrap_or(None) }, Message::NvidiaInfo)
                } else {
                    Task::none()
                };
                let task = Task::batch([self.rgb_thermal_tick(&snap), rgb_probe, nvidia_info]);
                self.snapshot = Some(snap);
                self.snapshot_at = Some(std::time::Instant::now());
                let auto = if self.snapshot.as_ref().map(|s| s.seq % 2 == 0).unwrap_or(false) { self.auto_evaluate() } else { Task::none() };
                Task::batch([task, auto])
            }
            Message::Hardware(raw) => {
                let (quirks, problem) = crate::config_store::load_quirks();
                if let Some(e) = problem {
                    self.toast = Some((e, false));
                }
                let model = Arc::new(oma_hw::model::HardwareModel::build(&raw, &quirks));
                let inv = Arc::new(raw.system.clone());
                self.inventory_title = format!("{} · {}", inv.dmi.board_name, inv.cpu.model.split(" Processor").next().unwrap_or(&inv.cpu.model));
                self.inventory = Some(inv.clone());
                if self.config.profiles.is_empty() {
                    // First run: profiles for what this machine has.
                    let fresh = Config::generated(&model);
                    self.config.profiles = fresh.profiles;
                    self.config.rules = fresh.rules;
                    self.config.active_profile = fresh.active_profile;
                    self.config.default_profile = fresh.default_profile;
                    crate::config_store::save(&self.config);
                }
                self.model = Some(model.clone());
                telemetry::set_model(model.clone());
                if !self.page_available(self.page) {
                    self.page = Page::Dashboard;
                }
                let lights = self.load_lights();
                let cc = self.cc.clone();
                Task::batch([Task::perform(async move { Arc::new(crate::fans::FanBackend::build(model, inv, cc).await) }, Message::FanBackend), lights])
            }
            Message::Controller(ok) => {
                self.controller_ready = ok;
                Task::none()
            }
            Message::Navigate(p) => {
                // A page this machine doesn't have (asked for over IPC) opens the dashboard.
                self.page = if self.model.is_none() || self.page_available(p) { p } else { Page::Dashboard };
                Task::none()
            }
            Message::NvidiaInfo(i) => {
                self.nvidia_info = i;
                Task::none()
            }
            Message::Cpu(m) => self.update_cpu(m),
            Message::Gpu(m) => self.update_gpu(m),
            Message::Cooling(m) => self.update_cooling(m),
            Message::Lighting(m) => self.update_lighting(m),
            Message::Profiles(m) => self.update_profiles(m),
            Message::Automation(m) => self.update_automation(m),
            Message::Settings(m) => self.update_settings(m),
            Message::FontLoaded(ok) => {
                if !ok {
                    tracing::warn!("a bundled font failed to load");
                }
                Task::none()
            }
            Message::Tick(now) => {
                self.now = now;
                if let Some((from, to, since)) = self.theme_fade {
                    let t = now.saturating_duration_since(since).as_secs_f32() / THEME_FADE.as_secs_f32();
                    if t >= 1.0 {
                        self.palette = to;
                        self.theme_fade = None;
                    } else {
                        self.palette = from.blend(&to, reveal::ease_out_cubic(t));
                    }
                }
                match self.overlay_phase {
                    Some(OverlayPhase::Opening(at)) if now.saturating_duration_since(at) >= OVERLAY_OPEN => self.overlay_phase = Some(OverlayPhase::Open),
                    Some(OverlayPhase::Closing(at)) if now.saturating_duration_since(at) >= OVERLAY_CLOSE => {
                        self.overlay_phase = None;
                        if let Some(id) = self.overlay_id() {
                            return Task::done(Message::Close(id));
                        }
                    }
                    _ => {}
                }
                // Success toasts fade after a few seconds; errors stay until dismissed.
                if self.toast.is_some() && self.toast_at.is_none() {
                    self.toast_at = Some(now);
                }
                if let (Some((_, true)), Some(at)) = (&self.toast, self.toast_at)
                    && now.duration_since(at) > std::time::Duration::from_secs(6)
                {
                    self.toast = None;
                    self.toast_at = None;
                }
                widgets::set_phase(now.duration_since(self.t0).as_secs_f32());
                widgets::set_thermal(self.smooth.heat, self.smooth.load);
                if let Some(s) = &self.snapshot {
                    let k = 0.12;
                    // Missing readings stay missing (NaN): gauges show a dash, not 0.
                    ease(&mut self.smooth.cpu_t, s.cpu.tctl_c.map(|t| t as f32), k);
                    ease(&mut self.smooth.gpu_t, s.gpu().and_then(|g| g.temp_c).map(|t| t as f32), k);
                    ease(&mut self.smooth.coolant, s.coolant_c.map(|t| t as f32), k);
                    ease(&mut self.smooth.gpu_w, s.nvidia.as_ref().and_then(|n| n.power_w).map(|w| w as f32), k);
                    let power = s.nvidia.as_ref().and_then(|n| n.power_w).or_else(|| s.package_w()).map(|w| w as f32);
                    ease(&mut self.smooth.power_w, power, k);
                    if s.nvidia.is_none()
                        && let Some(w) = power
                    {
                        self.smooth.power_peak = self.smooth.power_peak.max(w);
                    }
                    // Fans: no live tach at all is unknown, not 0 rpm.
                    let fastest = s.fans.iter().filter(|f| f.freshness == telemetry::Freshness::Live).map(|f| f.rpm).max();
                    ease(&mut self.smooth.fan_rpm, fastest.map(|r| r as f32), k);
                    ease(&mut self.smooth.cpu_load, Some(s.cpu.util_total as f32), k);
                    ease(&mut self.smooth.gpu_load, s.gpu().and_then(|g| g.load).map(|l| l as f32), k);
                    // The ambient field follows whatever temperature is known; none known, it rests.
                    let heat = finite_max(self.smooth.cpu_t, self.smooth.gpu_t).map(|t| ((t - 40.0) / 50.0).clamp(0.0, 1.0)).unwrap_or(0.0);
                    ease(&mut self.smooth.heat, Some(heat), 0.05);
                    let load = finite_max(self.smooth.cpu_load, self.smooth.gpu_load).map(|l| (l / 100.0).clamp(0.0, 1.0)).unwrap_or(0.0);
                    ease(&mut self.smooth.load, Some(load), 0.05);
                }
                Task::none()
            }
            Message::AsusLoaded(st) => {
                // The graphics mode says whether the dGPU is meant to be in use. A
                // pending mode doesn't say a switch runs (a refused one stays set),
                // and a state that didn't load keeps the last answer.
                if let Some(g) = &st.gfx {
                    telemetry::allow_dgpu(g.mode.uses_dgpu());
                }
                self.asus = st;
                Task::none()
            }
            Message::Asus(m) => self.update_asus(m),
            Message::Quick(m) => self.update_quick(m),
            Message::PpdLoaded(s) => {
                self.ppd = s;
                Task::none()
            }
            Message::ScreenSpace(h) => {
                self.screen_h = h;
                Task::none()
            }
            Message::HelperInstalled(r) => {
                match r {
                    Ok(out) => {
                        self.helper_log = out;
                        self.toast = Some(("Helper installed".into(), true));
                    }
                    Err(e) => {
                        self.helper_log = e.clone();
                        self.toast = Some((format!("Helper install failed: {}", e.lines().last().unwrap_or("")), false));
                    }
                }
                Task::perform(async { oma_hw::helper::Controller::connect().await.has_helper() }, Message::Controller)
            }
            Message::Auto(ev) => {
                self.auto_state.absorb(ev);
                self.auto_evaluate()
            }
            Message::RgbDevices(r) => {
                match r {
                    Ok(d) => {
                        self.rgb_server = true;
                        self.rgb_devices = d;
                    }
                    Err(e) => {
                        self.toast = Some((format!("OpenRGB: {e}"), false));
                        // Whether the server is there decides if it is asked again.
                        return Self::probe_rgb(Message::RgbServerSeen);
                    }
                }
                Task::none()
            }
            Message::RgbLink(up) => {
                self.rgb_server = up;
                if up {
                    // The server (or the app) just came up, or its devices changed:
                    // the active profile's lighting goes back on, since nothing
                    // else puts it there after a restart.
                    Task::perform(async { oma_hw::rgb::devices().await.map_err(|e| e.to_string()) }, Message::RgbUp)
                } else {
                    tracing::info!("OpenRGB server gone");
                    Task::none()
                }
            }
            Message::RgbUp(r) => match r {
                Ok(d) if d.is_empty() => {
                    // Still detecting: its device-list notice follows.
                    self.rgb_devices = d;
                    Task::none()
                }
                Ok(d) => {
                    self.rgb_devices = d;
                    tracing::info!(devices = self.rgb_devices.len(), "OpenRGB server up; putting the profile's lighting back");
                    self.restore_openrgb_lighting()
                }
                Err(e) => {
                    tracing::warn!(error = %e, "OpenRGB server up but its devices could not be listed");
                    Task::none()
                }
            },
            Message::LightingRestored(skipped) => {
                // Unattended: a device that is absent, or that OpenRGB lists
                // without LEDs, is noted in the log, not put in the user's face.
                if skipped.is_empty() {
                    tracing::info!("profile lighting put back on the OpenRGB devices");
                } else {
                    tracing::warn!(skipped = ?skipped, "some of the profile's lighting could not be put back");
                }
                Self::rgb_refresh()
            }
            Message::CcReady(ok, modes) => {
                self.cc_connected = ok;
                self.cc_modes = modes;
                self.cc = Some(self.cc_client());
                Task::none()
            }
            Message::FanBackend(b) => {
                self.fan_engine.set_floors(b.floors());
                b.set_power_mode(self.active_profile().and_then(|p| p.cpu.power_mode.clone()));
                self.fan_backend = Some(b);
                Task::none()
            }
            Message::FanSafety(now) => self.fan_tick(now),
            Message::FanResult(cmd, r) => {
                self.fan_engine.report(&cmd, r.is_ok(), std::time::Instant::now());
                // A stopping helper (an upgrade, a reinstall) refuses for a moment:
                // expected, and the engine sends the write again shortly. A command
                // overtaken by a newer one was never written and says nothing.
                if let Err(e) = r
                    && !e.contains(oma_hw::helper::RESTARTING)
                    && !e.contains(crate::fans::SUPERSEDED)
                {
                    self.fan_errors += 1;
                    if self.fan_errors <= 3 || self.fan_errors.is_multiple_of(60) {
                        self.toast = Some((format!("Fan control: {e}"), false));
                    }
                }
                Task::none()
            }
            Message::ApplyProfile(id) => self.start_apply(id, crate::apply::Origin::Manual),
            Message::AutoApply(id) => self.start_apply(id, crate::apply::Origin::Automation),
            Message::Reapply(reason) => {
                // Resume and a charger change often arrive together: once is enough.
                let now = std::time::Instant::now();
                if self.last_reapply.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(3)) {
                    return Task::none();
                }
                self.last_reapply = Some(now);
                tracing::info!(reason, "re-applying the active profile");
                self.start_apply(self.config.active_profile, crate::apply::Origin::Reapply)
            }
            Message::ProfileApplied(generation, name, origin, report) => {
                use crate::coordinator::Finish;
                let next = match self.coord.finished(generation, report.clone(), std::time::Instant::now()) {
                    // Replaced by a newer request: nothing here describes the machine now.
                    Finish::Superseded => return Task::none(),
                    Finish::Accepted => None,
                    Finish::StartNext(generation, request) => Some((generation, request)),
                };
                if let Some((generation, request)) = next {
                    // What it did stopped early; the queued request runs now.
                    return self.launch_apply(generation, request);
                }
                // Firmware can reset fan curves when the power mode changes: send them again now it has.
                self.fan_engine.invalidate();
                tracing::info!(profile = %name, ?origin, applied = ?report.applied, skipped = ?report.skipped, failed = ?report.failed, "profile apply finished");
                let summary = report.summary(&name);
                // Re-applies stay quiet unless something failed.
                if origin != crate::apply::Origin::Reapply || summary.is_err() {
                    self.toast = Some(match summary {
                        Ok(s) => (s, true),
                        Err(e) => (e, false),
                    });
                    self.toast_at = Some(std::time::Instant::now());
                }
                // NVIDIA settings skipped while the dGPU settled go in once it has.
                let nvidia = if report.nvidia_deferred { self.nvidia_when_ready() } else { Task::none() };
                Task::batch([self.load_lights(), nvidia])
            }
            Message::System(ev) => {
                use crate::events::Event;
                // Limits and graphics state move with all of these but the helper's
                // hand-back: refresh the ASUS view too.
                let reload = Task::perform(crate::pages::asus::load(), Message::AsusLoaded);
                match ev {
                    // The helper handed back the fans it guarded as it stopped. Send
                    // them again: that starts a fresh helper, which records the state
                    // they were handed back in.
                    Event::RecoveryAbandoned(outputs) => {
                        // Persistent: the user must know a fan is on nobody's control.
                        self.toast = Some((format!("Fan recovery abandoned by the helper: {outputs}. Check these outputs in Cooling or restart oma-helper"), false));
                        self.toast_at = None;
                        Task::none()
                    }
                    Event::FansHandedBack => {
                        // Not after a package removal: there's no helper left to start.
                        if oma_hw::helper::activatable() {
                            self.fan_engine.resend_all();
                        }
                        Task::none()
                    }
                    // supergfxd kills whatever holds the dGPU while it switches: let go
                    // now, and stay away until the switch has finished.
                    Event::GraphicsSwitch => {
                        telemetry::hold_off_dgpu(DGPU_HOLD_OFF);
                        Task::batch([reload, wait_for_switch()])
                    }
                    // Its settings go in once it has settled, where the mode uses it;
                    // nothing else in the profile depends on it.
                    Event::DgpuArrived => {
                        let used = self.asus.gfx.as_ref().is_none_or(|g| g.mode.uses_dgpu());
                        Task::batch([reload, if used { self.nvidia_when_ready() } else { Task::none() }])
                    }
                    Event::Resumed => Task::batch([reload, Task::done(Message::Reapply("resume"))]),
                    Event::Power(on) => Task::batch([reload, Task::done(Message::Reapply(if on { "charger connected" } else { "charger disconnected" }))]),
                    Event::PowerMode { owner, mode } => {
                        use oma_hw::model::Owner;
                        // What the daemon says now, whoever set it.
                        match owner {
                            Owner::Asusd => self.asus.profile = oma_hw::asusd::PlatformProfile::from_label(&mode),
                            Owner::PowerProfilesDaemon => {
                                if let Some(ppd) = &mut self.ppd {
                                    ppd.active = mode.clone();
                                }
                            }
                            _ => {}
                        }
                        // Only the daemon that owns power modes here is followed:
                        // power-profiles-daemon mirrors asusd's choice on laptops.
                        if self.model.as_ref().and_then(|m| m.controls.power_owner) != Some(owner) {
                            return Task::none();
                        }
                        let follow = self.follow_power_mode(&mode);
                        // asusd puts the mode's own firmware limits in place: show them.
                        if owner == Owner::Asusd { Task::batch([reload, follow]) } else { follow }
                    }
                    Event::CoolerControl(running) => {
                        let before = self.effective_fan_owner();
                        if let Some(inv) = &mut self.inventory {
                            Arc::make_mut(inv).daemons.coolercontrold = running;
                        }
                        let after = self.effective_fan_owner();
                        tracing::info!(running, owner = ?after, "CoolerControl's daemon changed state");
                        if !running {
                            self.cc_connected = false;
                            self.cc_modes.clear();
                        }
                        let mut tasks = Vec::new();
                        if running {
                            // Its Modes, once its API answers.
                            tasks.push(self.probe_cc(std::time::Duration::from_secs(2)));
                        }
                        if before != after {
                            let what = if after == FanOwner::CoolerControl { "CoolerControl started: it drives the fans now" } else { "CoolerControl stopped: OmaAsus drives the fans now" };
                            self.toast = Some((what.into(), true));
                            self.toast_at = Some(std::time::Instant::now());
                            if after == FanOwner::CoolerControl {
                                // Hand every output back before it takes them. When it
                                // stops, the engine drives them again on its next tick.
                                let cmds = self.fan_engine.release_all(std::time::Instant::now());
                                tasks.push(self.dispatch_fan_cmds(cmds));
                            }
                        }
                        Task::batch(tasks)
                    }
                }
            }
            Message::GraphicsSettled => Task::batch([Task::perform(crate::pages::asus::load(), Message::AsusLoaded), Task::done(Message::Reapply("graphics switch"))]),
            Message::GfxProbe(running, mode) => match (running, mode) {
                (false, _) => {
                    telemetry::no_graphics_manager();
                    Task::none()
                }
                (true, Some(m)) => {
                    telemetry::allow_dgpu(m.uses_dgpu());
                    Task::none()
                }
                // supergfxd runs but didn't answer (starting, busy, or no permission):
                // the dGPU is left alone; ask again.
                (true, None) => Task::perform(tokio::time::sleep(std::time::Duration::from_secs(10)), |()| Message::GfxRetry),
            },
            Message::GfxRetry => {
                if telemetry::dgpu_mode_known() {
                    Task::none()
                } else {
                    probe_graphics()
                }
            }
            Message::DgpuReady(ready) => {
                self.dgpu_wait = false;
                if !ready {
                    // Where the mode stopped using the dGPU, or the profile now has no
                    // NVIDIA settings, there is nothing to apply; otherwise say so.
                    if telemetry::dgpu_gate() != telemetry::DgpuGateState::ModeOff && self.active_profile().is_some_and(|p| p.gpu.nvidia.is_some()) {
                        tracing::warn!("deferred NVIDIA settings not applied: the dGPU didn't become ready");
                        self.toast = Some(("NVIDIA settings weren't applied: the dGPU didn't become ready. Apply the profile again once it is".into(), false));
                        self.toast_at = Some(std::time::Instant::now());
                    }
                    return Task::none();
                }
                let Some(p) = self.active_profile().filter(|p| p.gpu.nvidia.is_some()).cloned() else { return Task::none() };
                let name = p.name.clone();
                Task::perform(crate::apply::apply_nvidia(p), move |r| Message::NvidiaApplied(name.clone(), r))
            }
            Message::NvidiaApplied(name, report) => {
                tracing::info!(profile = %name, applied = ?report.applied, skipped = ?report.skipped, failed = ?report.failed, "deferred NVIDIA settings");
                if let Err(e) = report.summary(&name) {
                    self.toast = Some((e, false));
                    self.toast_at = Some(std::time::Instant::now());
                }
                // The gate closed again before it ran (another switch): wait once more.
                if report.nvidia_deferred { self.nvidia_when_ready() } else { Task::none() }
            }
            Message::Applied(r) => {
                self.toast = Some(match r {
                    Ok(s) => (s, true),
                    Err(e) => (e, false),
                });
                self.toast_at = Some(std::time::Instant::now());
                Task::none()
            }
            Message::DismissToast => {
                self.toast = None;
                Task::none()
            }
            Message::DismissToastNoop => Task::none(),
            Message::Ipc(cmd) => match cmd {
                ipc::Command::Toggle => Task::done(Message::ToggleOverlay),
                ipc::Command::Show => {
                    if self.overlay_id().is_none() { self.open_overlay() } else { Task::none() }
                }
                ipc::Command::Hide => self.close_overlay(),
                ipc::Command::Quit => Task::done(Message::Quit),
                ipc::Command::ReloadTheme => Task::done(Message::ReloadTheme),
                ipc::Command::Window => Task::done(Message::OpenWindow),
                ipc::Command::Profile(name) => match self.config.profiles.iter().find(|p| p.name.eq_ignore_ascii_case(&name)) {
                    Some(p) => Task::done(Message::ApplyProfile(p.id)),
                    None => Task::none(),
                },
                ipc::Command::Duplicate => self.shutdown("another OmaAsus is already running"),
                ipc::Command::Page(name) => match Page::ALL.iter().find(|p| p.label().eq_ignore_ascii_case(&name) || format!("{p:?}").eq_ignore_ascii_case(&name)) {
                    Some(p) => Task::done(Message::Navigate(*p)),
                    None => Task::none(),
                },
            },
            Message::ToggleOverlay => match self.overlay_phase {
                // A click while it is folding up brings it straight back.
                Some(OverlayPhase::Closing(_)) => {
                    self.overlay_phase = Some(OverlayPhase::Opening(std::time::Instant::now()));
                    Task::none()
                }
                _ if self.overlay_id().is_some() => self.close_overlay(),
                _ => self.open_overlay(),
            },
            Message::RgbServer(up) => {
                self.rgb_server = up;
                if up { Self::rgb_refresh() } else { Task::none() }
            }
            Message::RgbServerSeen(up) => {
                self.rgb_server = up;
                Task::none()
            }
            Message::ThemeChanged(palette, name) => {
                tracing::info!(theme = %name, "adopting the desktop theme");
                self.theme_name = name;
                if self.surfaces.is_empty() {
                    // Nothing on screen to cross-fade: just take it.
                    self.palette = palette;
                    self.theme_fade = None;
                } else {
                    let from = self.palette;
                    self.theme_fade = Some((from, palette, std::time::Instant::now()));
                }
                Task::none()
            }
            Message::ReloadTheme => {
                let (palette, name) = theme::load();
                Task::done(Message::ThemeChanged(palette, name))
            }
            Message::Tray(tray::Event::Ready(h)) => {
                self.tray = Some(h);
                self.tray_synced = tray::TrayState::default();
                Task::none()
            }
            Message::Tray(tray::Event::Unavailable) => {
                self.tray = None;
                self.tray_hosted = false;
                Task::none()
            }
            Message::Tray(tray::Event::Hosted(on)) => {
                self.tray_hosted = on;
                Task::none()
            }
            Message::Tray(tray::Event::Command(c)) => match c {
                tray::TrayCommand::ToggleOverlay => Task::done(Message::ToggleOverlay),
                tray::TrayCommand::OpenWindow => Task::done(Message::OpenWindow),
                tray::TrayCommand::Profile(id) => Task::done(Message::ApplyProfile(id)),
                tray::TrayCommand::Quit => Task::done(Message::Quit),
            },
            Message::Quit => {
                let ids: Vec<Id> = self.surfaces.drain().map(|(id, _)| id).collect();
                let closes = Task::batch(ids.into_iter().map(|id| Task::done(Message::RemoveWindow(id))));
                let bye = match self.tray.take() {
                    Some(h) => Task::future(async move { h.shutdown().await }).discard(),
                    None => Task::none(),
                };
                Task::batch([closes, bye]).chain(self.shutdown("quit requested"))
            }
            Message::OpenWindow => {
                if self.window_id().is_some() {
                    return Task::none();
                }
                let (id, task) = Message::base_window_open(IcedXdgWindowSettings { size: Some(PixelSize::px(1280, 820)), client_side_decorations: false });
                self.surfaces.insert(id, Surface::Window);
                task
            }
            Message::Close(id) => {
                let task = Task::done(Message::RemoveWindow(id));
                Task::batch([task, self.surface_gone(id)])
            }
            // The compositor (or a Close above) took the surface down.
            Message::SurfaceClosed(id) => self.surface_gone(id),
            _ => Task::none(),
        }
    }

    fn update_cpu(&mut self, m: CpuMsg) -> Task<Message> {
        match m {
            CpuMsg::Governor(g) => self.cpu_edit.governor = g,
            CpuMsg::Epp(e) => self.cpu_edit.epp = Some(e),
            CpuMsg::Boost(b) => self.cpu_edit.boost = Some(b),
            CpuMsg::Smt(b) => self.cpu_edit.smt = Some(b),
            CpuMsg::MaxMhz(v) => self.cpu_edit.scaling_max_khz = (v * 1000.0) as u64,
            CpuMsg::MinMhz(v) => self.cpu_edit.scaling_min_khz = (v * 1000.0) as u64,
            CpuMsg::Revert => {
                self.cpu_synced = false;
            }
            CpuMsg::SaveToProfile => {
                let id = self.config.active_profile;
                let edit = self.cpu_edit.clone();
                if let Some(name) = self.config.profiles.iter_mut().find(|p| p.id == id).map(|pr| {
                    pr.cpu.control = Some(edit);
                    pr.name.clone()
                }) {
                    crate::config_store::save(&self.config);
                    self.toast = Some((format!("CPU settings saved into {name}"), true));
                }
            }
            CpuMsg::Apply => {
                let info = self.inventory.as_ref().map(|i| i.cpu.clone()).unwrap_or_else(oma_hw::cpu::cpu_info);
                let target = self.cpu_edit.clone();
                self.cpu_synced = false;
                return Task::perform(
                    async move {
                        let ctl = oma_hw::helper::Controller::connect().await;
                        let plan = oma_hw::cpu::plan_writes(&info, &target);
                        match ctl.write_batch(&plan).await {
                            Ok(errs) if errs.is_empty() => Ok("CPU settings applied".to_string()),
                            Ok(errs) => Err(format!("CPU: {}", errs[0].1)),
                            Err(e) => Err(format!("CPU: {e}")),
                        }
                    },
                    Message::Applied,
                );
            }
        }
        Task::none()
    }

    fn update_gpu(&mut self, m: GpuMsg) -> Task<Message> {
        let info = self.nvidia_info.clone();
        match m {
            GpuMsg::PowerLimit(w) => {
                self.gpu_edit.power_limit_w = Some(w.round() as u32);
                self.gpu_dirty = true;
            }
            GpuMsg::LockClocks(on) => {
                self.gpu_edit.locked_graphics_mhz = on.then(|| (210, info.as_ref().map(|i| i.max_graphics_mhz).unwrap_or(3000)));
                self.gpu_edit.unlock_clocks = !on;
                self.gpu_dirty = true;
            }
            GpuMsg::LockMax(v) => {
                if let Some((lo, _)) = self.gpu_edit.locked_graphics_mhz {
                    self.gpu_edit.locked_graphics_mhz = Some((lo, v.round() as u32));
                    self.gpu_dirty = true;
                }
            }
            GpuMsg::GpcOffset(v) => {
                self.gpu_edit.gpc_offset_mhz = Some(v.round() as i32);
                self.gpu_dirty = true;
            }
            GpuMsg::MemOffset(v) => {
                self.gpu_edit.mem_offset_mhz = Some(v.round() as i32);
                self.gpu_dirty = true;
            }
            GpuMsg::FanManual(on) => {
                self.gpu_edit.fan_percent = on.then(|| vec![50]);
                self.gpu_edit.fan_auto = !on;
                self.gpu_dirty = true;
            }
            GpuMsg::FanPercent(v) => {
                self.gpu_edit.fan_percent = Some(vec![v.round() as u32]);
                self.gpu_dirty = true;
            }
            GpuMsg::Persistence(b) => {
                self.gpu_edit.persistence = Some(b);
                self.gpu_dirty = true;
            }
            GpuMsg::Revert => {
                self.gpu_dirty = false;
            }
            GpuMsg::SaveToProfile => {
                let id = self.config.active_profile;
                let edit = self.gpu_edit.clone();
                if let Some(name) = self.config.profiles.iter_mut().find(|p| p.id == id).map(|pr| {
                    pr.gpu.nvidia = Some(edit);
                    pr.name.clone()
                }) {
                    crate::config_store::save(&self.config);
                    self.toast = Some((format!("GPU settings saved into {name}"), true));
                }
            }
            GpuMsg::AmdLevel(level) => {
                let gpus = self.inventory.as_ref().map(|i| i.amd_gpus.clone()).unwrap_or_default();
                return Task::perform(
                    async move {
                        let ctl = oma_hw::helper::Controller::connect().await;
                        for g in gpus {
                            ctl.write(g.perf_level_path(), &level).await.map_err(|e| e.to_string())?;
                        }
                        Ok(format!("AMD GPU performance level: {level}"))
                    },
                    Message::Applied,
                );
            }
            GpuMsg::Apply => {
                // Opening the dGPU while supergfxd switches gets OmaAsus killed.
                let why = match telemetry::dgpu_gate() {
                    telemetry::DgpuGateState::Open => None,
                    telemetry::DgpuGateState::Settling => Some("The dGPU is settling after a graphics change; apply again in a few seconds"),
                    telemetry::DgpuGateState::ModeOff => Some("The graphics mode doesn't use the dGPU; change it on the ASUS page first"),
                    telemetry::DgpuGateState::Unknown => Some("Waiting for supergfxd to report the graphics mode"),
                };
                if let Some(why) = why {
                    self.toast = Some((why.into(), false));
                    self.toast_at = Some(std::time::Instant::now());
                    return Task::none();
                }
                let ctl_state = self.gpu_edit.clone();
                self.gpu_dirty = false;
                return Task::perform(
                    async move {
                        let ctl = oma_hw::helper::Controller::connect().await;
                        match ctl.nvidia_apply(0, &ctl_state).await {
                            Ok(errs) if errs.is_empty() => Ok("GPU settings applied".to_string()),
                            Ok(errs) => Err(format!("GPU: {}", errs.iter().map(|(s, e)| format!("{s}: {e}")).collect::<Vec<_>>().join("; "))),
                            Err(e) => Err(format!("GPU: {e}")),
                        }
                    },
                    Message::Applied,
                );
            }
        }
        Task::none()
    }

    fn view(&self, id: Id) -> Element<'_, Message> {
        widgets::begin_frame();
        let p = self.palette;
        let is_overlay = self.surfaces.get(&id) == Some(&Surface::Overlay);
        let shell: Element<Message> = if is_overlay {
            // The tray drop-down is its own composition, sized to its content.
            let (w, max_h) = self.overlay_bounds();
            crate::pages::quick::layout(self, w as f32, max_h).0
        } else {
            self.window_shell()
        };

        let (heat, load) = widgets::thermal();
        let progress = if is_overlay { self.overlay_progress() } else { 1.0 };
        let ambient = shader(widgets::ambient::Ambient { p, time: self.now.duration_since(self.t0).as_secs_f32(), heat, load, cell: 10.0, alpha: if is_overlay { self.config.overlay.opacity.clamp(0.5, 1.0) * progress } else { 1.0 }, intensity: if is_overlay { 0.7 } else { 1.0 } })
            .width(Length::Fill)
            .height(Length::Fill);
        let content_layer = container(shell).width(Length::Fill).height(Length::Fill).style(move |_| container::Style {
            border: iced::Border { color: if is_overlay { p.border_strong } else { iced::Color::TRANSPARENT }, width: if is_overlay { 1.0 } else { 0.0 }, radius: 0.0.into() },
            ..Default::default()
        });
        let content_layer: Element<Message> = if is_overlay { reveal::reveal(content_layer, progress).into() } else { content_layer.into() };
        stack![ambient, content_layer].width(Length::Fill).height(Length::Fill).into()
    }

    /// The main window: site header with navigation, toast, then the page.
    fn window_shell(&self) -> Element<'_, Message> {
        iced::widget::responsive(move |size| self.shell(size.width)).into()
    }

    /// Navigation that fits the window: labelled links and the status pills
    /// when there is room, then the links alone, then icons with tooltips.
    fn header(&self, width: f32) -> Element<'_, Message> {
        let p = self.palette;
        let pages = self.visible_pages();
        // JetBrains Mono advances 0.6 em, so the underline can be sized from the label.
        let mono = |s: &str| s.chars().count() as f32 * size::SMALL * 0.6;
        // The header's quick switch goes to this machine's performance profile.
        let performance = self.config.profile_by_role(ProfileRole::Performance);
        let perf_name = performance.map(|x| x.name.clone()).unwrap_or_else(|| "Performance".into());
        let status = if self.controller_ready { "helper" } else { "read-only" };
        use crate::pages::NavTier;
        let tier = NavTier::for_width(width, &pages, &perf_name, &[&self.theme_name, status]);
        let labels = matches!(tier, NavTier::Full | NavTier::Labels);
        let (word, pills) = (tier == NavTier::Full, tier == NavTier::Full);
        let button = tier != NavTier::Minimal;

        let link = |pg: Page| -> Element<'_, Message> {
            let active = pg == self.page;
            let tint = if active { p.text } else { p.text_secondary };
            let under = move |w: f32| container(iced::widget::Space::new().width(Length::Fixed(w)).height(2.0)).style(move |_| container::Style { background: Some(Background::Color(if active { p.brand } else { iced::Color::TRANSPARENT })), ..Default::default() });
            let body: Element<Message> = if labels {
                column![iced::widget::text(pg.label()).size(size::SMALL).font(theme::font::MONO).color(tint), under(mono(pg.label()))].spacing(6.0).into()
            } else {
                column![icons::icon(pg.icon(), tint, 16.0), under(16.0)].spacing(6.0).align_x(iced::Alignment::Center).into()
            };
            let b = iced::widget::button(body).padding(if labels { [6, 10] } else { [6, 8] }).style(widgets::button_style(p, widgets::ButtonKind::Nav { active: false })).on_press(Message::Navigate(pg));
            if labels {
                b.into()
            } else {
                let tip = container(iced::widget::text(pg.label()).size(size::SMALL).font(theme::font::MONO).color(p.text))
                    .padding([4, 8])
                    .style(move |_| container::Style { background: Some(Background::Color(p.surface_2)), border: iced::Border { color: p.border_strong, width: 1.0, radius: 0.0.into() }, ..Default::default() });
                iced::widget::tooltip(b, tip, iced::widget::tooltip::Position::Bottom).into()
            }
        };
        let links = iced::widget::Row::with_children(pages.iter().map(|pg| link(*pg))).spacing(space::XS).align_y(iced::Alignment::Center).wrap();

        let mut brand = iced::widget::Row::new().spacing(space::SM).align_y(iced::Alignment::Center).push(widgets::pixel::oma_mark(p, 26.0));
        if word {
            brand = brand.push(iced::widget::text("omaasus").size(size::SMALL).font(theme::font::MONO_MEDIUM).color(p.text));
        }
        let mut right = iced::widget::Row::new().spacing(space::SM).align_y(iced::Alignment::Center);
        if pills {
            right = right.push(widgets::pill(p, &self.theme_name, p.text_secondary));
        }
        // Without the helper nothing can be changed: that stays visible at any width.
        if pills || !self.controller_ready {
            right = right.push(widgets::pill(p, status, if self.controller_ready { p.brand } else { p.yellow }));
        }
        if button {
            right = right.push(widgets::btn(p, perf_name, widgets::ButtonKind::Primary, performance.map(|x| Message::ApplyProfile(x.id))));
        }
        row![brand, links, widgets::hfill(), right].spacing(space::XL).align_y(iced::Alignment::Center).into()
    }

    fn shell(&self, width: f32) -> Element<'_, Message> {
        let p = self.palette;
        let content: Element<Message> = match self.page {
            Page::Dashboard => crate::pages::dashboard::view(self),
            Page::Cpu => crate::pages::cpu::view(self),
            Page::Gpu => crate::pages::gpu::view(self),
            Page::Cooling => crate::pages::cooling::view(self),
            Page::Lighting => crate::pages::lighting::view(self),
            Page::Profiles => crate::pages::profiles::view(self),
            Page::Automation => crate::pages::automation::view(self),
            Page::Settings => crate::pages::settings::view(self),
            Page::Asus => crate::pages::asus::view(self),
        };
        let toast: Element<Message> = match &self.toast {
            Some((msg, ok)) => container(row![widgets::dim(p, msg), widgets::hfill(), iced::widget::button(icons::icon(Icon::Close, p.text_secondary, 14.0)).padding(6).style(widgets::button_style(p, widgets::ButtonKind::Ghost)).on_press(Message::DismissToast)].align_y(iced::Alignment::Center))
                .padding([8, 12])
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(p.surface)),
                    border: iced::Border { color: if *ok { p.brand } else { p.red }, width: 1.0, radius: 0.0.into() },
                    ..Default::default()
                })
                .into(),
            None => iced::widget::Space::new().height(0.0).into(),
        };

        let header = container(self.header(width))
            .padding(iced::Padding::from([10.0, space::XL]))
            .width(Length::Fill)
            .style(move |_| container::Style { background: Some(Background::Color(theme::alpha(p.bg, 0.86))), border: iced::Border { color: p.border_subtle, width: 1.0, radius: 0.0.into() }, ..Default::default() });
        column![header, container(column![toast, content].spacing(space::MD).width(Length::Fill).height(Length::Fill)).padding(space::XL).width(Length::Fill).height(Length::Fill)]
            .spacing(0.0)
            .into()
    }
}

/// Follows the OpenRGB SDK server over one held connection: `true` when the
/// connection is made or the server's device list changes, `false` when the
/// server goes away. Looks for the server again every few seconds while it
/// is down. This is what puts a profile's lighting back after OpenRGB
/// restarts: a liveness probe misses a quick restart, a closed connection
/// does not.
fn rgb_stream() -> impl iced::futures::Stream<Item = bool> {
    use iced::futures::SinkExt;
    iced::stream::channel(4, async move |mut out| {
        loop {
            if let Ok(mut c) = oma_hw::rgb::Client::connect().await {
                let _ = out.send(true).await;
                while c.wait().await == oma_hw::rgb::Link::DevicesChanged {
                    let _ = out.send(true).await;
                }
                let _ = out.send(false).await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    })
}
