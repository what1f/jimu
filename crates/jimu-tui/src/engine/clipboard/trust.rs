//! Environment-based delivery and toast policy for clipboard writes.
//!
//! A copy still writes to every backend at once; this module classifies whether a successful leg is known to reach the destination named by the UI.

use crate::engine::host::{DisplayServer, HostOs};
use crate::engine::terminal::TerminalName;

use super::{ClipboardFeedback, ClipboardWriteLegs};

/// Evidence that a clipboard write reached its intended destination.
#[derive(Debug, Clone, Copy, Eq, PartialEq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ClipboardDelivery {
    /// A successful write leg has a destination trusted by the environment policy.
    Confirmed,
    /// OSC 52 was emitted, but the outer terminal's clipboard support is unknown.
    Unverified,
    /// No usable write leg succeeded, or the destination is known not to support it.
    Failed,
}

impl ClipboardDelivery {
    pub fn is_confirmed(self) -> bool {
        self == Self::Confirmed
    }

    pub fn is_failed(self) -> bool {
        self == Self::Failed
    }

    pub fn reported_success(self) -> bool {
        matches!(self, Self::Confirmed | Self::Unverified)
    }

    pub fn telemetry_label(self) -> &'static str {
        self.into()
    }
}

/// Clipboard-relevant facts about the terminal and host environment.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[doc(hidden)]
pub struct ClipboardEnvironment {
    pub brand: TerminalName,
    pub host_os: HostOs,
    pub display_server: DisplayServer,
    pub remote: bool,
    pub container: bool,
    pub osc52_sink: bool,
    pub wayland_data_control: bool,
    pub wl_copy_available: bool,
}

/// The terminal's advertised OSC 52 clipboard capability.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[doc(hidden)]
pub enum Osc52Capability {
    Supported,
    Unsupported,
    Unknown,
}

impl Osc52Capability {
    #[doc(hidden)]
    pub fn label(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

impl ClipboardEnvironment {
    #[doc(hidden)]
    pub fn osc52_capability(self) -> Osc52Capability {
        if self.osc52_sink || self.brand.supports_osc52_clipboard() {
            Osc52Capability::Supported
        } else if self.brand == TerminalName::Unknown {
            Osc52Capability::Unknown
        } else {
            Osc52Capability::Unsupported
        }
    }
}

/// Native clipboard route evidence available before a copy is attempted.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NativeClipboardPreflight {
    Disabled,
    LocalAvailable,
    RemoteOnly,
    Unavailable,
}

fn trusted_wayland_native(wl_copy: bool, arboard: bool, data_control: bool) -> bool {
    wl_copy || (arboard && data_control)
}

/// Classify the configured native route without claiming that a write succeeded.
pub fn native_clipboard_preflight(
    route_native: bool,
    environment: ClipboardEnvironment,
) -> NativeClipboardPreflight {
    if !route_native {
        return NativeClipboardPreflight::Disabled;
    }
    if environment.remote || environment.container {
        return NativeClipboardPreflight::RemoteOnly;
    }
    match environment.host_os {
        HostOs::Linux => match environment.display_server {
            DisplayServer::Wayland
                if environment.wl_copy_available || environment.wayland_data_control =>
            {
                NativeClipboardPreflight::LocalAvailable
            }
            DisplayServer::Wayland | DisplayServer::Unknown => {
                NativeClipboardPreflight::Unavailable
            }
            DisplayServer::X11 => NativeClipboardPreflight::LocalAvailable,
            DisplayServer::Quartz | DisplayServer::Win32 => NativeClipboardPreflight::Unavailable,
        },
        HostOs::Macos | HostOs::Windows => NativeClipboardPreflight::LocalAvailable,
        HostOs::Other => NativeClipboardPreflight::Unavailable,
    }
}

/// Classify one emitted OSC 52 write.
/// Unknown SSH/container boundaries strip brand markers, so missing capability evidence is Unverified rather than Failed.
pub(crate) fn osc52_delivery(environment: ClipboardEnvironment) -> ClipboardDelivery {
    match environment.osc52_capability() {
        Osc52Capability::Supported => ClipboardDelivery::Confirmed,
        Osc52Capability::Unknown if environment.remote || environment.container => {
            ClipboardDelivery::Unverified
        }
        Osc52Capability::Unknown | Osc52Capability::Unsupported => ClipboardDelivery::Failed,
    }
}

/// Expected preflight confidence for an enabled clipboard route.
pub fn expected_delivery(
    native: NativeClipboardPreflight,
    route_tmux: bool,
    route_osc52: bool,
    environment: ClipboardEnvironment,
) -> ClipboardDelivery {
    if native == NativeClipboardPreflight::LocalAvailable {
        return ClipboardDelivery::Confirmed;
    }
    let osc52 = route_osc52.then(|| osc52_delivery(environment));
    if osc52 == Some(ClipboardDelivery::Confirmed) || route_tmux {
        return ClipboardDelivery::Confirmed;
    }
    if osc52 == Some(ClipboardDelivery::Unverified) {
        return ClipboardDelivery::Unverified;
    }
    ClipboardDelivery::Failed
}

/// True when native legs wrote the local OS clipboard rather than a remote host.
pub(crate) fn trusted_native(legs: &ClipboardWriteLegs, environment: ClipboardEnvironment) -> bool {
    if environment.remote || environment.container || !legs.route_native {
        return false;
    }
    match environment.host_os {
        HostOs::Linux => match environment.display_server {
            DisplayServer::Wayland => {
                trusted_wayland_native(legs.wl_copy_ok, legs.arboard_ok, legs.data_control)
            }
            _ => legs.cli_ok || legs.arboard_ok,
        },
        HostOs::Macos | HostOs::Windows | HostOs::Other => legs.cli_ok || legs.arboard_ok,
    }
}

/// Resolve the user-visible feedback; each feedback variant owns its delivery state.
pub(crate) fn resolve_copy_decision(
    legs: &ClipboardWriteLegs,
    text: &str,
    environment: ClipboardEnvironment,
) -> ClipboardFeedback {
    if trusted_native(legs, environment) {
        return ClipboardFeedback::Copied;
    }
    if legs.osc52_ok {
        match osc52_delivery(environment) {
            ClipboardDelivery::Confirmed => {
                if environment.container {
                    return ClipboardFeedback::CopiedOscContainer;
                }
                if environment.remote && environment.brand.is_vscode_family() && !text.is_ascii() {
                    return ClipboardFeedback::VsCodeSshNonAscii;
                }
                if environment.remote {
                    return ClipboardFeedback::CopiedOscRemote;
                }
                return ClipboardFeedback::Copied;
            }
            ClipboardDelivery::Unverified if !legs.tmux_ok => {
                if environment.container {
                    return ClipboardFeedback::UnverifiedOscContainer;
                }
                return ClipboardFeedback::UnverifiedOscRemote;
            }
            ClipboardDelivery::Unverified | ClipboardDelivery::Failed => {}
        }
    }
    if legs.tmux_ok {
        return ClipboardFeedback::CopiedTmux;
    }
    if environment.remote || environment.container {
        ClipboardFeedback::FailedRemote
    } else {
        ClipboardFeedback::Failed
    }
}
