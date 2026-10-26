"""Adjust a monitor's color temperature on Windows.

Works through the GPU's gamma ramp (the same mechanism f.lux uses), so it needs no
DDC/CI support from the monitor and only affects the display you pick.

    python screen_color.py               open the window
    python screen_color.py list          list displays and their current temperature
    python screen_color.py set 4500      set the external display(s) to 4500 K
    python screen_color.py set 5000 -d 2
    python screen_color.py reset         every display back to neutral (6500 K)
    python screen_color.py restore       re-apply the saved temperatures (e.g. at login)
"""

import argparse
import copy
import ctypes
import json
import os
import re
import sys
import tkinter as tk
import winreg
from contextlib import contextmanager
from ctypes import wintypes
from dataclasses import dataclass
from tkinter import ttk

NEUTRAL_K = 6500
MIN_K = 1900
MAX_K = 10000

SETTINGS_PATH = os.path.join(os.environ.get("APPDATA", os.path.expanduser("~")),
                             "screen_color", "settings.json")


# --- Win32 bindings ---------------------------------------------------------

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)

QDC_ONLY_ACTIVE_PATHS = 0x2
ERROR_INSUFFICIENT_BUFFER = 122
DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME = 1
DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME = 2
DISPLAYCONFIG_MODE_INFO_SIZE = 64  # mode info is only passed through, never read
DISPLAY_DEVICE_PRIMARY_DEVICE = 0x4

OUTPUT_TECHNOLOGY = {
    0xFFFFFFFF: "Other", 0: "VGA", 1: "S-Video", 2: "Composite", 3: "Component",
    4: "DVI", 5: "HDMI", 6: "LVDS", 8: "D-Jpn", 9: "SDI", 10: "DisplayPort",
    11: "Embedded DisplayPort", 12: "UDI", 13: "Embedded UDI", 14: "SDTV dongle",
    15: "Miracast", 16: "Indirect (wired)", 17: "Indirect (virtual)",
    18: "DisplayPort (USB tunnel)", 0x80000000: "Internal",
}
INTERNAL_TECHNOLOGIES = {6, 11, 13, 0x80000000}


class LUID(ctypes.Structure):
    _fields_ = [("LowPart", wintypes.DWORD), ("HighPart", wintypes.LONG)]


class DISPLAYCONFIG_RATIONAL(ctypes.Structure):
    _fields_ = [("Numerator", ctypes.c_uint32), ("Denominator", ctypes.c_uint32)]


class DISPLAYCONFIG_PATH_SOURCE_INFO(ctypes.Structure):
    _fields_ = [("adapterId", LUID), ("id", ctypes.c_uint32),
                ("modeInfoIdx", ctypes.c_uint32), ("statusFlags", ctypes.c_uint32)]


class DISPLAYCONFIG_PATH_TARGET_INFO(ctypes.Structure):
    _fields_ = [("adapterId", LUID), ("id", ctypes.c_uint32), ("modeInfoIdx", ctypes.c_uint32),
                ("outputTechnology", ctypes.c_uint32), ("rotation", ctypes.c_uint32),
                ("scaling", ctypes.c_uint32), ("refreshRate", DISPLAYCONFIG_RATIONAL),
                ("scanLineOrdering", ctypes.c_uint32), ("targetAvailable", wintypes.BOOL),
                ("statusFlags", ctypes.c_uint32)]


class DISPLAYCONFIG_PATH_INFO(ctypes.Structure):
    _fields_ = [("sourceInfo", DISPLAYCONFIG_PATH_SOURCE_INFO),
                ("targetInfo", DISPLAYCONFIG_PATH_TARGET_INFO), ("flags", ctypes.c_uint32)]


class DISPLAYCONFIG_DEVICE_INFO_HEADER(ctypes.Structure):
    _fields_ = [("type", ctypes.c_uint32), ("size", ctypes.c_uint32),
                ("adapterId", LUID), ("id", ctypes.c_uint32)]


class DISPLAYCONFIG_SOURCE_DEVICE_NAME(ctypes.Structure):
    _fields_ = [("header", DISPLAYCONFIG_DEVICE_INFO_HEADER),
                ("viewGdiDeviceName", wintypes.WCHAR * 32)]


class DISPLAYCONFIG_TARGET_DEVICE_NAME(ctypes.Structure):
    _fields_ = [("header", DISPLAYCONFIG_DEVICE_INFO_HEADER), ("flags", ctypes.c_uint32),
                ("outputTechnology", ctypes.c_uint32), ("edidManufactureId", ctypes.c_uint16),
                ("edidProductCodeId", ctypes.c_uint16), ("connectorInstance", ctypes.c_uint32),
                ("monitorFriendlyDeviceName", wintypes.WCHAR * 64),
                ("monitorDevicePath", wintypes.WCHAR * 128)]


class DISPLAY_DEVICEW(ctypes.Structure):
    _fields_ = [("cb", wintypes.DWORD), ("DeviceName", wintypes.WCHAR * 32),
                ("DeviceString", wintypes.WCHAR * 128), ("StateFlags", wintypes.DWORD),
                ("DeviceID", wintypes.WCHAR * 128), ("DeviceKey", wintypes.WCHAR * 128)]


user32.GetDisplayConfigBufferSizes.argtypes = [ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32),
                                               ctypes.POINTER(ctypes.c_uint32)]
user32.QueryDisplayConfig.argtypes = [ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32), ctypes.c_void_p,
                                      ctypes.POINTER(ctypes.c_uint32), ctypes.c_void_p, ctypes.c_void_p]
user32.DisplayConfigGetDeviceInfo.argtypes = [ctypes.c_void_p]
user32.EnumDisplayDevicesW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, ctypes.c_void_p, wintypes.DWORD]
gdi32.CreateDCW.restype = wintypes.HDC
gdi32.CreateDCW.argtypes = [wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.LPCWSTR, ctypes.c_void_p]
gdi32.DeleteDC.argtypes = [wintypes.HDC]
gdi32.SetDeviceGammaRamp.argtypes = [wintypes.HDC, ctypes.c_void_p]
gdi32.GetDeviceGammaRamp.argtypes = [wintypes.HDC, ctypes.c_void_p]

GammaRamp = (wintypes.WORD * 256) * 3


# --- Displays ---------------------------------------------------------------

@dataclass
class Display:
    index: int        # 1-based, as shown by `list`
    gdi_name: str     # e.g. \\.\DISPLAY2, what the gamma API takes
    name: str         # monitor name from its EDID
    connection: str
    internal: bool
    primary: bool
    device_path: str  # stable per monitor and port; the key for saved settings

    @property
    def kind(self):
        return ("built-in" if self.internal else "external") + (", primary" if self.primary else "")

    @property
    def label(self):
        details = self.kind if self.internal else f"{self.connection}, {self.kind}"
        return f"{self.index}: {self.name} ({details})"


def _primary_gdi_name():
    dd = DISPLAY_DEVICEW(cb=ctypes.sizeof(DISPLAY_DEVICEW))
    i = 0
    while user32.EnumDisplayDevicesW(None, i, ctypes.byref(dd), 0):
        if dd.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE:
            return dd.DeviceName
        i += 1
    return None


def _device_info(info, kind, adapter_id, target_id):
    info.header.type = kind
    info.header.size = ctypes.sizeof(info)
    info.header.adapterId = adapter_id
    info.header.id = target_id
    return user32.DisplayConfigGetDeviceInfo(ctypes.byref(info)) == 0


def list_displays():
    """Active displays, ordered by their \\\\.\\DISPLAYn number."""
    n_paths, n_modes = ctypes.c_uint32(), ctypes.c_uint32()
    for _ in range(3):  # buffer sizes can change if a display is plugged in between calls
        err = user32.GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, ctypes.byref(n_paths),
                                                 ctypes.byref(n_modes))
        if err:
            raise ctypes.WinError(err)
        paths = (DISPLAYCONFIG_PATH_INFO * n_paths.value)()
        modes = (ctypes.c_byte * (DISPLAYCONFIG_MODE_INFO_SIZE * n_modes.value))()
        err = user32.QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, ctypes.byref(n_paths), ctypes.byref(paths),
                                        ctypes.byref(n_modes), ctypes.byref(modes), None)
        if err != ERROR_INSUFFICIENT_BUFFER:
            break
    if err:
        raise ctypes.WinError(err)

    primary = _primary_gdi_name()
    displays = []
    for path in paths[:n_paths.value]:
        source = DISPLAYCONFIG_SOURCE_DEVICE_NAME()
        target = DISPLAYCONFIG_TARGET_DEVICE_NAME()
        if not (_device_info(source, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                             path.sourceInfo.adapterId, path.sourceInfo.id)
                and _device_info(target, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                                 path.targetInfo.adapterId, path.targetInfo.id)):
            continue
        tech = target.outputTechnology
        gdi_name = source.viewGdiDeviceName
        displays.append(Display(
            index=0,
            gdi_name=gdi_name,
            name=target.monitorFriendlyDeviceName or gdi_name.lstrip("\\.").title(),
            connection=OUTPUT_TECHNOLOGY.get(tech, f"type {tech}"),
            internal=tech in INTERNAL_TECHNOLOGIES,
            primary=gdi_name == primary,
            device_path=target.monitorDevicePath or gdi_name,
        ))

    displays.sort(key=lambda d: int(re.sub(r"\D", "", d.gdi_name) or 0))
    for i, d in enumerate(displays, 1):
        d.index = i
    return displays


def default_displays(displays):
    """External displays if there are any, otherwise the primary one."""
    return ([d for d in displays if not d.internal]
            or [d for d in displays if d.primary]
            or displays[:1])


# --- Color math -------------------------------------------------------------

def _planckian_xy(kelvin):
    """CIE 1931 xy chromaticity of a black body (Kim et al. 2002 cubic spline, 1667-25000 K)."""
    t = min(max(kelvin, 1667.0), 25000.0)
    if t <= 4000:
        x = -0.2661239e9 / t**3 - 0.2343589e6 / t**2 + 0.8776956e3 / t + 0.179910
    else:
        x = -3.0258469e9 / t**3 + 2.1070379e6 / t**2 + 0.2226347e3 / t + 0.240390
    if t <= 2222:
        y = -1.1063814 * x**3 - 1.34811020 * x**2 + 2.18555832 * x - 0.20219683
    elif t <= 4000:
        y = -0.9549476 * x**3 - 1.37418593 * x**2 + 2.09137015 * x - 0.16748867
    else:
        y = 3.0817580 * x**3 - 5.87338670 * x**2 + 3.75112997 * x - 0.37001483
    return x, y


def _linear_srgb(kelvin):
    x, y = _planckian_xy(kelvin)
    X, Y, Z = x / y, 1.0, (1 - x - y) / y
    return (3.2404542 * X - 1.5371385 * Y - 0.4985314 * Z,
            -0.9692660 * X + 1.8760108 * Y + 0.0415560 * Z,
            0.0556434 * X - 0.2040259 * Y + 1.0572252 * Z)


def channel_gains(kelvin):
    """Red/green/blue multipliers for the gamma ramp; exactly (1, 1, 1) at 6500 K."""
    rgb = [max(c / ref, 0.0) for c, ref in zip(_linear_srgb(kelvin), _linear_srgb(NEUTRAL_K))]
    peak = max(rgb)
    # The white point is scaled in linear light, but the ramp works on gamma-encoded values.
    return tuple((c / peak) ** (1 / 2.2) for c in rgb)


def build_ramp(kelvin):
    ramp = GammaRamp()
    for channel, gain in enumerate(channel_gains(kelvin)):
        for i in range(256):
            ramp[channel][i] = min(65535, round(i * 257 * gain))
    return ramp


def swatch_color(kelvin):
    return "#%02x%02x%02x" % tuple(round(255 * g) for g in channel_gains(kelvin))


# --- Gamma ramp -------------------------------------------------------------

@contextmanager
def _display_dc(gdi_name):
    hdc = gdi32.CreateDCW(None, gdi_name, None, None)
    if not hdc:
        raise OSError(f"Cannot open {gdi_name}")
    try:
        yield hdc
    finally:
        gdi32.DeleteDC(hdc)


def apply_temperature(gdi_name, kelvin):
    """Set a display's color temperature and return the temperature actually applied.

    Windows rejects gamma ramps that stray too far from neutral. If the requested value
    is refused, the warmest (or coolest) value Windows still accepts is used instead.
    """
    kelvin = round(kelvin)
    with _display_dc(gdi_name) as hdc:
        def accepted(k):
            return bool(gdi32.SetDeviceGammaRamp(hdc, ctypes.byref(build_ramp(k))))

        if accepted(kelvin):
            return kelvin
        if not accepted(NEUTRAL_K):
            raise OSError(f"Windows refused to change the gamma ramp of {gdi_name}. "
                          "HDR may be on for this display, or its driver doesn't allow it.")
        good, bad = NEUTRAL_K, kelvin
        while abs(good - bad) > 10:
            mid = (good + bad) // 2
            if accepted(mid):
                good = mid
            else:
                bad = mid
        # A rejected call leaves the ramp alone, so the last accepted value (good) is in effect.
        return good


def ramp_matches(gdi_name, kelvin):
    current = GammaRamp()
    with _display_dc(gdi_name) as hdc:
        if not gdi32.GetDeviceGammaRamp(hdc, ctypes.byref(current)):
            return False
    return bytes(current) == bytes(build_ramp(kelvin))


# --- Saved settings ---------------------------------------------------------

def load_settings(strict=False):
    """The saved settings. If the file exists but can't be read right now, returns None
    when `strict`, otherwise the defaults."""
    try:
        with open(SETTINGS_PATH, encoding="utf-8") as f:
            settings = json.load(f)
    except FileNotFoundError:
        settings = {}
    except OSError:
        if strict:
            return None
        settings = {}
    except ValueError:
        settings = {}
    settings.setdefault("temperatures", {})
    return settings


def save_settings(settings):
    os.makedirs(os.path.dirname(SETTINGS_PATH), exist_ok=True)
    # Write a temporary file and swap it in, so other programs never read a half-written file.
    temp = SETTINGS_PATH + ".tmp"
    with open(temp, "w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
    os.replace(temp, SETTINGS_PATH)


def merge_settings(base, ours, theirs):
    """Three-way merge for when this program and another (say the window and
    `screen_color.py set`) both changed the settings since they were read as `base`.
    The result has both sets of changes; where both changed a display, `ours` wins."""
    merged = copy.deepcopy(theirs)
    for path in set(base["temperatures"]) | set(ours["temperatures"]):
        mine = ours["temperatures"].get(path)
        if mine != base["temperatures"].get(path):
            if mine is None:
                merged["temperatures"].pop(path, None)
            else:
                merged["temperatures"][path] = mine
    if ours.get("selected") != base.get("selected"):
        merged["selected"] = ours.get("selected")
    return merged


def remember(settings, display, kelvin):
    # Neutral displays aren't stored, so they're left alone for other apps to manage.
    if kelvin == NEUTRAL_K:
        settings["temperatures"].pop(display.device_path, None)
    else:
        settings["temperatures"][display.device_path] = kelvin


# --- Sign-in ----------------------------------------------------------------

RUN_KEY = r"Software\Microsoft\Windows\CurrentVersion\Run"
# Where Task Manager's Startup apps page records the entries it turned off.
APPROVED_KEY = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
STARTUP_ENTRY = "Screen Color"


def startup_enabled():
    """Whether Windows will run `restore` when the user signs in."""
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY) as key:
            winreg.QueryValueEx(key, STARTUP_ENTRY)
    except OSError:
        return False
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, APPROVED_KEY) as key:
            flags, _ = winreg.QueryValueEx(key, STARTUP_ENTRY)
    except OSError:
        return True
    # The first byte is even for entries that are on and odd for ones turned off.
    return not (flags and flags[0] & 1)


def set_startup(enabled):
    """Add or remove the sign-in entry that runs this script's `restore` command."""
    # Clear any "turned off" mark from Task Manager, so the choice made here takes effect.
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, APPROVED_KEY, 0, winreg.KEY_SET_VALUE) as key:
            winreg.DeleteValue(key, STARTUP_ENTRY)
    except OSError:
        pass
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY, 0, winreg.KEY_SET_VALUE) as key:
        if enabled:
            # pythonw runs the script without a console window.
            pythonw = os.path.join(os.path.dirname(sys.executable), "pythonw.exe")
            python = pythonw if os.path.exists(pythonw) else sys.executable
            command = f'"{python}" "{os.path.abspath(__file__)}" restore'
            winreg.SetValueEx(key, STARTUP_ENTRY, 0, winreg.REG_SZ, command)
        else:
            try:
                winreg.DeleteValue(key, STARTUP_ENTRY)
            except FileNotFoundError:
                pass


# --- GUI --------------------------------------------------------------------

class App:
    REAPPLY_MS = 2000
    PRESETS = (3400, 4500, 5500, 6500, 8000)

    def __init__(self, root):
        self.root = root
        self.settings = load_settings()
        # The settings as last read from or written to disk; the base for merging in
        # changes other programs made to the file.
        self.base = copy.deepcopy(self.settings)
        self.displays = []
        self.current = None
        self.requested = NEUTRAL_K
        self.apply_job = None
        self.updating_scale = False

        root.title("Screen Color")
        root.resizable(False, False)
        frame = ttk.Frame(root, padding=16)
        frame.grid()

        ttk.Label(frame, text="Display").grid(row=0, column=0, columnspan=2, sticky="w")
        self.display_box = ttk.Combobox(frame, state="readonly", width=48)
        self.display_box.grid(row=1, column=0, columnspan=2, sticky="ew", pady=(2, 14))
        self.display_box.bind("<<ComboboxSelected>>", self.on_display_selected)

        readout = ttk.Frame(frame)
        readout.grid(row=2, column=0, columnspan=2, pady=(0, 6))
        self.swatch = tk.Canvas(readout, width=34, height=34, highlightthickness=1,
                                highlightbackground="#999999")
        self.swatch.grid(row=0, column=0, padx=(0, 10))
        self.kelvin_label = ttk.Label(readout, font=("Segoe UI", 22, "bold"), width=8)
        self.kelvin_label.grid(row=0, column=1)

        self.scale = ttk.Scale(frame, from_=MIN_K, to=MAX_K, orient="horizontal", length=380,
                               command=self.on_scale)
        self.scale.grid(row=3, column=0, columnspan=2, sticky="ew")
        ttk.Label(frame, text="Warmer").grid(row=4, column=0, sticky="w")
        ttk.Label(frame, text="Cooler").grid(row=4, column=1, sticky="e")

        presets = ttk.Frame(frame)
        presets.grid(row=5, column=0, columnspan=2, pady=(10, 4))
        for i, k in enumerate(self.PRESETS):
            text = f"{k} K" + (" (neutral)" if k == NEUTRAL_K else "")
            ttk.Button(presets, text=text, command=lambda k=k: self.choose(k)).grid(row=0, column=i, padx=2)

        self.status = ttk.Label(frame, foreground="#9a5b00", wraplength=380, justify="left")
        self.status.grid(row=6, column=0, columnspan=2, sticky="w", pady=(6, 0))

        self.startup = tk.BooleanVar(value=startup_enabled())
        ttk.Checkbutton(frame, text="Apply my colors when I sign in", variable=self.startup,
                        command=self.on_startup_toggled).grid(row=7, column=0, sticky="w", pady=(10, 0))
        ttk.Button(frame, text="Reset all displays", command=self.reset_all).grid(
            row=7, column=1, sticky="e", pady=(10, 0))

        ttk.Label(frame, foreground="#777777", wraplength=380, justify="left",
                  text="Keep this window open (minimized is fine) so the color is re-applied "
                       "after sleep, display changes, or apps that reset it."
                  ).grid(row=8, column=0, columnspan=2, sticky="w", pady=(12, 0))

        self.refresh_displays()
        root.protocol("WM_DELETE_WINDOW", self.on_close)
        root.after(self.REAPPLY_MS, self.tick)

    def saved_temperature(self, display):
        return self.settings["temperatures"].get(display.device_path, NEUTRAL_K)

    def refresh_displays(self):
        displays = list_displays()
        if [(d.device_path, d.gdi_name, d.label) for d in displays] == \
                [(d.device_path, d.gdi_name, d.label) for d in self.displays]:
            return
        self.displays = displays
        self.display_box["values"] = [d.label for d in displays]
        if not displays:
            self.current = None
            self.display_box.set("")
            self.status["text"] = "No active displays found."
            return
        # Keep the current selection if it's still connected, else fall back to the saved/default one.
        wanted = self.current.device_path if self.current else self.settings.get("selected")
        match = next((d for d in displays if d.device_path == wanted), default_displays(displays)[0])
        self.select(match)

    def select(self, display):
        self.current = display
        self.settings["selected"] = display.device_path
        self.display_box.current(self.displays.index(display))
        self.show_saved(self.saved_temperature(display))

    def show_saved(self, kelvin):
        """Put the slider and readout on `kelvin`, the current display's saved temperature."""
        self.requested = kelvin
        self.set_scale(kelvin)
        self.show(kelvin)

    def on_startup_toggled(self):
        try:
            set_startup(self.startup.get())
        except OSError as e:
            self.status["text"] = f"Couldn't change the sign-in setting: {e}"
        self.show_startup()

    def show_startup(self):
        """Tick the sign-in box if Windows will run `restore` at sign-in, which can also be
        changed in Task Manager's Startup apps page."""
        enabled = startup_enabled()
        if self.startup.get() != enabled:
            self.startup.set(enabled)

    def on_display_selected(self, _event):
        self.select(self.displays[self.display_box.current()])

    def set_scale(self, kelvin):
        self.updating_scale = True
        try:
            self.scale.set(kelvin)
        finally:
            self.updating_scale = False

    def on_scale(self, value):
        if not self.updating_scale:
            self.request(round(float(value) / 50) * 50)

    def choose(self, kelvin):
        self.set_scale(kelvin)
        self.request(kelvin)

    def request(self, kelvin):
        """Apply soon; coalesces the burst of events a slider drag produces."""
        if self.current is None:
            return
        self.requested = kelvin
        self.show(kelvin)
        if self.apply_job is None:
            self.apply_job = self.root.after(15, self.apply_requested)

    def apply_requested(self):
        self.apply_job = None
        try:
            applied = apply_temperature(self.current.gdi_name, self.requested)
        except OSError as e:
            self.status["text"] = str(e)
            return
        remember(self.settings, self.current, applied)
        self.show(applied)

    def show(self, applied):
        self.kelvin_label["text"] = f"{applied} K"
        self.swatch.configure(background=swatch_color(applied))
        if applied == self.requested:
            self.status["text"] = ""
        else:
            limit = "warmest" if self.requested < applied else "coolest"
            self.status["text"] = (f"Asked for {self.requested} K, but Windows limits how far the gamma "
                                   f"ramp can shift, so {applied} K is the {limit} it allows.")

    def reset_all(self):
        for d in self.displays:
            try:
                apply_temperature(d.gdi_name, NEUTRAL_K)
            except OSError:
                pass
            remember(self.settings, d, NEUTRAL_K)
        self.show_saved(NEUTRAL_K)
        self.sync_settings()

    def tick(self):
        """Pick up changes made from the command line, then re-apply saved temperatures
        that Windows or another app has reset."""
        try:
            self.refresh_displays()
            self.show_startup()
            changed = self.sync_settings()
            if changed is not None and self.apply_job is None:
                self.show_saved(changed)
            for d in self.displays:
                k = self.settings["temperatures"].get(d.device_path)
                if k and not ramp_matches(d.gdi_name, k):
                    apply_temperature(d.gdi_name, k)
        except OSError:
            pass  # e.g. a display disappeared mid-check; the next tick will catch up
        self.root.after(self.REAPPLY_MS, self.tick)

    def sync_settings(self):
        """Write the window's changes to the settings file and pick up changes other programs
        (like `screen_color.py set`) made there. Returns the current display's temperature if
        another program changed it."""
        on_disk = load_settings(strict=True)
        if on_disk is None:
            return None
        merged = merge_settings(self.base, self.settings, on_disk)
        if merged != on_disk:
            try:
                save_settings(merged)
            except OSError:
                return None  # try again on the next tick
        before = self.saved_temperature(self.current) if self.current else None
        self.settings, self.base = merged, copy.deepcopy(merged)
        after = self.saved_temperature(self.current) if self.current else None
        return after if after != before else None

    def on_close(self):
        try:
            self.sync_settings()
        finally:
            self.root.destroy()


def run_gui():
    try:
        ctypes.windll.shcore.SetProcessDpiAwareness(1)  # crisp text on scaled displays
    except (AttributeError, OSError):
        pass
    root = tk.Tk()
    App(root)
    root.mainloop()


# --- CLI --------------------------------------------------------------------

def pick_displays(displays, selector):
    if selector is None:
        return default_displays(displays)
    if selector.lower() == "all":
        return displays
    if selector.isdigit():
        chosen = [d for d in displays if d.index == int(selector)]
    else:
        s = selector.lower()
        chosen = [d for d in displays if s in d.name.lower() or s == d.gdi_name.lower()]
    if not chosen:
        sys.exit(f"No display matches {selector!r}. Run `list` to see them.")
    return chosen


def describe_current(display, settings):
    if ramp_matches(display.gdi_name, NEUTRAL_K):
        return "6500 K (neutral)"
    saved = settings["temperatures"].get(display.device_path)
    if saved and ramp_matches(display.gdi_name, saved):
        return f"{saved} K"
    return "changed by another app"


def apply_and_report(display, kelvin, settings):
    try:
        applied = apply_temperature(display.gdi_name, kelvin)
    except OSError as e:
        print(f"{display.index}: {display.name}: {e}")
        return
    remember(settings, display, applied)
    note = "" if applied == kelvin else f"  (asked for {kelvin} K; that's the limit Windows allows)"
    print(f"{display.index}: {display.name}: {applied} K{note}")


def kelvin_arg(text):
    k = int(text.lower().removesuffix("k"))
    if not MIN_K <= k <= MAX_K:
        raise argparse.ArgumentTypeError(f"must be between {MIN_K} and {MAX_K}")
    return k


def main(argv=None):
    parser = argparse.ArgumentParser(prog="screen_color",
                                     description="Adjust a monitor's color temperature on Windows.")
    sub = parser.add_subparsers(dest="command")
    sub.add_parser("gui", help="open the window (the default)")
    sub.add_parser("list", help="list displays and their current temperature")
    p = sub.add_parser("set", help="set the color temperature")
    p.add_argument("kelvin", type=kelvin_arg,
                   help=f"{MIN_K}-{MAX_K}; {NEUTRAL_K} is neutral, lower is warmer")
    p.add_argument("-d", "--display",
                   help="number from `list`, part of the monitor name, or 'all' (default: external displays)")
    p = sub.add_parser("reset", help=f"return displays to neutral ({NEUTRAL_K} K)")
    p.add_argument("-d", "--display", default="all",
                   help="number from `list`, part of the monitor name, or 'all' (the default)")
    sub.add_parser("restore", help="re-apply the saved temperatures, e.g. from a login shortcut")
    args = parser.parse_args(argv)

    if args.command in (None, "gui"):
        run_gui()
        return

    settings = load_settings()
    displays = list_displays()
    if not displays:
        sys.exit("No active displays found.")

    if args.command == "list":
        print(f"{'#':>2}  {'Device':<13} {'Monitor':<18} {'Connection':<21} {'Type':<18} Now")
        for d in displays:
            print(f"{d.index:>2}  {d.gdi_name:<13} {d.name:<18} {d.connection:<21} {d.kind:<18} "
                  f"{describe_current(d, settings)}")
        return

    if args.command == "set":
        for d in pick_displays(displays, args.display):
            apply_and_report(d, args.kelvin, settings)
    elif args.command == "reset":
        for d in pick_displays(displays, args.display):
            apply_and_report(d, NEUTRAL_K, settings)
    elif args.command == "restore":
        saved = settings["temperatures"]
        targets = [d for d in displays if d.device_path in saved]
        if not targets:
            print("Nothing saved for the connected displays.")
        for d in targets:
            apply_and_report(d, saved[d.device_path], settings)
    save_settings(settings)


if __name__ == "__main__":
    main()
