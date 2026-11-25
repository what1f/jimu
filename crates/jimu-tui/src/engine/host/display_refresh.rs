//! One-shot primary-display refresh probe (OnceLock-cached).
//! Fail-closed: it never panics into callers, does no TTY IO, and never mutates the display mode.

use std::sync::OnceLock;
use std::time::Instant;

use super::{DisplayServer, HostOs, is_wsl};

/// Sane bounds; a reading outside them fails closed.
const MIN_HZ: u32 = 30;
const MAX_HZ: u32 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display, strum::AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum DisplayRefreshSource {
    None,
    MacosCoreGraphics,
    WindowsEnumDisplaySettings,
    Linux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayRefreshProbeResult {
    pub hz: Option<u32>,
    pub source: DisplayRefreshSource,
    /// Empty when ok; else a stable skip/error token.
    pub skip_reason: &'static str,
    pub duration_ms: u64,
}

impl DisplayRefreshProbeResult {
    /// `ok` | `skipped` | `error`
    pub fn outcome(self) -> &'static str {
        if self.hz.is_some() {
            "ok"
        } else if self.skip_reason == "error" {
            "error"
        } else {
            "skipped"
        }
    }
}

/// Probes once per process; never panics.
pub fn probe_display_refresh() -> DisplayRefreshProbeResult {
    static CACHE: OnceLock<DisplayRefreshProbeResult> = OnceLock::new();
    *CACHE.get_or_init(probe_uncached)
}

fn probe_uncached() -> DisplayRefreshProbeResult {
    let start = Instant::now();
    let (hz, source, skip_reason) = probe_inner();
    DisplayRefreshProbeResult {
        hz,
        source,
        skip_reason,
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

fn probe_inner() -> (Option<u32>, DisplayRefreshSource, &'static str) {
    let is_ssh = crate::engine::clipboard::is_remote_session();
    let wsl = is_wsl();
    let os = HostOs::current();
    let display = DisplayServer::current();

    // Avoid FFI when env already forces a skip.
    let platform_hz = if precheck_skip(is_ssh, wsl).is_some() {
        None
    } else {
        match os {
            HostOs::Macos => Some(probe_macos()),
            HostOs::Windows => Some(probe_windows()),
            HostOs::Linux | HostOs::Other => None,
        }
    };

    decide(is_ssh, wsl, os, display, platform_hz)
}

/// Pure matrix used by production and tests; inject only the platform result.
fn decide(
    is_ssh: bool,
    is_wsl: bool,
    os: HostOs,
    display: DisplayServer,
    platform_hz: Option<Result<u32, &'static str>>,
) -> (Option<u32>, DisplayRefreshSource, &'static str) {
    if let Some(reason) = precheck_skip(is_ssh, is_wsl) {
        return (None, DisplayRefreshSource::None, reason);
    }
    match os {
        HostOs::Macos => {
            let source = DisplayRefreshSource::MacosCoreGraphics;
            match platform_hz.unwrap_or(Err("error")) {
                Ok(hz) => accept_hz(hz, source),
                Err(reason) => (None, source, reason),
            }
        }
        HostOs::Windows => {
            let source = DisplayRefreshSource::WindowsEnumDisplaySettings;
            match platform_hz.unwrap_or(Err("error")) {
                Ok(hz) => accept_hz(hz, source),
                Err(reason) => (None, source, reason),
            }
        }
        HostOs::Linux => {
            let reason = linux_skip_reason(display);
            (None, DisplayRefreshSource::Linux, reason)
        }
        HostOs::Other => (None, DisplayRefreshSource::None, "unsupported"),
    }
}

fn precheck_skip(is_ssh: bool, is_wsl: bool) -> Option<&'static str> {
    if is_ssh {
        return Some("ssh");
    }
    if is_wsl {
        return Some("wsl");
    }
    None
}

fn linux_skip_reason(display: DisplayServer) -> &'static str {
    match display {
        DisplayServer::Wayland => "wayland_unsupported",
        DisplayServer::X11 => "x11_unsupported",
        _ => "no_display",
    }
}

fn accept_hz(
    hz: u32,
    source: DisplayRefreshSource,
) -> (Option<u32>, DisplayRefreshSource, &'static str) {
    if !(MIN_HZ..=MAX_HZ).contains(&hz) {
        return (None, source, "out_of_range");
    }
    (Some(hz), source, "")
}

#[cfg(target_os = "macos")]
fn probe_macos() -> Result<u32, &'static str> {
    // Fail-closed if FFI panics (abort builds still abort).
    match std::panic::catch_unwind(|| {
        // SAFETY: read-only CoreGraphics display query; no mode mutation.
        unsafe { macos_main_display_refresh_hz() }
    }) {
        Ok(inner) => inner,
        Err(_) => Err("error"),
    }
}

#[cfg(not(target_os = "macos"))]
fn probe_macos() -> Result<u32, &'static str> {
    Err("unsupported")
}

#[cfg(target_os = "macos")]
unsafe fn macos_main_display_refresh_hz() -> Result<u32, &'static str> {
    type CgDisplayModeRef = *mut core::ffi::c_void;
    type CgDirectDisplayId = u32;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> CgDirectDisplayId;
        fn CGDisplayCopyDisplayMode(display: CgDirectDisplayId) -> CgDisplayModeRef;
        fn CGDisplayModeGetRefreshRate(mode: CgDisplayModeRef) -> f64;
        fn CGDisplayModeRelease(mode: CgDisplayModeRef);
    }

    // SAFETY: stable public CG APIs; null mode handled; Release pairs with Copy.
    let display = unsafe { CGMainDisplayID() };
    let mode = unsafe { CGDisplayCopyDisplayMode(display) };
    if mode.is_null() {
        return Err("error");
    }
    let rate = unsafe { CGDisplayModeGetRefreshRate(mode) };
    unsafe { CGDisplayModeRelease(mode) };
    // 0.0 is documented indeterminate for some LCD/VRR panels, so it maps to a skip, not an error
    // Future primary-display fallback must be thread-safe; no AppKit/NSScreen here.
    if !rate.is_finite() || rate < 0.0 {
        return Err("error");
    }
    if rate == 0.0 {
        return Err("indeterminate");
    }
    Ok(rate.round() as u32)
}

#[cfg(target_os = "windows")]
fn probe_windows() -> Result<u32, &'static str> {
    match std::panic::catch_unwind(|| {
        // SAFETY: read-only EnumDisplayDevices/Settings for primary only.
        unsafe { windows_primary_display_refresh_hz() }
    }) {
        Ok(inner) => inner,
        Err(_) => Err("error"),
    }
}

#[cfg(not(target_os = "windows"))]
fn probe_windows() -> Result<u32, &'static str> {
    Err("unsupported")
}

/// Primary monitor Hz (matches macOS `CGMainDisplayID`).
/// A null device name to `EnumDisplaySettingsW` selects the *current* adapter, which can differ from the primary on multi-monitor machines.
#[cfg(target_os = "windows")]
unsafe fn windows_primary_display_refresh_hz() -> Result<u32, &'static str> {
    // Win32's fixed-layout W structures. Keep the size and the accessed field offsets checked.
    #[repr(C)]
    struct DisplayDeviceW {
        cb: u32,
        device_name: [u16; 32],
        device_string: [u16; 128],
        state_flags: u32,
        device_id: [u16; 128],
        device_key: [u16; 128],
    }

    #[repr(C)]
    struct DevModeW {
        device_name: [u16; 32],
        spec_version: u16,
        driver_version: u16,
        size: u16,
        driver_extra: u16,
        fields: u32,
        position_or_print_fields: [u32; 4],
        color: i16,
        duplex: i16,
        y_resolution: i16,
        tt_option: i16,
        collate: i16,
        form_name: [u16; 32],
        log_pixels: u16,
        bits_per_pixel: u32,
        width: u32,
        height: u32,
        display_flags_or_nup: u32,
        display_frequency: u32,
        icm_and_panning_fields: [u32; 8],
    }

    const {
        assert!(std::mem::size_of::<DisplayDeviceW>() == 840);
        assert!(std::mem::offset_of!(DisplayDeviceW, state_flags) == 324);
        assert!(std::mem::size_of::<DevModeW>() == 220);
        assert!(std::mem::offset_of!(DevModeW, size) == 68);
        assert!(std::mem::offset_of!(DevModeW, display_frequency) == 184);
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumDisplayDevicesW(
            device: *const u16,
            index: u32,
            display: *mut DisplayDeviceW,
            flags: u32,
        ) -> i32;
        fn EnumDisplaySettingsW(device: *const u16, mode: u32, devmode: *mut DevModeW) -> i32;
    }

    const DISPLAY_DEVICE_PRIMARY_DEVICE: u32 = 0x4;
    const ENUM_CURRENT_SETTINGS: u32 = u32::MAX;

    // Bound device enumeration so a broken driver cannot spin forever.
    for i in 0u32..32 {
        // SAFETY: zeroed DISPLAY_DEVICEW with cb set is the documented pattern.
        let mut device: DisplayDeviceW = unsafe { std::mem::zeroed() };
        device.cb = std::mem::size_of::<DisplayDeviceW>() as u32;
        // SAFETY: null parent = desktop adapters; i indexes adapters.
        let ok = unsafe { EnumDisplayDevicesW(std::ptr::null(), i, &mut device, 0) };
        if ok == 0 {
            break;
        }
        if device.state_flags & DISPLAY_DEVICE_PRIMARY_DEVICE == 0 {
            continue;
        }

        // SAFETY: zeroed DEVMODEW with dmSize set; DeviceName is the primary.
        let mut devmode: DevModeW = unsafe { std::mem::zeroed() };
        devmode.size = std::mem::size_of::<DevModeW>() as u16;
        let ok = unsafe {
            EnumDisplaySettingsW(
                device.device_name.as_ptr(),
                ENUM_CURRENT_SETTINGS,
                &mut devmode,
            )
        };
        if ok == 0 {
            return Err("error");
        }
        let hz = devmode.display_frequency;
        // 0/1 often mean "default hardware rate", so fail closed
        if hz < 2 {
            return Err("error");
        }
        return Ok(hz);
    }
    Err("error")
}
