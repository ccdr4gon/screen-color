//! The window, built from standard Win32 controls so it looks native and needs no GUI framework.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList, GetConsoleWindow};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::{SS_NOPREFIX, SS_RIGHT};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::color::{self, MAX_K, MIN_K, NEUTRAL_K};
use crate::display::{self, Display};
use crate::settings::Settings;
use crate::{startup, wide};

const PRESETS: [u32; 5] = [3400, 4500, 5500, 6500, 8000];
/// Temperature change per slider step.
const STEP_K: u32 = 50;
/// Missing from windows-sys; commctrl.h defines it as WM_USER.
const TBM_GETPOS: u32 = WM_USER;

const ID_DISPLAY: u16 = 100;
const ID_SLIDER: u16 = 101;
const ID_PRESET: u16 = 110; // one per preset
const ID_RESET_ALL: u16 = 120;
const ID_STARTUP: u16 = 121;

const TIMER_REAPPLY: usize = 1;
const TIMER_APPLY: usize = 2;
/// How often to check whether Windows or another app has reset the gamma ramp.
const REAPPLY_MS: u32 = 2000;
/// Slider events this close together are coalesced into one gamma ramp change.
const APPLY_DELAY_MS: u32 = 15;

const MAIN_STYLE: WINDOW_STYLE = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
// Layout in 96-DPI pixels; everything is scaled by the window's DPI.
const CLIENT_W: i32 = 440;
const CLIENT_H: i32 = 356;
const READOUT_TOP: i32 = 74;
const READOUT_BOTTOM: i32 = 120;

const STATUS_COLOR: COLORREF = rgb(0x9a, 0x5b, 0x00);
const HINT_COLOR: COLORREF = rgb(0x6b, 0x6b, 0x6b);
const SWATCH_BORDER: COLORREF = rgb(0x99, 0x99, 0x99);

const HINT: &str = "Keep this window open (minimized is fine) so the color is re-applied \
                    after sleep, display changes, or apps that reset it.";

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}

fn points_to_pixels(points: i32, dpi: u32) -> i32 {
    (points * dpi as i32 + 36) / 72
}

fn loword(value: usize) -> u16 {
    (value & 0xFFFF) as u16
}

fn hiword(value: usize) -> u16 {
    ((value >> 16) & 0xFFFF) as u16
}

struct Controls {
    display_label: HWND,
    display_combo: HWND,
    slider: HWND,
    warmer: HWND,
    cooler: HWND,
    presets: Vec<HWND>,
    status: HWND,
    startup: HWND,
    reset_all: HWND,
    hint: HWND,
}

impl Controls {
    /// Creates the controls in tab order.
    fn create(parent: HWND) -> Self {
        let instance = unsafe { GetModuleHandleW(null()) };
        let make = |class: &str, text: &str, style: u32, id: u16| {
            let (class, text) = (wide(class), wide(text));
            unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    text.as_ptr(),
                    WS_CHILD | WS_VISIBLE | style,
                    0,
                    0,
                    0,
                    0,
                    parent,
                    id as usize as HMENU,
                    instance,
                    null(),
                )
            }
        };
        let label = SS_NOPREFIX;
        let button = BS_PUSHBUTTON as u32 | WS_TABSTOP;

        let display_label = make("STATIC", "Display", label, 0);
        let display_combo = make("COMBOBOX", "", CBS_DROPDOWNLIST as u32 | WS_VSCROLL | WS_TABSTOP, ID_DISPLAY);
        let slider = make("msctls_trackbar32", "", TBS_HORZ | TBS_NOTICKS | WS_TABSTOP, ID_SLIDER);
        let warmer = make("STATIC", "Warmer", label, 0);
        let cooler = make("STATIC", "Cooler", label | SS_RIGHT, 0);
        let presets = PRESETS
            .iter()
            .enumerate()
            .map(|(i, &k)| {
                let text = if k == NEUTRAL_K { format!("{k} K (neutral)") } else { format!("{k} K") };
                make("BUTTON", &text, button, ID_PRESET + i as u16)
            })
            .collect();
        let status = make("STATIC", "", label, 0);
        let checkbox = BS_AUTOCHECKBOX as u32 | WS_TABSTOP;
        let startup = make("BUTTON", "Apply my colors when I sign in", checkbox, ID_STARTUP);
        let reset_all = make("BUTTON", "Reset all displays", button, ID_RESET_ALL);
        let hint = make("STATIC", HINT, label, 0);

        unsafe {
            SendMessageW(slider, TBM_SETRANGEMIN, FALSE as WPARAM, (MIN_K / STEP_K) as LPARAM);
            SendMessageW(slider, TBM_SETRANGEMAX, TRUE as WPARAM, (MAX_K / STEP_K) as LPARAM);
            SendMessageW(slider, TBM_SETLINESIZE, 0, (100 / STEP_K) as LPARAM);
            SendMessageW(slider, TBM_SETPAGESIZE, 0, (500 / STEP_K) as LPARAM);
        }
        Self { display_label, display_combo, slider, warmer, cooler, presets, status, startup, reset_all, hint }
    }

    /// Each control's position and size, in 96-DPI pixels.
    fn layout(&self) -> Vec<(HWND, [i32; 4])> {
        let mut layout = vec![
            (self.display_label, [16, 14, 408, 20]),
            (self.display_combo, [16, 36, 408, 240]),
            (self.slider, [10, 124, 420, 32]),
            (self.warmer, [16, 158, 120, 20]),
            (self.cooler, [304, 158, 120, 20]),
        ];
        let mut x = 16;
        for (&button, &k) in self.presets.iter().zip(&PRESETS) {
            let width = if k == NEUTRAL_K { 108 } else { 69 };
            layout.push((button, [x, 188, width, 28]));
            x += width + 6;
        }
        layout.extend([
            (self.status, [16, 228, 408, 36]),
            (self.startup, [16, 272, 260, 20]),
            (self.reset_all, [284, 268, 140, 28]),
            (self.hint, [16, 310, 408, 36]),
        ]);
        layout
    }
}

struct State {
    settings: Settings,
    /// The settings as last read from or written to disk; the base for merging in
    /// changes other programs made to the file.
    saved: Settings,
    displays: Vec<Display>,
    current: Option<usize>,
    /// The temperature asked for on the current display.
    requested: u32,
    /// The temperature in the readout.
    shown: u32,
    apply_pending: bool,
}

struct Window {
    hwnd: HWND,
    controls: Controls,
    background: HBRUSH,
    font: Cell<HFONT>,
    big_font: Cell<HFONT>,
    dpi: Cell<u32>,
    last_focus: Cell<HWND>,
    status_text: RefCell<String>,
    state: RefCell<State>,
}

impl Window {
    fn new(hwnd: HWND) -> Self {
        let settings = Settings::load();
        Self {
            hwnd,
            controls: Controls::create(hwnd),
            background: unsafe { GetSysColorBrush(COLOR_WINDOW) },
            font: Cell::new(null_mut()),
            big_font: Cell::new(null_mut()),
            dpi: Cell::new(96),
            last_focus: Cell::new(null_mut()),
            status_text: RefCell::new(String::new()),
            state: RefCell::new(State {
                saved: settings.clone(),
                settings,
                displays: Vec::new(),
                current: None,
                requested: NEUTRAL_K,
                shown: NEUTRAL_K,
                apply_pending: false,
            }),
        }
    }

    fn handle(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_COMMAND => self.on_command(loword(wparam), hiword(wparam)),
            WM_HSCROLL if lparam as HWND == self.controls.slider => self.on_slider(),
            WM_TIMER if wparam == TIMER_APPLY => self.apply_requested(),
            WM_TIMER if wparam == TIMER_REAPPLY => self.tick(),
            WM_CTLCOLORSTATIC => return Some(self.static_colors(wparam as HDC, lparam as HWND)),
            WM_PAINT => self.paint(),
            WM_ACTIVATE => {
                // Remember the focused control when deactivated (WA_INACTIVE) to restore it later.
                if loword(wparam) == 0 {
                    self.last_focus.set(unsafe { GetFocus() });
                }
                return None;
            }
            WM_SETFOCUS => {
                let target = self.last_focus.get();
                unsafe { SetFocus(if target.is_null() { self.controls.slider } else { target }) };
            }
            WM_DPICHANGED => {
                let suggested = unsafe { &*(lparam as *const RECT) };
                self.apply_dpi(hiword(wparam) as u32, Some((suggested.left, suggested.top)));
            }
            WM_DESTROY => self.on_destroy(),
            _ => return None,
        }
        Some(0)
    }

    fn on_command(&self, id: u16, code: u16) {
        let code = code as u32;
        match id {
            ID_DISPLAY if code == CBN_SELCHANGE => {
                let i = unsafe { SendMessageW(self.controls.display_combo, CB_GETCURSEL, 0, 0) };
                if i >= 0 {
                    self.select(i as usize);
                }
            }
            ID_RESET_ALL if code == BN_CLICKED => self.reset_all(),
            ID_STARTUP if code == BN_CLICKED => self.on_startup_clicked(),
            _ if code == BN_CLICKED && (ID_PRESET..ID_PRESET + PRESETS.len() as u16).contains(&id) => {
                self.choose(PRESETS[(id - ID_PRESET) as usize]);
            }
            _ => {}
        }
    }

    fn on_startup_clicked(&self) {
        let checked = unsafe { SendMessageW(self.controls.startup, BM_GETCHECK, 0, 0) } == BST_CHECKED as LRESULT;
        if let Err(message) = startup::set_enabled(checked) {
            self.set_status(&message);
        }
        self.show_startup();
    }

    /// Ticks the sign-in box if Windows will run `restore` at sign-in, which can also be
    /// changed in Task Manager's Startup apps page.
    fn show_startup(&self) {
        let state = if startup::is_enabled() { BST_CHECKED } else { BST_UNCHECKED };
        unsafe { SendMessageW(self.controls.startup, BM_SETCHECK, state as WPARAM, 0) };
    }

    fn on_slider(&self) {
        let position = unsafe { SendMessageW(self.controls.slider, TBM_GETPOS, 0, 0) };
        self.request(position as u32 * STEP_K);
    }

    fn set_slider(&self, kelvin: u32) {
        unsafe { SendMessageW(self.controls.slider, TBM_SETPOS, TRUE as WPARAM, (kelvin / STEP_K) as LPARAM) };
    }

    fn choose(&self, kelvin: u32) {
        self.set_slider(kelvin);
        self.request(kelvin);
    }

    /// Applies soon; coalesces the burst of events a slider drag produces.
    fn request(&self, kelvin: u32) {
        let start_timer = {
            let mut state = self.state.borrow_mut();
            if state.current.is_none() {
                return;
            }
            state.requested = kelvin;
            !std::mem::replace(&mut state.apply_pending, true)
        };
        self.show(kelvin);
        if start_timer {
            unsafe { SetTimer(self.hwnd, TIMER_APPLY, APPLY_DELAY_MS, None) };
        }
    }

    fn apply_requested(&self) {
        unsafe { KillTimer(self.hwnd, TIMER_APPLY) };
        let (display, requested) = {
            let mut state = self.state.borrow_mut();
            state.apply_pending = false;
            let Some(i) = state.current else { return };
            (state.displays[i].clone(), state.requested)
        };
        match color::apply_temperature(&display.gdi_name, requested) {
            Ok(applied) => {
                self.state.borrow_mut().settings.remember(&display, applied);
                self.show(applied);
            }
            Err(message) => self.set_status(&message),
        }
    }

    /// Shows `kelvin` in the readout, explaining when Windows didn't allow the requested value.
    fn show(&self, kelvin: u32) {
        let requested = {
            let mut state = self.state.borrow_mut();
            state.shown = kelvin;
            state.requested
        };
        unsafe { InvalidateRect(self.hwnd, &self.readout_rect(), FALSE) };
        if kelvin == requested {
            self.set_status("");
        } else {
            let limit = if requested < kelvin { "warmest" } else { "coolest" };
            self.set_status(&format!(
                "Asked for {requested} K, but Windows limits how far the gamma ramp can shift, \
                 so {kelvin} K is the {limit} it allows."
            ));
        }
    }

    fn set_status(&self, text: &str) {
        if *self.status_text.borrow() == text {
            return;
        }
        *self.status_text.borrow_mut() = text.to_owned();
        let text = wide(text);
        unsafe { SetWindowTextW(self.controls.status, text.as_ptr()) };
    }

    fn refresh_displays(&self) {
        let Ok(displays) = display::list_displays() else { return };
        let wanted = {
            let state = self.state.borrow();
            if state.displays == displays {
                return;
            }
            // Keep the current selection if it's still connected, else the saved or default one.
            let current = state.current.map(|i| state.displays[i].device_path.clone());
            current.or_else(|| state.settings.selected.clone())
        };
        let choice = displays
            .iter()
            .position(|d| Some(&d.device_path) == wanted.as_ref())
            .or_else(|| display::default_displays(&displays).first().map(|d| d.index - 1));
        let labels: Vec<String> = displays.iter().map(Display::label).collect();
        {
            let mut state = self.state.borrow_mut();
            state.displays = displays;
            state.current = None;
        }
        unsafe { SendMessageW(self.controls.display_combo, CB_RESETCONTENT, 0, 0) };
        for label in labels {
            let label = wide(&label);
            unsafe { SendMessageW(self.controls.display_combo, CB_ADDSTRING, 0, label.as_ptr() as LPARAM) };
        }
        match choice {
            Some(i) => self.select(i),
            None => self.set_status("No active displays found."),
        }
    }

    fn select(&self, i: usize) {
        let kelvin = {
            let mut guard = self.state.borrow_mut();
            let state = &mut *guard;
            let display = &state.displays[i];
            let kelvin = state.settings.temperature(display).unwrap_or(NEUTRAL_K);
            state.settings.selected = Some(display.device_path.clone());
            state.current = Some(i);
            kelvin
        };
        unsafe { SendMessageW(self.controls.display_combo, CB_SETCURSEL, i, 0) };
        self.show_saved(kelvin);
    }

    /// Puts the slider and readout on `kelvin`, the current display's saved temperature.
    fn show_saved(&self, kelvin: u32) {
        self.state.borrow_mut().requested = kelvin;
        self.set_slider(kelvin);
        self.show(kelvin);
    }

    fn reset_all(&self) {
        let displays = self.state.borrow().displays.clone();
        for display in &displays {
            let _ = color::apply_temperature(&display.gdi_name, NEUTRAL_K);
            self.state.borrow_mut().settings.remember(display, NEUTRAL_K);
        }
        self.show_saved(NEUTRAL_K);
        self.sync_settings();
    }

    /// Picks up changes made from the command line, then re-applies saved temperatures
    /// that Windows or another app has reset.
    fn tick(&self) {
        self.refresh_displays();
        self.show_startup();
        let busy = self.state.borrow().apply_pending;
        if let Some(kelvin) = self.sync_settings()
            && !busy
        {
            self.show_saved(kelvin);
        }
        let (displays, settings) = {
            let state = self.state.borrow();
            (state.displays.clone(), state.settings.clone())
        };
        for display in &displays {
            if let Some(kelvin) = settings.temperature(display)
                && !color::ramp_matches(&display.gdi_name, kelvin)
            {
                let _ = color::apply_temperature(&display.gdi_name, kelvin);
            }
        }
    }

    /// Writes the window's changes to the settings file and picks up changes other programs
    /// (like `screen-color set`) made there. Returns the current display's temperature if
    /// another program changed it.
    fn sync_settings(&self) -> Option<u32> {
        let mut guard = self.state.borrow_mut();
        let state = &mut *guard;
        let on_disk = Settings::read()?;
        let merged = Settings::merge(&state.saved, &state.settings, &on_disk);
        if merged != on_disk && merged.save().is_err() {
            return None; // try again on the next tick
        }
        let current = state.current.map(|i| &state.displays[i]);
        let kelvin = |settings: &Settings| current.map(|d| settings.temperature(d).unwrap_or(NEUTRAL_K));
        let (before, after) = (kelvin(&state.settings), kelvin(&merged));
        state.settings = merged.clone();
        state.saved = merged;
        if after != before { after } else { None }
    }

    fn on_destroy(&self) {
        unsafe {
            KillTimer(self.hwnd, TIMER_REAPPLY);
            KillTimer(self.hwnd, TIMER_APPLY);
        }
        self.sync_settings();
        unsafe { PostQuitMessage(0) };
    }

    fn static_colors(&self, hdc: HDC, control: HWND) -> LRESULT {
        let color = if control == self.controls.status {
            STATUS_COLOR
        } else if control == self.controls.hint {
            HINT_COLOR
        } else {
            unsafe { GetSysColor(COLOR_WINDOWTEXT) }
        };
        unsafe {
            SetTextColor(hdc, color);
            SetBkMode(hdc, TRANSPARENT as i32);
        }
        self.background as LRESULT
    }

    fn readout_rect(&self) -> RECT {
        let dpi = self.dpi.get();
        RECT {
            left: 0,
            top: scale(READOUT_TOP, dpi),
            right: scale(CLIENT_W, dpi),
            bottom: scale(READOUT_BOTTOM, dpi),
        }
    }

    /// Draws the color swatch and the temperature.
    fn paint(&self) {
        let kelvin = self.state.borrow().shown;
        let dpi = self.dpi.get();
        let area = self.readout_rect();
        unsafe {
            let mut paint: PAINTSTRUCT = zeroed();
            let hdc = BeginPaint(self.hwnd, &mut paint);
            FillRect(hdc, &area, self.background);
            let old_font = SelectObject(hdc, self.big_font.get());

            // Lay out for the widest value so the swatch doesn't move as the number changes.
            let mut widest = wide("10000 K");
            let mut text_size: RECT = zeroed();
            DrawTextW(hdc, widest.as_mut_ptr(), -1, &mut text_size, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
            let (swatch, gap) = (scale(34, dpi), scale(12, dpi));
            let left = (area.right - (swatch + gap + text_size.right)) / 2;
            let top = (area.top + area.bottom - swatch) / 2;
            let swatch_rect = RECT { left, top, right: left + swatch, bottom: top + swatch };

            let [r, g, b] = color::white_point_rgb(kelvin);
            let fill = CreateSolidBrush(rgb(r, g, b));
            FillRect(hdc, &swatch_rect, fill);
            DeleteObject(fill);
            let border = CreateSolidBrush(SWATCH_BORDER);
            FrameRect(hdc, &swatch_rect, border);
            DeleteObject(border);

            let mut text = wide(&format!("{kelvin} K"));
            let mut text_rect = RECT {
                left: swatch_rect.right + gap,
                top: area.top,
                right: swatch_rect.right + gap + text_size.right,
                bottom: area.bottom,
            };
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, GetSysColor(COLOR_WINDOWTEXT));
            DrawTextW(hdc, text.as_mut_ptr(), -1, &mut text_rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);

            SelectObject(hdc, old_font);
            EndPaint(self.hwnd, &paint);
        }
    }

    /// Sizes the window, fonts and controls for `dpi`, optionally moving the window too.
    fn apply_dpi(&self, dpi: u32, position: Option<(i32, i32)>) {
        self.dpi.set(dpi);
        let (font, big_font) = create_fonts(dpi);
        let old_fonts = [self.font.replace(font), self.big_font.replace(big_font)];
        for (control, [x, y, w, h]) in self.controls.layout() {
            unsafe {
                SendMessageW(control, WM_SETFONT, font as WPARAM, 0);
                MoveWindow(control, scale(x, dpi), scale(y, dpi), scale(w, dpi), scale(h, dpi), TRUE);
            }
        }
        for old in old_fonts {
            if !old.is_null() {
                unsafe { DeleteObject(old) };
            }
        }

        let mut frame = RECT { left: 0, top: 0, right: scale(CLIENT_W, dpi), bottom: scale(CLIENT_H, dpi) };
        unsafe { AdjustWindowRectExForDpi(&mut frame, MAIN_STYLE, FALSE, 0, dpi) };
        let (x, y, flags) = match position {
            Some((x, y)) => (x, y, SWP_NOZORDER | SWP_NOACTIVATE),
            None => (0, 0, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE),
        };
        unsafe {
            SetWindowPos(self.hwnd, null_mut(), x, y, frame.right - frame.left, frame.bottom - frame.top, flags);
            InvalidateRect(self.hwnd, null(), TRUE);
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        for font in [self.font.get(), self.big_font.get()] {
            if !font.is_null() {
                unsafe { DeleteObject(font) };
            }
        }
    }
}

/// The Windows UI font at `dpi`, and a large bold version of it for the temperature.
fn create_fonts(dpi: u32) -> (HFONT, HFONT) {
    let mut metrics: NONCLIENTMETRICSW = unsafe { zeroed() };
    metrics.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
    let ok = unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            &mut metrics as *mut NONCLIENTMETRICSW as *mut c_void,
            0,
            dpi,
        )
    } != 0;
    let mut font = metrics.lfMessageFont;
    if !ok {
        font = unsafe { zeroed() };
        font.lfHeight = -points_to_pixels(9, dpi);
        for (dst, src) in font.lfFaceName.iter_mut().zip("Segoe UI".encode_utf16()) {
            *dst = src;
        }
    }
    let regular = unsafe { CreateFontIndirectW(&font) };
    font.lfHeight = -points_to_pixels(22, dpi);
    font.lfWeight = 700; // bold
    let big = unsafe { CreateFontIndirectW(&font) };
    (regular, big)
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let window = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Window;
    if !window.is_null()
        && let Some(result) = unsafe { &*window }.handle(msg, wparam, lparam)
    {
        return result;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// On Windows versions without the manifest's console allocation policy, double-clicking
/// the exe opens a console window along with ours. Close it if nothing else is using it.
fn hide_own_console() {
    unsafe {
        if GetConsoleWindow().is_null() {
            return;
        }
        let mut processes = [0u32; 2];
        if GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) == 1 {
            FreeConsole();
        }
    }
}

fn error_box(text: &str) {
    let (text, title) = (wide(text), wide("Screen Color"));
    unsafe { MessageBoxW(null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR) };
}

pub fn run() {
    hide_own_console();
    let controls = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
    };
    unsafe { InitCommonControlsEx(&controls) };

    let instance = unsafe { GetModuleHandleW(null()) };
    let class_name = wide("ScreenColorWindow");
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: null_mut(),
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        hbrBackground: unsafe { GetSysColorBrush(COLOR_WINDOW) },
        lpszMenuName: null(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: null_mut(),
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        return error_box("Couldn't register the window class.");
    }

    let title = wide("Screen Color");
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            MAIN_STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    if hwnd.is_null() {
        return error_box("Couldn't create the window.");
    }

    let window = Box::into_raw(Box::new(Window::new(hwnd)));
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, window as isize) };
    let window_ref = unsafe { &*window };
    window_ref.apply_dpi(unsafe { GetDpiForWindow(hwnd) }, None);
    window_ref.refresh_displays();
    window_ref.show_startup();
    unsafe {
        SetTimer(hwnd, TIMER_REAPPLY, REAPPLY_MS, None);
        ShowWindow(hwnd, SW_SHOW);
        SetFocus(window_ref.controls.slider);
    }

    let mut msg: MSG = unsafe { zeroed() };
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        unsafe {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    // The window is destroyed by now, so nothing can reach the pointer anymore.
    drop(unsafe { Box::from_raw(window) });
}
