//! Color temperature math and the GPU gamma ramp.

use std::ffi::c_void;
use std::ptr::null;

use windows_sys::Win32::Graphics::Gdi::{CreateDCW, DeleteDC, HDC};
use windows_sys::Win32::UI::ColorSystem::{GetDeviceGammaRamp, SetDeviceGammaRamp};

use crate::wide;

pub const NEUTRAL_K: u32 = 6500;
pub const MIN_K: u32 = 1900;
pub const MAX_K: u32 = 10000;

/// 256 levels for each of red, green and blue, as SetDeviceGammaRamp expects.
pub type Ramp = [[u16; 256]; 3];

/// CIE 1931 xy chromaticity of a black body (Kim et al. 2002 cubic spline, 1667-25000 K).
fn planckian_xy(kelvin: f64) -> (f64, f64) {
    let t = kelvin.clamp(1667.0, 25000.0);
    let x = if t <= 4000.0 {
        -0.2661239e9 / t.powi(3) - 0.2343589e6 / t.powi(2) + 0.8776956e3 / t + 0.179910
    } else {
        -3.0258469e9 / t.powi(3) + 2.1070379e6 / t.powi(2) + 0.2226347e3 / t + 0.240390
    };
    let y = if t <= 2222.0 {
        -1.1063814 * x.powi(3) - 1.34811020 * x.powi(2) + 2.18555832 * x - 0.20219683
    } else if t <= 4000.0 {
        -0.9549476 * x.powi(3) - 1.37418593 * x.powi(2) + 2.09137015 * x - 0.16748867
    } else {
        3.0817580 * x.powi(3) - 5.87338670 * x.powi(2) + 3.75112997 * x - 0.37001483
    };
    (x, y)
}

fn linear_srgb(kelvin: f64) -> [f64; 3] {
    let (x, y) = planckian_xy(kelvin);
    let (big_x, big_y, big_z) = (x / y, 1.0, (1.0 - x - y) / y);
    [
        3.2404542 * big_x - 1.5371385 * big_y - 0.4985314 * big_z,
        -0.9692660 * big_x + 1.8760108 * big_y + 0.0415560 * big_z,
        0.0556434 * big_x - 0.2040259 * big_y + 1.0572252 * big_z,
    ]
}

/// Red/green/blue multipliers for the gamma ramp; exactly (1, 1, 1) at 6500 K.
pub fn channel_gains(kelvin: u32) -> [f64; 3] {
    let target = linear_srgb(kelvin as f64);
    let neutral = linear_srgb(NEUTRAL_K as f64);
    let rgb: [f64; 3] = std::array::from_fn(|i| (target[i] / neutral[i]).max(0.0));
    let peak = rgb.iter().copied().fold(0.0, f64::max);
    // The white point is scaled in linear light, but the ramp works on gamma-encoded values.
    rgb.map(|c| (c / peak).powf(1.0 / 2.2))
}

pub fn build_ramp(kelvin: u32) -> Ramp {
    let mut ramp = [[0; 256]; 3];
    for (levels, gain) in ramp.iter_mut().zip(channel_gains(kelvin)) {
        for (i, level) in levels.iter_mut().enumerate() {
            *level = (i as f64 * 257.0 * gain).round_ties_even().min(65535.0) as u16;
        }
    }
    ramp
}

/// The white point as 8-bit RGB, for the swatch in the window.
pub fn white_point_rgb(kelvin: u32) -> [u8; 3] {
    channel_gains(kelvin).map(|c| (c * 255.0).round() as u8)
}

/// A device context for one display, deleted on drop.
struct DisplayDc(HDC);

impl DisplayDc {
    fn open(gdi_name: &str) -> Result<Self, String> {
        let name = wide(gdi_name);
        let hdc = unsafe { CreateDCW(null(), name.as_ptr(), null(), null()) };
        if hdc.is_null() {
            Err(format!("Cannot open {gdi_name}"))
        } else {
            Ok(Self(hdc))
        }
    }

    fn set_ramp(&self, ramp: &Ramp) -> bool {
        unsafe { SetDeviceGammaRamp(self.0, ramp.as_ptr() as *const c_void) != 0 }
    }

    fn ramp(&self) -> Option<Ramp> {
        let mut ramp = [[0; 256]; 3];
        let ok = unsafe { GetDeviceGammaRamp(self.0, ramp.as_mut_ptr() as *mut c_void) != 0 };
        ok.then_some(ramp)
    }
}

impl Drop for DisplayDc {
    fn drop(&mut self) {
        unsafe { DeleteDC(self.0) };
    }
}

/// Sets a display's color temperature and returns the temperature actually applied.
///
/// Windows rejects gamma ramps that stray too far from neutral. If the requested value
/// is refused, the warmest (or coolest) value Windows still accepts is used instead.
pub fn apply_temperature(gdi_name: &str, kelvin: u32) -> Result<u32, String> {
    let dc = DisplayDc::open(gdi_name)?;
    let accepted = |k: u32| dc.set_ramp(&build_ramp(k));
    if accepted(kelvin) {
        return Ok(kelvin);
    }
    if !accepted(NEUTRAL_K) {
        return Err(format!(
            "Windows refused to change the gamma ramp of {gdi_name}. \
             HDR may be on for this display, or its driver doesn't allow it."
        ));
    }
    let (mut good, mut bad) = (NEUTRAL_K as i64, kelvin as i64);
    while (good - bad).abs() > 10 {
        let mid = (good + bad) / 2;
        if accepted(mid as u32) {
            good = mid;
        } else {
            bad = mid;
        }
    }
    // A rejected call leaves the ramp alone, so the last accepted value (good) is in effect.
    Ok(good as u32)
}

pub fn ramp_matches(gdi_name: &str, kelvin: u32) -> bool {
    DisplayDc::open(gdi_name)
        .ok()
        .and_then(|dc| dc.ramp())
        .is_some_and(|ramp| ramp == build_ramp(kelvin))
}
