//! System events: resume from sleep (logind) and the charger connecting or
//! disconnecting (limits differ on battery) call for re-applying the active
//! profile; a graphics switch starting and the dGPU coming back on the bus
//! (supergfxd) call for staying off the dGPU first; the helper handing back
//! the fans it guarded as it stops calls for sending them again; the power
//! mode changing on asusd or power-profiles-daemon calls for the profile
//! that carries it to follow; CoolerControl's daemon starting or stopping
//! changes who drives the fans.

use iced::futures::stream::{self, BoxStream, Stream, StreamExt};
use iced::futures::SinkExt;
use oma_hw::asusd::{PlatformProfile, PlatformProxy};
use oma_hw::model::Owner;
use oma_hw::ppd::PowerProfilesProxy;
use oma_hw::supergfx::{dgpu_arrived, GfxPower, SuperGfxProxy, UserActionRequired};
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Event {
    Resumed,
    /// Mains power connected (`true`) or disconnected.
    Power(bool),
    /// supergfxd has started a switch: anything but a refusal (a switch that
    /// waits for a logout has started too, and runs at once without a display
    /// manager). It kills whatever holds the dGPU meanwhile.
    GraphicsSwitch,
    /// The dGPU came back on the bus.
    DgpuArrived,
    /// The helper stopped (a package upgrade, Settings → Reinstall helper,
    /// `systemctl stop`) and handed back the fans it guarded.
    FansHandedBack,
    /// The helper ran out of attempts to hand these outputs back: nothing
    /// drives them now.
    RecoveryAbandoned(String),
    /// The power mode changed on the daemon `owner` names: asusd's platform
    /// profile (its label) or power-profiles-daemon's active profile. Sent
    /// for a change made anywhere, this app's own writes included.
    PowerMode { owner: Owner, mode: String },
    /// CoolerControl's daemon started (`true`) or stopped, as its systemd
    /// unit reports. While it runs it drives every fan it knows.
    CoolerControl(bool),
}

#[zbus::proxy(interface = "org.freedesktop.login1.Manager", default_service = "org.freedesktop.login1", default_path = "/org/freedesktop/login1")]
trait Login1 {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.freedesktop.systemd1.Manager", default_service = "org.freedesktop.systemd1", default_path = "/org/freedesktop/systemd1")]
trait SystemdManager {
    /// Ask for unit property changes on this connection; systemd sends none otherwise.
    fn subscribe(&self) -> zbus::Result<()>;
    /// The unit's object, loaded if it isn't yet. A unit that doesn't exist
    /// loads too, as inactive.
    fn load_unit(&self, name: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[zbus::proxy(interface = "org.freedesktop.systemd1.Unit", default_service = "org.freedesktop.systemd1")]
trait SystemdUnit {
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
}

const AC_POLL: Duration = Duration::from_secs(2);

/// Changes only. A property stream opens with the value as it is, which is
/// where things were, not a change; a daemon that isn't running when the
/// stream opens gives one that never yields.
fn changes_only<T: Clone + PartialEq>(values: impl Stream<Item = T>) -> impl Stream<Item = T> {
    values
        .scan(None::<T>, |last, now| {
            let changed = last.as_ref().is_some_and(|l| *l != now);
            *last = Some(now.clone());
            std::future::ready(Some(changed.then_some(now)))
        })
        .filter_map(std::future::ready)
}

/// Whether mains power is connected; `None` without a mains supply (desktops).
fn on_ac() -> Option<bool> {
    let mains: Vec<_> = oma_hw::sysfs::list_dir("/sys/class/power_supply").into_iter().filter(|p| oma_hw::sysfs::read_string(p.join("type")).as_deref() == Some("Mains")).collect();
    (!mains.is_empty()).then(|| mains.iter().any(|p| oma_hw::sysfs::read_string(p.join("online")).as_deref() == Some("1")))
}

pub fn stream() -> impl Stream<Item = Event> {
    iced::stream::channel(8, async move |mut out| {
        let power = stream::unfold(on_ac(), |last| async move {
            loop {
                tokio::time::sleep(AC_POLL).await;
                let now = on_ac();
                if now != last {
                    return Some((now.map(Event::Power), now));
                }
            }
        })
        .filter_map(|e| async move { e });
        let mut sources: Vec<BoxStream<'static, Event>> = vec![power.boxed()];
        let mut watching = vec![if on_ac().is_some() { "charger" } else { "no charger (no mains supply)" }];
        if let Ok(conn) = zbus::Connection::system().await {
            if let Ok(sleep) = async { Login1Proxy::new(&conn).await?.receive_prepare_for_sleep().await }.await {
                // `start = false` is the wake-up half.
                sources.push(sleep.filter_map(|s| async move { s.args().ok().filter(|a| !*a.start()).map(|_| Event::Resumed) }).boxed());
                watching.push("resume");
            }
            if let Ok(gfx) = SuperGfxProxy::new(&conn).await {
                // supergfxd announces a switch right after starting it, with what the
                // user must do; only its refusals mean nothing happens. The signal can
                // lose a race with an early KillNvidia: the mode gate and the settle
                // after the dGPU arrives are the main protection.
                if let Ok(actions) = gfx.receive_notify_action().await {
                    sources.push(actions.filter_map(|s| async move { s.args().ok().filter(|a| UserActionRequired::from_u32(*a.action()).switch_runs()).map(|_| Event::GraphicsSwitch) }).boxed());
                    watching.push("graphics switches");
                }
                // Its status signal also reports every runtime-PM wake and sleep;
                // only the dGPU coming back on the bus matters.
                if let Ok(status) = gfx.receive_notify_gfx_status().await {
                    // It sends changes only: start from what it says now, so the first change counts.
                    let initial = oma_hw::supergfx::state(&conn).await.ok().and_then(|s| s.power);
                    let arrivals = status
                        .scan(initial, |last: &mut Option<GfxPower>, s| {
                            let now = s.args().ok().map(|a| GfxPower::from_u32(*a.status()));
                            let arrived = now.is_some_and(|n| dgpu_arrived(*last, n));
                            if now.is_some() {
                                *last = now;
                            }
                            std::future::ready(Some(arrived))
                        })
                        .filter_map(|arrived| async move { arrived.then_some(Event::DgpuArrived) });
                    sources.push(arrivals.boxed());
                    watching.push("dGPU arrivals");
                }
            }
            // The helper says so as it stops, having handed the fans back. Anyone may
            // send a signal on the system bus, even straight to us, so it's taken
            // only from the owner of the helper's name, which only root can be: a
            // proxy's signal stream follows that owner and drops everyone else's.
            // That's zbus 5's SignalStream: it learns the owner with GetNameOwner
            // (never starting the helper) and follows NameOwnerChanged. Check it
            // still does when bumping zbus.
            if let Ok(changes) = async { oma_hw::helper::HelperProxy::builder(&conn).cache_properties(zbus::proxy::CacheProperties::No).build().await?.receive_changed().await }.await {
                sources.push(
                    changes
                        .filter_map(|s| async move {
                            let what = s.args().ok()?.what().to_string();
                            if what == oma_hw::helper::HANDED_BACK {
                                Some(Event::FansHandedBack)
                            } else {
                                what.strip_prefix(oma_hw::helper::RECOVERY_ABANDONED).map(|rest| Event::RecoveryAbandoned(rest.trim_start_matches(':').trim().to_string()))
                            }
                        })
                        .boxed(),
                );
                watching.push("helper hand-backs");
            }
            // Power modes, as the daemons report them: a change made anywhere (a
            // keyboard shortcut, `powerprofilesctl`, a bar widget) is seen. Both
            // are watched; the app follows the one that owns power modes here.
            if let Ok(p) = PlatformProxy::new(&conn).await {
                let modes = p.receive_platform_profile_changed().await.filter_map(|c| async move { c.get().await.ok().map(|v| PlatformProfile::from_u32(v).label().to_string()) });
                sources.push(changes_only(modes).map(|mode| Event::PowerMode { owner: Owner::Asusd, mode }).boxed());
            }
            if let Ok(p) = PowerProfilesProxy::new(&conn).await {
                let modes = p.receive_active_profile_changed().await.filter_map(|c| async move { c.get().await.ok() });
                sources.push(changes_only(modes).map(|mode| Event::PowerMode { owner: Owner::PowerProfilesDaemon, mode }).boxed());
            }
            watching.push("power modes");
            // CoolerControl's unit: it drives every fan it knows while it runs, so
            // who drives them changes as it starts and stops. An install without
            // the unit (detected by its port instead) loads as inactive and never
            // changes, which leaves what detection found.
            let unit = async {
                let manager = SystemdManagerProxy::new(&conn).await?;
                manager.subscribe().await?;
                let path = manager.load_unit("coolercontrold.service").await?;
                SystemdUnitProxy::builder(&conn).path(path)?.build().await
            };
            if let Ok(unit) = unit.await {
                let running = unit.receive_active_state_changed().await.filter_map(|c| async move { c.get().await.ok().map(|s| s == "active" || s == "reloading") });
                sources.push(changes_only(running).map(Event::CoolerControl).boxed());
                watching.push("CoolerControl");
            }
        }
        tracing::info!(events = %watching.join(", "), "watching system events");
        let mut events = stream::select_all(sources);
        while let Some(e) = events.next().await {
            if out.send(e).await.is_err() {
                break;
            }
        }
    })
}
