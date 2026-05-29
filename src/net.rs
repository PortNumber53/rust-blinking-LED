//! Network stack bring-up and the two background runner tasks.
//!
//! There is exactly one `embassy_net::Stack` for the whole program. It starts
//! configured with a static AP address (192.168.4.1/24) and is reconfigured at
//! runtime to a DHCP client when we switch from AP mode to STA mode (see
//! [`reconfigure_dhcp`]). The `Stack` handle is `Copy`, so it is shared by value
//! across all tasks (HTTP, DHCP, DNS, and the main state machine).

use cyw43::NetDriver;
use cyw43_pio::PioSpi;
use embassy_net::{Config, StackResources, StaticConfigV4};
use embassy_net::Ipv4Cidr;
use embassy_rp::gpio::Output;
use embassy_rp::peripherals::PIO0;
use static_cell::StaticCell;

use crate::dhcp_server::SERVER_IP;

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

/// Build the stack from the cyw43 net device. Returns the `Stack` handle and
/// spawns the net runner task. The `StackResources` live in a `'static` cell.
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
