<p align="center">
  <img src="docs/brand/identity.png" alt="OmaAsus — precision hardware control" width="900">
</p>

<h1 align="center">OmaAsus</h1>

<p align="center">
  A control center and Hyprland overlay for ASUS hardware on Linux, written in Rust.<br>
  Profiles, power, fans, CPU, GPU, lighting and automation for ROG desktops and laptops, styled after <a href="https://omarchy.org">Omarchy</a> and coloured by your active Omarchy theme.
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#supported-hardware">Supported hardware</a> ·
  <a href="#what-it-controls">What it controls</a> ·
  <a href="#design">Design</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#cli">CLI</a> ·
  <a href="#safety">Safety</a>
</p>

---

## Why

Windows has Armoury Crate. On Linux the same ground is covered by separate daemons: `asusd` for ROG laptops, `supergfxd` for graphics modes, CoolerControl for fans, OpenRGB for lighting, `power-profiles-daemon` for power, NVML for NVIDIA cards. OmaAsus puts one interface over them. It works out what the machine is, uses whichever of those it finds, and shows only the controls that exist. Root-only work goes through a single helper behind polkit.

It runs as a window, as a tray item, and as a layer-shell overlay you toggle over a game with one key.

## Supported hardware

| Area | What OmaAsus drives | Through |
|---|---|---|
| ROG laptops | Power modes, fan curves per power mode, charge limit, firmware attributes (PPT limits, dGPU TGP, panel overdrive and others), keyboard Aura lighting, the Slash LED bar | `asusd` 6.x. Where `asusd` doesn't manage the fan curves, one curve per fan is written to `asus_custom_fan_curve` (untested) |
| Graphics modes | Integrated, Hybrid, VFIO, MUX and eGPU | `supergfxd` 5.x |
| ASUS desktop boards | Nuvoton Super I/O fan headers, including the board's own Smart Fan IV curves; VRM, board and coolant temperatures; an experimental text readout on the LiveDash OLED of ROG Extreme boards | `nct6775`, `asus-ec-sensors`, HID |
| Coolers | ROG Ryujin II 360 pump and fans (tested on the EVA edition; the III should work through the same driver, untested); Lian Li UNI FAN hubs (the desktop's hub is tested, other models are recognised by product id but untested) | `asus_rog_ryujin`, HID |
| NVIDIA GPUs | Power limit, clock lock and offsets (driver 555+), fans, persistence. One NVIDIA GPU: with two, the first on the bus is the one tuned | NVML |
| AMD GPUs | DPM performance level, temperature, power | amdgpu sysfs |
| CPUs | Governor, EPP, SMT, frequency limits, temperatures; boost where the driver offers it (`amd-pstate` from Linux 6.11, `acpi-cpufreq`); package power from an APU's reported power, or RAPL where the kernel lets users read it | cpufreq (`amd-pstate`, `acpi-cpufreq`, `intel_pstate`), hwmon, powercap |
| Everything else | Any hwmon fan output and sensor, OpenRGB devices, power modes | hwmon, the OpenRGB SDK, `power-profiles-daemon` or ACPI `platform_profile` |

It also works with CoolerControl, which can own the fans instead (while it runs it drives every fan it knows, hubs included, and OmaAsus only switches its Mode per profile), GameMode and Hyprland IPC. None of these is required. A machine without a component doesn't get its controls. The ROG Ally goes through the same `asusd` interfaces but hasn't been tried.

### Tested on

| Machine | Exercised |
|---|---|
| ROG Crosshair X670E Extreme, Ryzen 9 7950X, RTX 4090, ROG Ryujin II 360 EVA, Lian Li UNI FAN hub | With OmaAsus 0.1: board headers and Smart Fan IV curves, the Ryujin, the Lian Li hub, NVIDIA tuning, OpenRGB. The current build, with the hardware model, hasn't been run there yet. This Ryujin's id isn't upstream, so the `asus_rog_ryujin` driver is bound to it by hand |
| ROG Zephyrus G14 GA403WR, Ryzen AI 9 HX 370, Radeon 890M, NVIDIA dGPU | `asusd` fan curves written and read back with `oma curves`; keyboard Aura and the Slash bar written and read back with `oma lighting`; the amdgpu DPM level set from the Graphics page; firmware attributes and charge limit read and shown; `supergfxd` observed in Integrated mode. Not yet: power-mode, firmware-attribute and charge-limit writes, fan curves and lighting from the GUI, graphics switches, NVIDIA tuning |

Both run Arch with Omarchy.

### Other machines

Behaviour isn't keyed to those two machines. At start OmaAsus builds a model of the hardware from what the kernel and the daemons report, and each page shows what that model holds. Facts about specific hardware that no probe can report, such as a laptop fan's top speed or a pump's safe minimum, are kept in `crates/oma-hw/src/knowledge.rs`. `oma model` prints the model with the facts it used and where each came from.

If detection or a stored fact is wrong for your machine, correct it in `~/.config/omaasus/quirks.toml`:

```toml
fan_max_rpm = { CPU = 6400 }          # top speed by fan name
min_duty = { "ryujin:pump" = 70.0 }   # by output id, in percent
hide = ["hwmon:acpitz:temp1"]         # leave out of the model
```

`hide` removes an output, sensor or device from the model, so from the Cooling outputs, curve sources and the hardware summary; the live lists on the dashboard still show every sensor. `min_duty` replaces the built-in floor, and setting a pump below its safe minimum is at your own risk.

`oma capture [dir]` saves a machine's raw inventory, with GPU UUIDs redacted, as a test fixture, and `oma model --from` rebuilds the model from one. For a machine OmaAsus handles badly, a capture is the most useful thing to attach to a report.

## What it controls

| Page | Controls |
|---|---|
| **Dashboard** | Active profile and quick switch; CPU and GPU temperature; coolant where there is a coolant sensor, fan speed otherwise; power against the GPU's reported limit or the most the package has drawn; load history; fans as live, stale, stopped or offline; other temperatures |
| **Processor** | Governor, energy-performance preference, core boost, SMT, frequency ceiling and floor, per-core clocks and load with preferred-core ranking |
| **Graphics** | NVIDIA power limit, locked clocks, core and memory offsets (driver 555+), fan speed, persistence, throttle reasons; amdgpu DPM level; the dGPU's state when it is asleep or switched off |
| **Cooling** | Who drives the fans (OmaAsus, CoolerControl or firmware). Per output: automatic, fixed, software curve, the board's Smart Fan IV curve, or a laptop's firmware curve stored per power mode. Curve editor with hysteresis and ramp limiting; temperature sources from the sensors present |
| **Lighting** | `asusd` keyboard Aura (effects, colour, brightness) and Slash bar (animations, brightness, when it shows); OpenRGB devices (colour, effects, thermal glow). Remembered per profile. A solid colour uses the device's own Static mode and is saved to the device where it allows, so it outlives OpenRGB; the active profile's lighting is put back whenever the OpenRGB server starts |
| **Profiles** | Create, duplicate, rename, recolour, delete, set default. A profile holds a power mode, CPU, GPU, cooling, lighting and a CoolerControl Mode, and can carry firmware limits set in `config.toml`. The first run creates one per power mode the machine has |
| **Automation** | Manual or automatic mode; rules on GameMode, fullscreen game, window class, process name, CPU/GPU temperature, time of day; priorities and hold times |
| **ASUS** | Power mode, charge limit, firmware attributes read live from the kernel and applied on release, `supergfxd` graphics mode with a confirmation that says what the switch involves |
| **Settings** | Helper install, CoolerControl credentials, overlay and tray, telemetry rate, a LiveDash OLED test where there is one, a summary of the detected hardware and the knowledge it used |

Graphics, Cooling and ASUS appear only when there is hardware or a daemon behind them; Lighting also appears when OpenRGB is installed, since that page is where its server is started.

<p align="center">
  <img src="docs/screenshots/cooling.png" alt="Cooling page with the curve editor" width="900">
</p>

## Design

The original OA monogram pairs a split, chamfered enclosure with an angular A. [Brand assets and usage](docs/brand/README.md) include scalable artwork and the monochrome tray version.

The interface is a port of [omarchy-site](https://github.com/omacom/omarchy-site)'s design system:

- **Geist** for headings and controls, **JetBrains Mono** for navigation, labels, values and copy. Both are under the SIL Open Font License; the licences are next to the fonts in `crates/oma-gui/assets/fonts`.
- The site's token roles: `bg-deep`, `bg`, `surface`, `surface-2`, `border-subtle`/`strong`, `text`/`secondary`/`muted`, `brand`, `brand-ink`, and the five field bands. Zero corner radius. Opaque surfaces with a one-pixel elevation ring. Brand-filled primary buttons.
- **Your Omarchy theme drives the colours.** On start, OmaAsus asks `omarchy-theme-current` and reads that theme's `colors.toml`, mixing intermediate shades the way the site does. Change theme and the app follows at once, cross-fading to the new colours.
- The background is the site's **pixel field** as a GPU shader: 10 px cells, Bayer-dithered drifting blobs, corner clustering, a cursor halo, and a subtle pulse with system load.
- The active profile is set in the site's **3×5 pixel glyph font** with the five brand bands, and the header carries the original OmaAsus OA monogram.
- The dashboard fills the window and its type scale follows it. On narrow tiles the navigation folds to icons.

<p align="center">
  <img src="docs/screenshots/overlay.png" alt="Layer-shell overlay" width="360">
  &nbsp;&nbsp;
  <img src="docs/screenshots/lighting.png" alt="Lighting page" width="520">
</p>

## Install

Requirements: Wayland, a Vulkan-capable GPU, Hyprland for the overlay and automation signals. Arch is the tested platform.

### From a release

Each [release](https://github.com/Mhsbrian/OmaAsus/releases) carries a pacman package and a portable archive. Both install the helper too.

Arch and Omarchy:

```sh
sudo pacman -U omaasus-<version>-1-x86_64.pkg.tar.zst
```

If you installed the helper by hand before, pacman stops on the files that already exist. Remove the hand install first with `sudo scripts/install-helper.sh --uninstall` (it refuses to touch files a package owns), then install the package.

Other distributions (systemd, polkit, and the glibc version the release notes give):

```sh
tar xf omaasus-<version>-x86_64-linux.tar.gz
sudo omaasus-<version>-x86_64-linux/install.sh
```

`install.sh --uninstall` removes it again.

### From source

Requires Rust 1.98+.

```sh
git clone https://github.com/Mhsbrian/OmaAsus
cd OmaAsus
cargo build --release
```

Binaries land in `target/release`: `omaasus` (GUI), `oma-helper` (root service), `oma` (CLI; some subcommands write to hardware). `packaging/arch/build.sh` turns a checkout into a pacman package; [packaging/README.md](packaging/README.md) covers that and the release process.

### The helper

Fans, governors, GPU limits and lighting controllers sit behind root-only sysfs and hidraw nodes. OmaAsus ships one small system service, `com.omaasus.Helper1`, gated by two polkit actions:

- `com.omaasus.helper.control`: hwmon and NVIDIA fans, CPU governor, EPP, boost and limits, platform profile, firmware attributes and power limits, NVIDIA power limit and clock lock, the amdgpu DPM level. Allowed for the active local session without a prompt, like power-profiles-daemon.
- `com.omaasus.helper.advanced`: GPU clock offsets, SMT, the GPU MUX, dGPU and eGPU switches, amdgpu overdrive, and HID writes, which include Lian Li hub fan speeds and the LiveDash. Asks for your password; polkit then remembers it for a few minutes.

The release packages install it. With a source build, install it from **Settings → Install helper** (runs through `pkexec`), or by hand:

```sh
sudo scripts/install-helper.sh target/release/oma-helper crates/oma-helper/data
```

A helper installed this way is not owned by pacman, so the package later refuses to install over it. Before switching to the package, remove it:

```sh
sudo scripts/install-helper.sh --uninstall
```

That installs the binary, D-Bus policy, polkit actions, the systemd unit, and udev rules giving your user access to Aura, Ryujin, Lian Li and LiveDash devices. Run it again after pulling changes to the helper.

D-Bus starts the helper when OmaAsus first needs it; it isn't enabled at boot. Its device access is granted by driver group (`char-nvidia`, `char-hidraw` and so on), which systemd resolves when the helper starts. So the helper exits after five idle minutes, never while it guards a client's fans, and the next start picks up an NVIDIA driver loaded in the meantime, as after a switch to Hybrid. If a profile ever reports `cannot open the GPU through NVML`, run `sudo systemctl restart oma-helper`.

### Hyprland

Hyprland 0.56+ (Lua config), append to `~/.config/hypr/bindings.lua`:

```lua
o.bind("SUPER + F12", "OmaAsus overlay", hl.dsp.exec({ cmd = "omaasus toggle" }))
o.bind("SUPER + CTRL + G", "Performance profile", hl.dsp.exec({ cmd = "omaasus profile performance" }))
-- Omarchy dims every window to 0.985/0.96 opacity; keep OmaAsus opaque:
o.window({ title = "^OmaAsus$" }, { opacity = "1 1" })
```

Older Hyprland: see `packaging/hyprland.conf` (packages install both snippets under `/usr/share/omaasus`). Start the overlay daemon at login with the user unit, `packaging/omaasus.service`, which the packages install:

```sh
systemctl --user enable --now omaasus
```

### Tray

OmaAsus registers a StatusNotifierItem, so Omarchy's bar shows the OmaAsus OA monogram in its tray. Closing the main window then leaves the daemon running: automation rules, the fan engine and the overlay keep working, and the icon is the way back.

| Click | Action |
| --- | --- |
| Left | Drop the panel down under the bar (click again to fold it up) |
| Middle | Open the main window |
| Right | Menu: window, panel, profile switch, quit |

The icon is symbolic, so the bar recolours it to the current theme. A package installs it under `/usr/share/icons/hicolor`; a `cargo` build writes its own copies on first start. Turn the tray off under Settings if your bar has no StatusNotifier host.

### Optional integrations

- **CoolerControl**: if its daemon is running, OmaAsus leaves fan curves to it by default and activates a CoolerControl *Mode* per profile. Enter the CCAdmin password under Settings.
- **OpenRGB**: start `openrgb --server`, or press the button on the Lighting page. The client speaks the SDK protocol natively.
- **GameMode**: registered games trigger automation rules.

## Architecture

```
crates/
  oma-hw/      unprivileged hardware layer: detection (capture, then the hardware
               model), the knowledge base, hwmon, cpufreq, NVML, amdgpu, asusd,
               supergfxd, power-profiles-daemon, CoolerControl, OpenRGB, Lian Li,
               LiveDash, profiles and the software fan engine
  oma-helper/  root D-Bus service: polkit, sysfs allow-list, NVML apply, fan watchdog
  oma-gui/     omaasus: iced 0.14 window and layer-shell overlay, profile apply
               pipeline, automation, tray
  oma-cli/     oma: model, inventory, sensors, fan curves, lighting, capture
research/      references the knowledge base cites (asusctl, supergfxctl, G-Helper),
               with live-verified corrections
```

Hardware is read on sampler and reader threads and written through the helper, the owning daemon over D-Bus, or background tasks; the UI thread renders what those report. A persistent registry keeps every channel live, stale or offline, never missing.

## CLI

```
omaasus                       open the window (also starts the overlay daemon)
omaasus --overlay             start headless; the overlay waits for a toggle
omaasus toggle | show | hide  control the overlay from a keybind
omaasus window                open the window (starts the app if nothing runs)
omaasus profile <name>        apply a profile by name
omaasus page <name>           jump to a page
omaasus quit                  stop the daemon and drop the tray item

oma model [--json] [--from f] the hardware model and the knowledge it used
oma inventory [--json]        raw detection
oma sensors                   every hwmon reading
oma watch [secs]              live CPU/GPU line
oma curves [set | off]        asusd fan curves per power mode (writes)
oma lighting [show]           asusd keyboard and Slash lighting (show writes)
oma capture [dir]             save this machine's inventory as a test fixture
oma nvidia | cpu | daemons | rgb [--set index]   (rgb --set writes)
```

## Safety

- The helper writes only allow-listed sysfs attributes, refused before polkit is asked, and HID reports only to ASUS and ENE devices.
- Releasing a fan output, on exit or when a profile stops driving it, restores the saved `pwm_enable` mode on hwmon outputs, returns GPU fans to automatic and re-enables PWM sync on Lian Li channels. The Ryujin falls back to a safe fixed duty.
- If OmaAsus dies instead, the helper restores the hwmon outputs it had changed, and NVIDIA fans where the GPU is awake. A restore that fails is kept and retried for about two minutes; if it still fails the helper logs it, tells a running OmaAsus (which shows a persistent warning), and reports it through `RecoveryReport`. Lian Li channels aren't covered: a crash while a hub channel is under manual control leaves it at its last speed until OmaAsus runs again or the hub is power-cycled.
- A software fan curve that can't read its temperature holds its speed for 30 seconds, then runs at its top. Fan evaluation runs on its own clock, and a reading older than four seconds no longer counts, so a stalled sensor path cannot leave a curve acting on old numbers or stop the fallback.
- OmaAsus opens the NVIDIA GPU only in graphics modes that use it, lets go when `supergfxd` announces a switch, leaves it alone for 15 seconds after it comes back on the bus, and never wakes a sleeping one to read it.
- On desktops without an internal panel, `supergfxd`'s Integrated mode isn't offered; it would unbind your display GPU. Graphics switches ask first and say what they involve.
- GPU clock offsets apply to the P0 VF curve. Start small and check stability.
- A profile is a complete GPU state. One without a power limit restores the stock limit where the card allows it, so a Quiet profile's cap doesn't carry over into Performance.
- CPU boost is written per policy where the kernel offers it. A global boost of 0 made `power-profiles-daemon` fail every switch out of power-saver.
- Every hwmon device and fan hub has its own bounded reader: one that stops answering holds a single outstanding read, is shown as silent in Cooling and Settings, and cannot slow the others or the app. A reading that arrives late is dated when it was asked for and never drives a fan curve. CPU, NVML and amdgpu reads still run inline in the sampler; if one of those blocks, frames stop and fan control falls back after four seconds. The helper likewise runs each device's writes in their own lane, so a wedged device fails its own calls as busy instead of freezing the helper.
- Closing the window exits unless a bar shows the tray item, the overlay is open, or OmaAsus was started with `--overlay`.

## License

MIT.
