# Screen Color

Adjusts a monitor's color temperature on Windows by changing the GPU's gamma ramp,
the same approach f.lux uses. Each display is adjusted on its own, and the monitor
doesn't need DDC/CI support.

`screen-color.exe` is self-contained: it uses only DLLs that come with Windows, so you
can copy it anywhere. Download it from [Releases](https://github.com/ccdr4gon/screen-color/releases/latest),
or build it as described under [Building](#building).

## Usage

**Window:** double-click `screen-color.exe`. Pick a display, then drag the slider or click
a preset. It opens on your external display by default.

**Command line:**

```
screen-color.exe list              # displays and their current temperature
screen-color.exe set 4500          # external display(s) to 4500 K
screen-color.exe set 5000 -d 2     # by number from `list`...
screen-color.exe set 5000 -d dell  # ...or part of the monitor name, or "all"
screen-color.exe reset             # every display back to neutral (6500 K)
screen-color.exe restore           # re-apply the saved temperatures
```

6500 K is neutral; lower is warmer (more orange), higher is cooler (bluer).
Settings are saved to `%APPDATA%\screen_color\settings.json`. If the window is open when you
use the command line, it picks up the change within 2 seconds.

## Things to know

- **How warm it can go is limited by Windows.** Windows rejects gamma ramps that move
  any level more than about half of the full range from neutral. Even with the
  `GdiIcmGammaRange` registry value set to 256, the warmest it accepts is about **3300 K**.
  If you ask for less, the program applies the warmest value Windows accepts and tells you.
  Without that registry value the limit can be narrower.
- **Windows resets the gamma ramp** when you restart, and also after sleep, display
  reconnects, resolution changes and some full-screen games.
  - Tick **Apply my colors when I sign in** in the window to have your colors re-applied
    each time you sign in. This adds a "Screen Color" entry to Task Manager's Startup apps
    page, where you can also turn it off. If you move `screen-color.exe`, untick and tick
    the box again so the entry points to the new location.
  - While the window is open (minimized is fine), it checks every 2 seconds and re-applies
    your setting after sleep and the other resets.
- **HDR and Night Light:** gamma ramps have no effect while HDR is on for a display, and
  Windows Night Light can conflict with them. Turn those off for the display you adjust.
- **Console window:** on Windows 11 24H2 and later, starting the exe from Explorer or a
  shortcut shows no console window. On older versions a console window flashes briefly
  when you double-click it.
- **Why not the monitor's own settings?** Changing a monitor's hardware color settings
  needs DDC/CI, which many monitors don't support or have turned off. The gamma ramp works
  with any monitor.

## Building

The exe is written in Rust (`src/`). With Rust installed from [rustup.rs](https://rustup.rs):

```
cargo build --release
```

The result is `target\release\screen-color.exe`. `screen-color.manifest` is embedded at
build time; it turns on themed controls, per-monitor DPI scaling and the console policy above.

## Python version

`screen_color.py` is the original Python version, with the same features and settings file:
run `python screen_color.py` with the same commands, or double-click `launch_gui.pyw` for the
window. The two versions can be used interchangeably.

## License

MIT. See [LICENSE](LICENSE).
