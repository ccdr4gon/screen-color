//! Screen Color: adjust a monitor's color temperature on Windows.
//!
//! Works through the GPU's gamma ramp (the same mechanism f.lux uses), so it needs no
//! DDC/CI support from the monitor and only affects the display you pick.

mod color;
mod display;
mod gui;
mod settings;
mod startup;

use std::process::ExitCode;

use color::{MAX_K, MIN_K, NEUTRAL_K};
use display::Display;
use settings::Settings;

const USAGE: &str = "\
Adjust a monitor's color temperature.

Usage:
  screen-color                        open the window
  screen-color list                   list displays and their current temperature
  screen-color set <kelvin> [-d <display>]
                                      set the temperature: 1900-10000, where 6500 is
                                      neutral and lower is warmer (default: external displays)
  screen-color reset [-d <display>]   back to neutral 6500 K (default: all displays)
  screen-color restore                re-apply the saved temperatures, e.g. at login

<display> is a number from `list`, part of the monitor name, or \"all\".
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let Some((command, rest)) = args.split_first() else {
        gui::run();
        return Ok(());
    };
    let mut operands = Vec::new();
    let mut selector = None;
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        if arg == "-d" || arg == "--display" {
            let value = rest.next().ok_or(
                "-d needs a display: a number from `list`, part of the monitor name, or \"all\".",
            )?;
            selector = Some(value.as_str());
        } else if let Some(value) = arg.strip_prefix("--display=") {
            selector = Some(value);
        } else {
            operands.push(arg.as_str());
        }
    }

    match (command.as_str(), operands.as_slice(), selector) {
        ("gui", [], None) => {
            gui::run();
            Ok(())
        }
        ("list", [], None) => list(),
        ("set", [kelvin], _) => set(parse_kelvin(kelvin)?, selector),
        ("reset", [], _) => set(NEUTRAL_K, Some(selector.unwrap_or("all"))),
        ("restore", [], None) => restore(),
        ("help" | "-h" | "--help" | "/?", _, _) => {
            print!("{USAGE}");
            Ok(())
        }
        _ => Err(format!("Unrecognized command line.\n\n{USAGE}")),
    }
}

fn parse_kelvin(text: &str) -> Result<u32, String> {
    match text.strip_suffix(['k', 'K']).unwrap_or(text).parse::<u32>() {
        Ok(kelvin) if (MIN_K..=MAX_K).contains(&kelvin) => Ok(kelvin),
        _ => Err(format!("The temperature must be a number from {MIN_K} to {MAX_K}, e.g. 4500.")),
    }
}

fn active_displays() -> Result<Vec<Display>, String> {
    let displays = display::list_displays()?;
    if displays.is_empty() {
        return Err("No active displays found.".into());
    }
    Ok(displays)
}

fn pick_displays<'a>(displays: &'a [Display], selector: Option<&str>) -> Result<Vec<&'a Display>, String> {
    let Some(selector) = selector else {
        return Ok(display::default_displays(displays));
    };
    if selector.eq_ignore_ascii_case("all") {
        return Ok(displays.iter().collect());
    }
    let chosen: Vec<&Display> = match selector.parse::<usize>() {
        Ok(number) => displays.iter().filter(|d| d.index == number).collect(),
        Err(_) => {
            let wanted = selector.to_lowercase();
            displays
                .iter()
                .filter(|d| d.name.to_lowercase().contains(&wanted) || d.gdi_name.to_lowercase() == wanted)
                .collect()
        }
    };
    if chosen.is_empty() {
        return Err(format!("No display matches \"{selector}\". Run `screen-color list` to see them."));
    }
    Ok(chosen)
}

fn describe_current(display: &Display, settings: &Settings) -> String {
    if color::ramp_matches(&display.gdi_name, NEUTRAL_K) {
        return "6500 K (neutral)".into();
    }
    match settings.temperature(display) {
        Some(kelvin) if color::ramp_matches(&display.gdi_name, kelvin) => format!("{kelvin} K"),
        _ => "changed by another app".into(),
    }
}

fn apply_and_report(display: &Display, kelvin: u32, settings: &mut Settings) {
    match color::apply_temperature(&display.gdi_name, kelvin) {
        Ok(applied) => {
            settings.remember(display, applied);
            let note = if applied == kelvin {
                String::new()
            } else {
                format!("  (asked for {kelvin} K; that's the limit Windows allows)")
            };
            println!("{}: {}: {applied} K{note}", display.index, display.name);
        }
        Err(message) => println!("{}: {}: {message}", display.index, display.name),
    }
}

fn save(settings: &Settings) -> Result<(), String> {
    settings.save().map_err(|e| format!("Couldn't save the settings: {e}"))
}

fn list() -> Result<(), String> {
    let settings = Settings::load();
    let displays = active_displays()?;
    println!("{:>2}  {:<13} {:<18} {:<21} {:<18} Now", "#", "Device", "Monitor", "Connection", "Type");
    for d in &displays {
        println!(
            "{:>2}  {:<13} {:<18} {:<21} {:<18} {}",
            d.index,
            d.gdi_name,
            d.name,
            d.connection,
            d.kind(),
            describe_current(d, &settings)
        );
    }
    Ok(())
}

fn set(kelvin: u32, selector: Option<&str>) -> Result<(), String> {
    let mut settings = Settings::load();
    let displays = active_displays()?;
    for display in pick_displays(&displays, selector)? {
        apply_and_report(display, kelvin, &mut settings);
    }
    save(&settings)
}

fn restore() -> Result<(), String> {
    let mut settings = Settings::load();
    let displays = active_displays()?;
    let saved: Vec<(&Display, u32)> =
        displays.iter().filter_map(|d| Some((d, settings.temperature(d)?))).collect();
    if saved.is_empty() {
        println!("Nothing saved for the connected displays.");
    }
    for (display, kelvin) in saved {
        apply_and_report(display, kelvin, &mut settings);
    }
    save(&settings)
}

/// UTF-16 with a terminating NUL, for Win32 calls.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A Win32 UTF-16 buffer, up to its first NUL.
fn from_wide(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}
