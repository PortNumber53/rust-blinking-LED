//! WiFi scan and join helpers wrapping the cyw43 `Control` API.
//!
//! These run while the radio is being driven by the cyw43 background task. Note
//! that `scan` borrows `Control` mutably for the lifetime of the returned
//! `Scanner`, so no other control call can happen until the scan completes.

use heapless::{String, Vec};

/// Max SSID length per 802.11 (32 bytes). We store it as a UTF-8 string for the
/// web UI; non-UTF-8 SSIDs are dropped from the list.
pub const SSID_MAX: usize = 32;
/// Max networks we surface in the portal. Plenty for a typical environment and
/// bounds our RAM use.
pub const MAX_NETWORKS: usize = 24;

/// One scanned network, deduplicated by SSID and keeping the strongest signal.
#[derive(Clone)]
pub struct Network {
    pub ssid: String<SSID_MAX>,
    pub rssi: i16,
}

/// The result list, sorted strongest-signal-first.
pub type NetworkList = Vec<Network, MAX_NETWORKS>;

/// Scan for nearby access points. Deduplicates by SSID (keeping the strongest
/// RSSI seen) and returns up to [`MAX_NETWORKS`] entries sorted by signal.
pub async fn scan(control: &mut cyw43::Control<'static>) -> NetworkList {
    let mut list: NetworkList = Vec::new();

    let mut scanner = control.scan(Default::default()).await;
    while let Some(bss) = scanner.next().await {
        let len = (bss.ssid_len as usize).min(SSID_MAX);
        let raw = &bss.ssid[..len];
        // Skip hidden (empty) SSIDs and any that aren't valid UTF-8.
        if raw.is_empty() {
            continue;
        }
        let name = match core::str::from_utf8(raw) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Deduplicate: if we've seen this SSID, keep the stronger signal.
        if let Some(existing) = list.iter_mut().find(|n| n.ssid.as_str() == name) {
            if bss.rssi > existing.rssi {
                existing.rssi = bss.rssi;
            }
            continue;
        }

        if list.is_full() {
            continue;
        }
        let mut ssid = String::new();
        if ssid.push_str(name).is_err() {
            continue;
        }
        // List can only fail to push if full, which we checked above.
        let _ = list.push(Network {
            ssid,
            rssi: bss.rssi,
        });
    }
    // The Scanner borrows `control`; dropping it here re-enables control ops.
    drop(scanner);

    // Sort strongest-first (descending RSSI). heapless::Vec derefs to a slice.
    list.sort_unstable_by(|a, b| b.rssi.cmp(&a.rssi));
    list
}

/// Attempt to join `ssid`. An empty `password` joins as an open network.
/// Returns `Ok(())` on association success.
pub async fn join(
    control: &mut cyw43::Control<'static>,
    ssid: &str,
    password: &str,
) -> Result<(), cyw43::JoinError> {
    let opts = if password.is_empty() {
        cyw43::JoinOptions::new_open()
    } else {
        cyw43::JoinOptions::new(password.as_bytes())
    };
    control.join(ssid, opts).await
}
