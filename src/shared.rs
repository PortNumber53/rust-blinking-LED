//! Shared state between the HTTP server task and the main state machine.
//!
//! The HTTP task cannot touch the radio directly — the cyw43 `Control` handle
//! lives in `main`. So the two communicate through this module:
//!   - `NETWORKS`: the latest scan results, produced by main, read by HTTP.
//!   - `CONNECT`: a one-shot request (SSID + password) the HTTP task raises when
//!     the user submits the form; main consumes it and performs the join.
//!   - `STATUS`: the current phase, written by main, polled by the HTTP `/status`
//!     endpoint so the web UI can show progress.
//!   - `SCAN_REQUEST`: raised by HTTP when the portal page loads, so main runs a
//!     fresh scan.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use heapless::String;

use crate::wifi::{NetworkList, SSID_MAX};

/// Max Wi-Fi passphrase length (WPA2 PSK is up to 63 chars).
pub const PASSWORD_MAX: usize = 63;

/// Current high-level phase of the device, surfaced to the web UI.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// AP up, portal serving, waiting for the user to pick a network.
    Portal,
    /// A scan is in progress.
    Scanning,
    /// Switching to the chosen network (closing AP, joining).
    Connecting,
    /// Joined the upstream network and confirmed internet reachability.
    Connected,
    /// The join attempt failed; back to the portal so the user can retry.
    Failed,
}

impl Status {
    /// Short machine-readable token used by the `/status` JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Portal => "portal",
            Status::Scanning => "scanning",
            Status::Connecting => "connecting",
            Status::Connected => "connected",
            Status::Failed => "failed",
        }
    }
}

/// A pending connect request from the web form.
#[derive(Clone)]
pub struct ConnectRequest {
    pub ssid: String<SSID_MAX>,
    pub password: String<PASSWORD_MAX>,
}

/// Latest scan results (written by main after a scan, read by HTTP to render).
pub static NETWORKS: Mutex<CriticalSectionRawMutex, NetworkList> = Mutex::new(NetworkList::new());

/// Raised by HTTP when the portal page is requested; main runs a scan in response.
pub static SCAN_REQUEST: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Raised by HTTP when the user submits the connect form.
pub static CONNECT: Signal<CriticalSectionRawMutex, ConnectRequest> = Signal::new();

/// Current status, written by main, polled by `/status`.
pub static STATUS: Mutex<CriticalSectionRawMutex, Status> = Mutex::new(Status::Portal);

/// Helper: read the current status.
pub async fn status() -> Status {
    *STATUS.lock().await
}

/// Helper: set the current status.
pub async fn set_status(s: Status) {
    *STATUS.lock().await = s;
}
