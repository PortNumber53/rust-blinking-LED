//! Pico 2 W captive-portal Wi-Fi network selector.
//!
//! Boots as an open Wi-Fi access point ("Pico-Proxy") running a captive portal.
//! A user joins the AP, the portal page pops up, lists nearby Wi-Fi networks,
//! and lets them pick one + enter its password. The Pico then drops the AP,
//! switches the single CYW43 radio to client (STA) mode, joins the chosen
//! network, and confirms internet reachability. Reboot to reconfigure.
//!
//! Why a config-then-switch flow: the CYW43439 has ONE radio. It can be an AP or
//! a client, not robustly both at once. So the portal (phone joins the Pico) and
//! the upstream join (Pico joins a router) are time-separated into two phases.
//!
//! This milestone stops at "joined + reachable". It does NOT yet forward/NAT the
//! connected clients' traffic to the upstream — that requires concurrent AP+STA
//! and a NAT layer smoltcp doesn't provide, and is deferred.
//!
//! Onboard LED (CYW43 GPIO 0) is a coarse status indicator:
//!   - slow blink : AP up, portal waiting for a selection
//!   - fast blink : connecting to the chosen upstream
//!   - solid on   : connected + internet confirmed

#![no_std]
#![no_main]

mod dhcp_server;
mod dns_server;
mod http;
mod net;
mod shared;
mod wifi;

use cyw43::aligned_bytes;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::{bind_interrupts, dma};
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

use shared::Status;

// Halt on panic. With no debug probe there's nowhere to print a backtrace.
use panic_halt as _;

/// AP SSID. Open network — the captive portal is the gatekeeper.
const AP_SSID: &str = "Pico-Proxy";
/// 2.4 GHz channel for the AP. 6 is a common, widely-valid default.
const AP_CHANNEL: u8 = 6;

#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"pico-proxy"),
    embassy_rp::binary_info::rp_program_description!(
        c"Captive-portal WiFi network selector for the Pico 2 W (CYW43)."
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let mut rng = RoscRng;

    // --- CYW43 bring-up (identical wiring to the original blink firmware) ---
    let fw = aligned_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = aligned_bytes!("../cyw43-firmware/43439A0_clm.bin");
    let nvram = aligned_bytes!("../cyw43-firmware/nvram_rp2040.bin");

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);

    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        RM2_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw, nvram).await;
    // Spawning at startup cannot realistically fail; panic-halt if it does.
    spawner.spawn(net::cyw43_task(runner).unwrap());

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    // --- Network stack + service tasks ---
    let seed = rng.next_u64();
    let stack = net::init_stack(&spawner, net_device, seed);

    spawner.spawn(dhcp_server::dhcp_server_task(stack).unwrap());
    spawner.spawn(dns_server::dns_server_task(stack).unwrap());
    spawner.spawn(http::http_server_task(stack).unwrap());

    // --- Phase 1: AP + captive portal ---
    control.start_ap_open(AP_SSID, AP_CHANNEL).await;
    shared::set_status(Status::Portal).await;
    // Kick an initial scan so the list is ready when the first client connects.
    shared::SCAN_REQUEST.signal(());

    // Main state machine: handle scan requests and the one connect request.
    run_state_machine(&mut control, stack).await;
}

/// The portal/connect state machine. Loops handling scans until the user submits
/// a connect request, then performs the AP→STA switch and reports the result.
async fn run_state_machine(
    control: &mut cyw43::Control<'static>,
    stack: embassy_net::Stack<'static>,
) -> ! {
    loop {
        // Wait for either a scan request or a connect request, blinking the LED
        // at the "portal" cadence while we wait.
        let event = wait_for_event(control).await;

        match event {
            Event::Scan => {
                shared::set_status(Status::Scanning).await;
                let list = wifi::scan(control).await;
                {
                    let mut shared_list = shared::NETWORKS.lock().await;
                    *shared_list = list;
                }
                shared::set_status(Status::Portal).await;
            }
            Event::Connect(req) => {
                shared::set_status(Status::Connecting).await;
                // Tear down the AP so the radio is free for client mode.
                control.close_ap().await;
                // Reconfigure the IP stack to obtain a lease from the upstream.
                net::reconfigure_dhcp(stack);

                let joined = wifi::join(control, req.ssid.as_str(), req.password.as_str())
                    .await
                    .is_ok();

                if joined && confirm_online(control, stack).await {
                    shared::set_status(Status::Connected).await;
                    // Solid LED, then idle — the HTTP /status task keeps serving.
                    control.gpio_set(0, true).await;
                    idle_connected().await;
                } else {
                    // Roll back to the portal so the user can retry.
                    shared::set_status(Status::Failed).await;
                    let _ = control.leave().await;
                    net::reconfigure_ap(stack);
                    control.start_ap_open(AP_SSID, AP_CHANNEL).await;
                    shared::set_status(Status::Portal).await;
                    shared::SCAN_REQUEST.signal(());
                }
            }
        }
    }
}

/// An event the state machine reacts to.
enum Event {
    Scan,
    Connect(shared::ConnectRequest),
}

/// Wait for a scan or connect signal, blinking the LED at the portal cadence.
/// Returns as soon as either signal fires.
async fn wait_for_event(control: &mut cyw43::Control<'static>) -> Event {
    use embassy_futures::select::{select3, Either3};

    let mut led = false;
    loop {
        let blink = Timer::after(Duration::from_millis(500));
        match select3(shared::CONNECT.wait(), shared::SCAN_REQUEST.wait(), blink).await {
            Either3::First(req) => return Event::Connect(req),
            Either3::Second(()) => return Event::Scan,
            Either3::Third(()) => {
                led = !led;
                control.gpio_set(0, led).await;
            }
        }
    }
}

/// Confirm internet reachability: wait for a DHCP lease, then resolve a known
/// hostname. Returns true if both succeed within a timeout.
async fn confirm_online(
    control: &mut cyw43::Control<'static>,
    stack: embassy_net::Stack<'static>,
) -> bool {
    use embassy_futures::select::{select, Either};

    // Fast blink while we wait for the lease + DNS.
    let blink = async {
        let mut led = false;
        loop {
            led = !led;
            control.gpio_set(0, led).await;
            Timer::after(Duration::from_millis(120)).await;
        }
    };

    let work = async {
        // Wait for DHCP to assign an address (link up + config up).
        stack.wait_config_up().await;
        // Resolve a well-known host as a liveness check.
        match stack
            .dns_query("example.com", embassy_net::dns::DnsQueryType::A)
            .await
        {
            Ok(addrs) => !addrs.is_empty(),
            Err(_) => false,
        }
    };

    // Bound the whole check so a bad password / no-DHCP network doesn't hang.
    let timeout = Timer::after(Duration::from_secs(25));
    match select(select(work, timeout), blink).await {
        Either::First(Either::First(ok)) => ok,    // work finished first
        Either::First(Either::Second(())) => false, // timed out
        Either::Second(()) => unreachable!(),       // blink never returns
    }
}

/// After a successful connection, idle forever. The radio + stack + HTTP status
/// task keep running; the user reboots to reconfigure.
async fn idle_connected() -> ! {
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}
