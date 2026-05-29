//! Cross-core IPC: channels and the per-core executor/stack statics.
//!
//! The RP2350 has two Cortex-M33 cores. core0 owns the radio and the whole
//! network stack (cyw43 + embassy-net + DHCP/DNS/HTTP + the AP→STA state
//! machine) because `embassy_rp::init` runs on core0 and unmasks the timer +
//! PIO/DMA IRQs only in core0's (per-core) NVIC — so timer/peripheral-driven
//! async tasks only make progress on core0. core1 runs the application — here, a
//! Morse-code status indicator that uses `block_for` busy-wait timing (no IRQ).
//!
//! The two cores talk ONLY through the channels below. They use
//! `CriticalSectionRawMutex`, which is the only embassy-sync raw mutex sound
//! across cores: `ThreadModeRawMutex`/`NoopRawMutex` are single-executor only
//! and would provide no real mutual exclusion between cores.
//!
//! Direction of each channel:
//!   - [`EVT`]  core0 → core1 : network status changes.
//!   - [`CMD`]  core1 → core0 : LED on/off requests (core1 decides the blink
//!     pattern; core0 owns the CYW43 GPIO and performs the toggle).

use embassy_executor::Executor;
use embassy_rp::multicore::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

use crate::shared::Status;

/// Commands core1 → core0.
#[derive(Clone, Copy)]
pub enum Cmd {
    /// Drive the onboard LED (CYW43 GPIO 0) on/off. Sent by core1's Morse
    /// generator; executed by core0, which owns the radio.
    SetLed(bool),
}

/// Events core0 → core1.
#[derive(Clone, Copy)]
pub enum Evt {
    /// The network status changed. core1 re-derives its Morse message from this.
    Status(Status),
}

/// Max length of a user-submitted message to blink in Morse.
pub const MSG_MAX: usize = 64;

/// A user-typed message (from the portal) for core1 to blink once on the LED.
pub type Message = heapless::String<MSG_MAX>;

/// core1 → core0 command channel. Small depth: LED commands are paced by core1.
pub static CMD: Channel<CriticalSectionRawMutex, Cmd, 4> = Channel::new();

/// core0 → core1 event channel.
pub static EVT: Channel<CriticalSectionRawMutex, Evt, 4> = Channel::new();

/// core0 → core1 message channel: text submitted on the portal to blink once in
/// Morse. Depth 2 so a quick double-submit isn't lost; extras are dropped.
pub static MSG: Channel<CriticalSectionRawMutex, Message, 2> = Channel::new();

/// Stack for core1's executor (bytes). The network stack + service tasks need
/// considerably more than the 4 KiB the upstream blinky example uses; 16 KiB
/// leaves headroom (the MPU stack guard installed by `spawn_core1` will fault
/// loudly if this is ever too small).
pub const CORE1_STACK_SIZE: usize = 16 * 1024;

/// core1's stack. Accessed once, via `addr_of_mut!`, when spawning core1.
pub static mut CORE1_STACK: Stack<CORE1_STACK_SIZE> = Stack::new();

/// The two thread-mode executors, one per core, in `'static` cells.
pub static EXECUTOR0: static_cell::StaticCell<Executor> = static_cell::StaticCell::new();
pub static EXECUTOR1: static_cell::StaticCell<Executor> = static_cell::StaticCell::new();
