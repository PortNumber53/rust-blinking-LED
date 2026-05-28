# rustic_base — Pico 2 W onboard LED blink (Rust)

A minimal `no_std` Rust firmware that blinks the **onboard LED of a Raspberry Pi
Pico 2 W** (RP2350). Built on [Embassy](https://embassy.dev/).

## Why this needs the WiFi chip

On the Pico 2 W the user LED is **not** wired to an RP2350 GPIO — it hangs off
**GPIO 0 of the CYW43439 wireless chip**. So even "just blink an LED" requires
bringing up the CYW43 over its PIO-driven SPI link and loading the WiFi firmware
blob. The blobs in [`cyw43-firmware/`](cyw43-firmware/) are baked into the binary
at compile time and are required to build.

> A plain Pico 2 (non-W) puts the LED on GPIO 25 — this firmware will **not**
> blink it. That board needs a regular `gpio::Output` instead.

## Toolchain

```sh
rustup target add thumbv8m.main-none-eabihf   # RP2350 ARM Cortex-M33 core
cargo install elf2uf2-rs                       # ELF -> UF2 converter
```

## Build

```sh
cargo build --release
```

Produces `target/thumbv8m.main-none-eabihf/release/rustic_base` (ELF).

## Flash (no debug probe required)

1. **Unplug** the board if connected.
2. Hold the **BOOTSEL** button, plug in USB, then release. The board mounts as a
   USB drive named **`RP2350`**.
3. Convert + copy the firmware:

   ```sh
   elf2uf2-rs target/thumbv8m.main-none-eabihf/release/rustic_base rustic_base.uf2
   cp rustic_base.uf2 /Volumes/RP2350/      # macOS
   ```

When the bootloader accepts the image it flashes and reboots automatically — the
`RP2350` drive disappears. The LED then blinks at ~2 Hz (250 ms on / 250 ms off).

### ⚠️ Gotcha: UF2 family ID

The RP2350 bootloader only accepts UF2 images tagged with the **RP2350 ARM-S**
family ID `0xe48bff59`. Older builds of `elf2uf2-rs` hardcode the **RP2040** ID
`0xe48bff56`, which the RP2350 bootloader silently **rejects** (the `.uf2` just
sits on the drive and the board never reboots).

If your `elf2uf2-rs` is old, either upgrade it, use `picotool`, or patch the
family ID in every 512-byte UF2 block (`0xe48bff56` -> `0xe48bff59`). A
debug-probe workflow (`probe-rs run --chip RP235x`) sidesteps this entirely.

## Project layout

| Path                 | Purpose                                                  |
| -------------------- | -------------------------------------------------------- |
| `src/main.rs`        | The blink program (drives LED via CYW43 GPIO 0).         |
| `Cargo.toml`         | Embassy 0.10 + cyw43 0.7 dependencies.                   |
| `.cargo/config.toml` | Build target + `elf2uf2-rs` runner.                      |
| `build.rs`           | Places `memory.x` on the linker path + linker args.      |
| `memory.x`           | RP2350 memory layout (4 MiB flash) + boot blocks.        |
| `cyw43-firmware/`    | CYW43439 firmware blobs (required to control the LED).   |
