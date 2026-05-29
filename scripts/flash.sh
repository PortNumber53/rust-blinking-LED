#!/usr/bin/env bash
#
# Build, convert, family-ID-patch, and flash the firmware onto a Pico 2 W that
# is mounted in BOOTSEL mode.
#
# Why this script exists: the `elf2uf2-rs` currently installed stamps the
# RP2040 UF2 family ID (0xe48bff56) on every block. The RP2350 (Pico 2 W)
# bootloader only accepts the RP2350 ARM-S family ID (0xe48bff59) and *silently
# rejects* anything else — the .uf2 just sits on the drive and the board never
# reboots. This script patches every 512-byte UF2 block from 56 -> 59 before
# copying, so flashing "just works" with the toolchain on this machine.
#
# Usage:
#   ./scripts/flash.sh              # release build, auto-copy if board mounted
#   ./scripts/flash.sh --debug      # debug build instead of release
#   ./scripts/flash.sh --no-copy    # build + patch only, don't copy to the board
#
# Put the board in BOOTSEL first: hold BOOTSEL, plug in USB, release. It mounts
# as /Volumes/RP2350 (macOS).

set -euo pipefail

PROFILE="release"
PROFILE_FLAG="--release"
DO_COPY=1

for arg in "$@"; do
  case "$arg" in
    --debug)   PROFILE="debug"; PROFILE_FLAG="" ;;
    --no-copy) DO_COPY=0 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

# Resolve repo root from this script's location so it works from anywhere.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="thumbv8m.main-none-eabihf"
ELF="target/$TARGET/$PROFILE/rustic_base"
UF2="$ROOT/pico-proxy.uf2"
VOLUME="/Volumes/RP2350"

echo ">> Building ($PROFILE)..."
# shellcheck disable=SC2086
cargo build $PROFILE_FLAG

if [[ ! -f "$ELF" ]]; then
  echo "!! ELF not found at $ELF" >&2
  exit 1
fi

echo ">> Converting ELF -> UF2..."
elf2uf2-rs "$ELF" "$UF2"

echo ">> Patching UF2 family ID 0xe48bff56 (RP2040) -> 0xe48bff59 (RP2350 ARM-S)..."
python3 - "$UF2" <<'PY'
import struct, sys
path = sys.argv[1]
data = bytearray(open(path, "rb").read())
RP2040, RP2350 = 0xe48bff56, 0xe48bff59
MAGIC0, MAGIC1, MAGIC_END = 0x0A324655, 0x9E5D5157, 0x0AB16F30
patched = 0
nblocks = len(data) // 512
for i in range(0, len(data), 512):
    blk = data[i:i + 512]
    if len(blk) < 512:
        break
    # Validate this really is a UF2 block before touching it.
    assert struct.unpack_from("<I", blk, 0)[0] == MAGIC0, f"bad magic0 at block {i // 512}"
    assert struct.unpack_from("<I", blk, 4)[0] == MAGIC1, f"bad magic1 at block {i // 512}"
    assert struct.unpack_from("<I", blk, 508)[0] == MAGIC_END, f"bad end magic at block {i // 512}"
    if struct.unpack_from("<I", blk, 28)[0] == RP2040:
        struct.pack_into("<I", data, i + 28, RP2350)
        patched += 1
open(path, "wb").write(data)
fams = {struct.unpack_from("<I", data, i + 28)[0]
        for i in range(0, len(data), 512) if len(data[i:i + 512]) == 512}
print(f"   {nblocks} blocks, patched {patched}, family ids now: {[hex(f) for f in fams]}")
assert fams == {RP2350}, "not all blocks ended up RP2350 — refusing to flash"
PY

echo ">> UF2 ready: $UF2"

if [[ "$DO_COPY" -eq 0 ]]; then
  echo ">> --no-copy: skipping flash. Copy it yourself with:"
  echo "   cp \"$UF2\" \"$VOLUME/\""
  exit 0
fi

if [[ ! -d "$VOLUME" ]]; then
  echo "!! Board not mounted at $VOLUME."
  echo "   Put the board in BOOTSEL mode (hold BOOTSEL, plug in USB, release), then re-run."
  echo "   Or copy manually: cp \"$UF2\" \"$VOLUME/\""
  exit 1
fi

echo ">> Flashing to $VOLUME ..."
# The xattr copy can warn on FAT volumes; the file data still lands fine.
cp "$UF2" "$VOLUME/" 2>/dev/null || cp "$UF2" "$VOLUME/" || true
sync
sleep 3

if [[ -d "$VOLUME" ]]; then
  echo "!! $VOLUME is still mounted — the image may have been rejected."
  echo "   Check the family-ID patch above and your elf2uf2-rs version."
  exit 1
fi

echo ">> Done. RP2350 unmounted — the board accepted the image and rebooted."
echo "   Look for the open 'Pico-Proxy' Wi-Fi network."
