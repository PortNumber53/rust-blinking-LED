# pico-proxy — Pico 2 W captive-portal Wi-Fi selector (Rust)

A `no_std` Rust firmware for the **Raspberry Pi Pico 2 W** (RP2350 + CYW43439)
that turns the board into a self-hosted **captive portal**: it broadcasts its own
Wi-Fi access point, serves a web page that lists nearby Wi-Fi networks, and lets
you pick one (with its password) for the Pico to connect to. Built on
[Embassy](https://embassy.dev/) with [`embassy-net`](https://crates.io/crates/embassy-net).

> This is the first milestone. It goes as far as **joining the chosen upstream
> network and confirming internet reachability**. It does **not** yet forward or
> NAT the connected clients' traffic to the upstream — see *Roadmap* below.

## Dual-core architecture

The RP2350 has two Cortex-M33 cores, and the firmware splits work across them:

- **core0** owns the radio and the entire network stack (cyw43 bring-up, the
  embassy-net runner, DHCP/DNS/HTTP, and the AP→STA state machine). It *must* be
  core0: `embassy_rp::init()` runs there and unmasks the timer alarm IRQ and the
  cyw43 PIO/DMA IRQs only in **core0's** NVIC. The RP2350 NVIC is per-core, so an
  async task that awaits a `Timer` (or the cyw43 driver) only makes progress on
  core0 — putting that work on core1 silently hangs.
- **core1** runs the application: a **Morse-code status indicator**. It receives
  network-status events from core0 and continuously blinks the current state on
  the onboard LED in Morse (Portal→`AP`, Scanning→`S`, Connecting→`C`,
  Connected→`OK`, Failed→`ERR`). You can also type a message in the portal and
  core1 will blink it once before resuming the status pattern.

Because the LED is a CYW43 GPIO owned by core0, core1 doesn't toggle it directly:
it sends `SetLed` commands back to core0. So the LED's *meaning* is computed on
core1 and the *action* happens on core0. The two cores communicate only through
`embassy_sync` channels using `CriticalSectionRawMutex` (the only raw mutex sound
across cores). core1 times its blinks with `embassy_time::block_for` (a busy-wait
that polls the shared timer counter and needs no interrupt), since `Timer` only
works on core0.

> **Hardware constraint, learned the hard way:** the radio/timer stack cannot run
> on core1. See the per-core NVIC explanation above — this is why core0 does the
> networking and core1 does the (busy-wait-timed) Morse.

## How it works

The CYW43439 has a **single radio** that can be a Wi-Fi access point *or* a Wi-Fi
client, but not robustly both at once. So the firmware runs in two time-separated
phases:

1. **Config phase (AP + captive portal).** On boot the Pico starts an open AP
   named **`Pico-Proxy`**. It runs three network services on its own stack
   (static IP `192.168.4.1/24`):
   - a minimal **DHCP server** ([`src/dhcp_server.rs`](src/dhcp_server.rs)) that
     leases `192.168.4.x` addresses and advertises the Pico as router + DNS;
   - a **catch-all DNS server** ([`src/dns_server.rs`](src/dns_server.rs)) that
     answers every query with `192.168.4.1`, so OS captive-portal probes trigger
     the login page;
   - an **HTTP server** ([`src/http.rs`](src/http.rs)) serving the portal page
     ([`src/portal.html`](src/portal.html)), a `/networks` scan list, a
     `/connect` form handler, a `/status` poll endpoint, and a `/blink` endpoint
     (POST a message to blink it on the LED in Morse — see the dual-core section).
     Unknown paths 302-redirect to `/` (this is what pops the captive portal on
     phones).

2. **Run phase (STA / client).** When you pick a network and submit the form, the
   Pico tears down the AP (`close_ap`), reconfigures its IP stack for DHCP, and
   joins the chosen network as a client. It then waits for a DHCP lease and
   resolves a known host to confirm it's online. To reconfigure, reboot the board
   (which restarts in the config phase).

### Onboard LED status (Morse)

The onboard LED (CYW43 GPIO 0) blinks the current status as a Morse word, driven
by core1 (see the dual-core section). Unit time is 150 ms (dot = 1 unit, dash = 3):

| State        | Morse word | Code              |
| ------------ | ---------- | ----------------- |
| Portal       | `AP`       | `.-` `.--.`       |
| Scanning     | `S`        | `...`             |
| Connecting   | `C`        | `-.-.`            |
| Connected    | `OK`       | `---` `-.-`       |
| Failed       | `ERR`      | `.` `.-.` `.-.`   |

You can also type any message in the portal's **"Blink a message on the LED"**
box; core1 blinks it once in Morse (letters, digits, punctuation), then resumes
the status word.

## Using it

1. Flash the firmware (see below) and power the board.
2. On a phone/laptop, join the open Wi-Fi network **`Pico-Proxy`**. A captive-
   portal page should pop up automatically; if not, browse to
   <http://192.168.4.1/>.
3. Pick a network from the list, enter its password (leave blank for open
   networks), and tap **Connect**. The page reports progress; the AP drops while
   the Pico switches to the chosen network, which is expected.

## Toolchain

```sh
rustup target add thumbv8m.main-none-eabihf   # RP2350 ARM Cortex-M33 core
cargo install elf2uf2-rs                       # ELF -> UF2 converter
```

## Build

```sh
cargo build --release
```

Produces `target/thumbv8m.main-none-eabihf/release/rustic_base` (ELF). The crate
is still named `rustic_base`; the program identifies itself as `pico-proxy` in
`picotool info`.

## Flash (no debug probe required)

1. **Unplug** the board if connected.
2. Hold the **BOOTSEL** button, plug in USB, then release. The board mounts as a
   USB drive named **`RP2350`**.
3. Run the flash script — it builds, converts to UF2, **patches the family ID**
   (see the gotcha below), and copies it onto the mounted board:

   ```sh
   ./scripts/flash.sh             # release build, auto-flash if board mounted
   ./scripts/flash.sh --debug     # debug build
   ./scripts/flash.sh --no-copy   # build + patch only (writes ./pico-proxy.uf2)
   ```

When the bootloader accepts the image it flashes and reboots automatically — the
`RP2350` drive disappears and the open **`Pico-Proxy`** Wi-Fi network appears.

### ⚠️ Gotcha: UF2 family ID (why you need `flash.sh`)

The RP2350 bootloader only accepts UF2 images tagged with the **RP2350 ARM-S**
family ID `0xe48bff59`. Older builds of `elf2uf2-rs` (including the one this
project was developed against) hardcode the **RP2040** ID `0xe48bff56`, which the
RP2350 bootloader silently **rejects** — the `.uf2` just sits on the drive and the
board never reboots. This is *silent*: there's no error, the flash simply doesn't
take, so it's easy to mistake for a firmware bug.

[`scripts/flash.sh`](scripts/flash.sh) works around this by rewriting every
512-byte UF2 block's family-ID field from `0xe48bff56` to `0xe48bff59` after
`elf2uf2-rs` runs (validating the UF2 block magic first). If you instead have a
recent `elf2uf2-rs`, `picotool`, or a debug probe (`probe-rs run --chip RP235x`),
those sidestep the issue and you can flash directly without the script.

> Note: `cargo run --release` uses the `elf2uf2-rs -d` runner, which on this
> machine produces the **wrong** (RP2040) family ID and so will silently fail to
> flash a Pico 2 W. Use `./scripts/flash.sh` instead.

> **Debugging tip:** this firmware uses `panic-halt`, so a panic or a failed
> CYW43 bring-up halts silently with no output. If you have a debug probe, switch
> the panic handler to `panic-probe` + `defmt-rtt` and add `-Tdefmt.x` in
> `build.rs` to get logs over RTT.

## Roadmap

- [x] AP + captive portal (DHCP, catch-all DNS, HTTP).
- [x] Scan nearby networks, pick one, join as client, confirm reachability.
- [ ] **Forward/NAT** connected clients' traffic to the upstream. This requires
      concurrent AP+STA on the single radio (experimental in cyw43) plus a NAT
      layer (smoltcp has none built in), and is the main deferred piece.
- [ ] Persist the chosen network to flash so a reboot reconnects automatically.

## Project layout

| Path                   | Purpose                                                       |
| ---------------------- | ------------------------------------------------------------- |
| `src/main.rs`          | Dual-core boot: spawn core1, bring up cyw43 + net on core0.   |
| `src/ipc.rs`           | Cross-core channels (Cmd/Evt/Msg) + per-core executor statics.|
| `src/morse.rs`         | core1 Morse generator: status words + portal messages.        |
| `src/net.rs`           | embassy-net stack init, runner tasks, AP→STA state machine.   |
| `src/dhcp_server.rs`   | Minimal DHCPv4 server for AP clients.                         |
| `src/dns_server.rs`    | Catch-all DNS responder for the captive portal.              |
| `src/http.rs`          | HTTP/1.1 server: portal, scan list, connect, status, blink.   |
| `src/portal.html`      | The portal web page (embedded via `include_str!`).           |
| `src/wifi.rs`          | Wi-Fi scan (dedup + sort) and join helpers.                   |
| `src/shared.rs`        | Cross-task state (scan results, connect request, status).    |
| `scripts/flash.sh`     | Build + UF2 + family-ID patch + flash to a BOOTSEL board.     |
| `Cargo.toml`           | Embassy 0.10 + cyw43 0.7 + embassy-net 0.9 dependencies.      |
| `.cargo/config.toml`   | Build target + `elf2uf2-rs` runner.                           |
| `build.rs`             | Places `memory.x` on the linker path + linker args.          |
| `memory.x`             | RP2350 memory layout (4 MiB flash) + boot blocks.            |
| `cyw43-firmware/`      | CYW43439 firmware blobs (required to build).                  |
