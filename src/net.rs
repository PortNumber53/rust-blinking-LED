//! Network stack bring-up, runner tasks, and the core1 network state machine.
//!
//! Everything here runs on **core1**, which owns the radio. There is exactly one
//! `embassy_net::Stack` for the whole program. It starts configured with a
//! static AP address (192.168.4.1/24) and is reconfigured at runtime to a DHCP
//! client when we switch from AP mode to STA mode. The `Stack` handle is `Copy`,
//! so it is shared by value across all tasks (HTTP, DHCP, DNS, state machine).
//!
//! The state machine no longer drives the LED directly. Instead it reports every
//! status change to core0 via [`crate::ipc::EVT`], and it drains LED commands
//! from core0 via [`crate::ipc::CMD`] — it is the sole owner of the cyw43
//! `Control`, so all GPIO toggles funnel through it.

use cyw43::NetDriver;
use cyw43_pio::PioSpi;
use embassy_net::Ipv4Cidr;
use embassy_net::{Config, StackResources, StaticConfigV4};
use embassy_rp::gpio::Output;
use embassy_rp::peripherals::PIO0;
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

use crate::dhcp_server::SERVER_IP;
use crate::ipc::{Cmd, Evt, CMD, EVT};
use crate::shared::{self, Status};

/// Max concurrent smoltcp sockets. We run: HTTP listener (1) + a couple of
/// pipelined HTTP connections, DHCP UDP (1), DNS UDP (1), and the DHCP *client*
/// + DNS *client* sockets used after the STA switch. 8 leaves comfortable slack.
pub const SOCKETS: usize = 8;

/// The static AP configuration: the Pico is 192.168.4.1, is its own gateway,
/// and advertises itself as DNS (so the catch-all resolver is reached).
pub fn ap_config() -> Config {
    let mut dns = heapless::Vec::new();
    let _ = dns.push(SERVER_IP);
    Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(SERVER_IP, 24),
        gateway: Some(SERVER_IP),
        dns_servers: dns,
    })
}

/// Switch the live stack to DHCP-client mode (used after joining an upstream AP).
pub fn reconfigure_dhcp(stack: embassy_net::Stack<'static>) {
    stack.set_config_v4(embassy_net::ConfigV4::Dhcp(Default::default()));
}

/// Switch the live stack back to the static AP address.
pub fn reconfigure_ap(stack: embassy_net::Stack<'static>) {
    let mut dns = heapless::Vec::new();
    let _ = dns.push(SERVER_IP);
    stack.set_config_v4(embassy_net::ConfigV4::Static(StaticConfigV4 {
        address: Ipv4Cidr::new(SERVER_IP, 24),
        gateway: Some(SERVER_IP),
        dns_servers: dns,
    }));
}

/// Convenience alias for the concrete cyw43 SPI bus type on the Pico 2 W.
pub type Cyw43Spi = cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>;

/// Services the CYW43 chip. Must run continuously for the whole program life —
/// including across the AP→STA switch. Never restart it.
#[embassy_executor::task]
pub async fn cyw43_task(runner: cyw43::Runner<'static, Cyw43Spi>) -> ! {
    runner.run().await
}

/// Drives the embassy-net TCP/IP stack. Also runs for the whole program life.
#[embassy_executor::task]
pub async fn net_task(mut runner: embassy_net::Runner<'static, NetDriver<'static>>) -> ! {
    runner.run().await
}

/// Build the stack from the cyw43 net device and spawn the net runner task on
/// the given (core1) spawner. The `StackResources` live in a `'static` cell.
pub fn init_stack(
    spawner: &embassy_executor::Spawner,
    net_device: NetDriver<'static>,
    seed: u64,
) -> embassy_net::Stack<'static> {
    static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        ap_config(),
        RESOURCES.init(StackResources::new()),
        seed,
    );
    // Spawning at startup cannot realistically fail; halt if it somehow does.
    spawner.spawn(net_task(runner).unwrap());
    stack
}

/// Set the shared status, and notify core0 so it can update its Morse output.
async fn set_status(s: Status) {
    shared::set_status(s).await;
    EVT.send(Evt::Status(s)).await;
}

/// Drain any pending LED commands from core0 and apply them to the radio GPIO.
/// Non-blocking: applies whatever is queued and returns.
async fn apply_pending_led(control: &mut cyw43::Control<'static>) {
    while let Ok(Cmd::SetLed(on)) = CMD.try_receive() {
        control.gpio_set(0, on).await;
    }
}

/// AP SSID. Open network — the captive portal is the gatekeeper.
const AP_SSID: &str = "Pico-Proxy";
/// 2.4 GHz channel for the AP. 6 is a common, widely-valid default.
const AP_CHANNEL: u8 = 6;

/// The core1 network state machine. Owns `Control`. Runs the AP + captive portal,
/// reports status to core0, applies LED commands from core0, and performs the
/// AP→STA switch when the user picks a network.
#[embassy_executor::task]
pub async fn network_task(
    mut control: cyw43::Control<'static>,
    stack: embassy_net::Stack<'static>,
) -> ! {
    // Phase 1: AP + captive portal.
    control.start_ap_open(AP_SSID, AP_CHANNEL).await;
    set_status(Status::Portal).await;
    shared::SCAN_REQUEST.signal(());

    loop {
        let event = wait_for_event(&mut control).await;
        match event {
            NetEvent::Scan => {
                set_status(Status::Scanning).await;
                let list = crate::wifi::scan(&mut control).await;
                {
                    let mut shared_list = shared::NETWORKS.lock().await;
                    *shared_list = list;
                }
                set_status(Status::Portal).await;
            }
            NetEvent::Connect(req) => {
                set_status(Status::Connecting).await;
                control.close_ap().await;
                reconfigure_dhcp(stack);

                let joined = crate::wifi::join(&mut control, req.ssid.as_str(), req.password.as_str())
                    .await
                    .is_ok();

                if joined && confirm_online(&mut control, stack).await {
                    set_status(Status::Connected).await;
                    // Stay here applying LED commands forever; reboot to reconfigure.
                    loop {
                        match CMD.receive().await {
                            Cmd::SetLed(on) => control.gpio_set(0, on).await,
                        }
                    }
                } else {
                    set_status(Status::Failed).await;
                    let _ = control.leave().await;
                    reconfigure_ap(stack);
                    control.start_ap_open(AP_SSID, AP_CHANNEL).await;
                    set_status(Status::Portal).await;
                    shared::SCAN_REQUEST.signal(());
                }
            }
        }
    }
}

/// What the state machine reacts to.
enum NetEvent {
    Scan,
    Connect(shared::ConnectRequest),
}

/// Wait for a scan or connect signal while continuously servicing LED commands
/// from core0. Returns as soon as either signal fires.
async fn wait_for_event(control: &mut cyw43::Control<'static>) -> NetEvent {
    use embassy_futures::select::{select3, Either3};

    loop {
        // Service any queued LED commands, then wait on the next interesting
        // thing: a connect request, a scan request, or the next LED command.
        apply_pending_led(control).await;
        match select3(
            shared::CONNECT.wait(),
            shared::SCAN_REQUEST.wait(),
            CMD.receive(),
        )
        .await
        {
            Either3::First(req) => return NetEvent::Connect(req),
            Either3::Second(()) => return NetEvent::Scan,
            Either3::Third(Cmd::SetLed(on)) => control.gpio_set(0, on).await,
        }
    }
}

/// Confirm internet reachability: wait for a DHCP lease, then resolve a known
/// host. Bounded by a timeout. While waiting, keep applying LED commands.
async fn confirm_online(
    control: &mut cyw43::Control<'static>,
    stack: embassy_net::Stack<'static>,
) -> bool {
    use embassy_futures::select::{select, Either};

    let led = async {
        loop {
            match CMD.receive().await {
                Cmd::SetLed(on) => control.gpio_set(0, on).await,
            }
        }
    };

    let work = async {
        stack.wait_config_up().await;
        match stack
            .dns_query("example.com", embassy_net::dns::DnsQueryType::A)
            .await
        {
            Ok(addrs) => !addrs.is_empty(),
            Err(_) => false,
        }
    };

    let timeout = Timer::after(Duration::from_secs(25));
    match select(select(work, timeout), led).await {
        Either::First(Either::First(ok)) => ok,
        Either::First(Either::Second(())) => false,
        Either::Second(()) => unreachable!(),
    }
}
