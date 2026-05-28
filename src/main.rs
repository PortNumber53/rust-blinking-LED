//! Blink the onboard LED on a Raspberry Pi Pico 2 W.
//!
//! On the Pico 2 W the user LED is NOT wired to an RP2350 GPIO. It is connected
//! to GPIO 0 of the CYW43439 wireless chip. To toggle it we must bring up the
//! CYW43 over its PIO-driven SPI link (loading the WiFi firmware blob), then ask
//! the chip to drive its GPIO 0.
//!
//! Flashing (no debug probe needed):
//!   1. Hold BOOTSEL while plugging the board into USB -> it mounts as "RP2350".
//!   2. `cargo run --release` (the `elf2uf2-rs -d` runner converts + copies the
//!      UF2 onto the mounted board, which then reboots and runs this firmware).
//!
//! This program does NOT work on a plain Pico 2 (non-W) — that board's LED is on
//! GPIO 25 and would be blinked with a normal `Output` instead.

#![no_std]
#![no_main]

use cyw43::aligned_bytes;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::{bind_interrupts, dma};
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

// Halt on panic. With no debug probe there's nowhere to print a backtrace,
// so we simply stop. (A failed CYW43 init would land here -> LED never blinks.)
use panic_halt as _;

// Program metadata for `picotool info`. Not required, but recommended.
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"rustic_base blinky"),
    embassy_rp::binary_info::rp_program_description!(
        c"Blinks the Pico 2 W onboard LED (CYW43 GPIO 0) via PIO0 SPI."
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
});

/// Background task that services the CYW43 chip. It must run continuously for
/// `control` operations (like toggling the LED GPIO) to make progress.
#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // CYW43 firmware blobs, baked into flash (4-byte aligned for the chip loader).
    let fw = aligned_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = aligned_bytes!("../cyw43-firmware/43439A0_clm.bin");
    let nvram = aligned_bytes!("../cyw43-firmware/nvram_rp2040.bin");

    // CYW43 control lines (fixed by the Pico 2 W board layout):
    //   PIN_23 = WL_ON (power), PIN_25 = SPI CS,
    //   PIN_24 = SPI DIO,       PIN_29 = SPI CLK.
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);

    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        // SPI breaks if clocked too fast; RM2_CLOCK_DIVIDER is the safe choice
        // for the RP2350 and works on both classic and RM2-module Pico 2 W boards.
        // See: https://github.com/embassy-rs/embassy/issues/3960
        RM2_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (_net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw, nvram).await;
    // Spawning a single task at startup cannot realistically fail; panic-halt if it does.
    let token = cyw43_task(runner).unwrap_or_else(|_| panic!("failed to create cyw43 task"));
    spawner.spawn(token);

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    // Blink: 250 ms on, 250 ms off.
    let delay = Duration::from_millis(250);
    loop {
        control.gpio_set(0, true).await;
        Timer::after(delay).await;
        control.gpio_set(0, false).await;
        Timer::after(delay).await;
    }
}
