//! Pico 2 W captive-portal Wi-Fi selector — dual-core edition.
//!
//! The RP2350 has two Cortex-M33 cores, and this firmware splits work across
//! them:
//!   - **core0** owns the radio and the entire network stack: cyw43 bring-up,
//!     the cyw43 + embassy-net runners, the DHCP/DNS/HTTP service tasks, and the
//!     AP→STA state machine. It MUST be core0 because `embassy_rp::init` runs
//!     here and unmasks the timer alarm IRQ (and the cyw43 PIO/DMA IRQs) only in
//!     core0's NVIC — the RP2350 NVIC is per-core, so async drivers that await
//!     timers/peripherals only make progress on core0.
//!   - **core1** runs the application: a Morse-code status indicator. It receives
//!     network-status events from core0 and blinks the current state on the
//!     onboard LED in Morse, by sending LED on/off commands back to core0 (which
//!     owns the CYW43 GPIO). core1 uses `block_for` busy-wait timing, since
//!     `Timer` doesn't work off-core. So the LED's meaning is computed on core1
//!     and the toggle happens on core0 — a visible proof of the cross-core link.
//!
//! The Wi-Fi behavior is unchanged from the single-core version: boot as the
//! open AP "Pico-Proxy" with a captive portal, let the user pick a network, then
//! switch to client mode and confirm reachability. Reboot to reconfigure.
//!
//! Morse status words (blinked on the LED, computed by core1):
//!   Portal → "AP", Scanning → "S", Connecting → "C",
//!   Connected → "OK", Failed → "ERR".

#![no_std]
#![no_main]

mod dhcp_server;
mod dns_server;
mod http;
mod ipc;
mod morse;
mod net;
mod shared;
mod wifi;

use core::ptr::addr_of_mut;

use cyw43::aligned_bytes;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_executor::{Executor, Spawner};
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::multicore::spawn_core1;
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::{bind_interrupts, dma};
use static_cell::StaticCell;

// Halt on panic. With no debug probe there's nowhere to print a backtrace.
use panic_halt as _;

#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"pico-proxy"),
    embassy_rp::binary_info::rp_program_description!(
        c"Dual-core captive-portal WiFi selector + Morse status (Pico 2 W)."
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
    // embassy_rp::init runs exactly once, on core0; it hands out all Peri tokens
    // and unmasks the timer + (later) peripheral IRQs in core0's NVIC. All
    // timer/radio-driven tasks therefore live on core0 (this executor).
    let p = embassy_rp::init(Default::default());

    // Bring up core1 first: it only runs the Morse app (no timers via the IRQ
    // driver, no peripherals), so it needs no Peri tokens.
    spawn_core1(
        p.CORE1,
        unsafe { &mut *addr_of_mut!(ipc::CORE1_STACK) },
        || {
            let executor1 = ipc::EXECUTOR1.init(Executor::new());
            executor1.run(|spawner| {
                spawner.spawn(morse::morse_task().unwrap());
            });
        },
    );

    // --- core0: CYW43 bring-up (same wiring as the original blink firmware) ---
    let mut rng = RoscRng;
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
    spawner.spawn(net::cyw43_task(runner).unwrap());

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    // --- core0: network stack + service tasks ---
    let stack = net::init_stack(&spawner, net_device, rng.next_u64());
    spawner.spawn(dhcp_server::dhcp_server_task(stack).unwrap());
    spawner.spawn(dns_server::dns_server_task(stack).unwrap());
    spawner.spawn(http::http_server_task(stack).unwrap());

    // The state machine owns `control` (sole radio-GPIO owner): it runs the AP +
    // captive portal, reports status to core1, and applies core1's LED commands.
    spawner.spawn(net::network_task(control, stack).unwrap());
}
