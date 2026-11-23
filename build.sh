#!/bin/bash
# Build one Skyward-vivo recovery image from its device configuration.
# Set DEVICE to the codename of devices/<codename>.toml.
# Set SKYWARD_TOOLCHAIN to a folder with rustup/ and cargo/ to use a
# toolchain other than the one on PATH. Set STOCK_RECOVERY to the stock
# recovery image; it defaults to stock/recovery.img.
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DEVICE=${DEVICE:?set DEVICE to the codename of devices/<codename>.toml}
CONFIG="$ROOT/devices/$DEVICE.toml"
STOCK=${STOCK_RECOVERY:-"$ROOT/stock/recovery.img"}

if [ -n "${SKYWARD_TOOLCHAIN:-}" ]; then
  export RUSTUP_HOME="$SKYWARD_TOOLCHAIN/rustup"
  export CARGO_HOME="$SKYWARD_TOOLCHAIN/cargo"
  export PATH="$CARGO_HOME/bin:$PATH"
fi
if [ -z "${ANDROID_NDK_HOME:-}" ] && [ -n "${ANDROID_NDK_ROOT:-}" ]; then
  export ANDROID_NDK_HOME="$ANDROID_NDK_ROOT"
fi
: "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to the Android NDK r29}"
if [ ! -f "$CONFIG" ]; then
  echo "no device configuration at $CONFIG" >&2
  exit 1
fi
if [ ! -f "$STOCK" ]; then
  echo "no stock recovery at $STOCK; set STOCK_RECOVERY to the image taken from the full package" >&2
  exit 1
fi

cd "$ROOT"

echo "== host: skyward-pack"
cargo build --locked --release -p skyward-pack

echo "== android: skyward"
cargo ndk -t arm64-v8a --platform 30 build --locked --release -p skyward

echo "== android: skyward-system-init (static)"
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo ndk -t arm64-v8a --platform 30 build --locked --release \
  -p skyward-system-init --target-dir target/system-init

echo "== checking the stock recovery against $CONFIG"
expected=$(target/release/skyward-pack stock --config "$CONFIG" | sed -n 's/^recovery=//p')
actual=$(sha256sum "$STOCK" | awk '{print $1}')
if [ -z "$expected" ]; then
  echo "the device configuration names no stock recovery hash" >&2
  exit 1
fi
if [ "$actual" != "$expected" ]; then
  echo "stock recovery SHA-256 is $actual, expected $expected" >&2
  exit 1
fi

echo "== pack: skyward-vivo-$DEVICE.img"
mkdir -p "$ROOT/out"
target/release/skyward-pack recovery \
  --stock "$STOCK" \
  --binary target/aarch64-linux-android/release/skyward \
  --system-init target/system-init/aarch64-linux-android/release/system-init \
  --config "$CONFIG" \
  --font resources/fonts/MiSansVF.ttf \
  --mono resources/fonts/KirkMono-Regular.ttf \
  --mono-license resources/fonts/KirkMono-OFL.txt \
  --out "$ROOT/out/skyward-vivo-$DEVICE.img"

ls -lh "$ROOT/out/skyward-vivo-$DEVICE.img"
sha256sum "$ROOT/out/skyward-vivo-$DEVICE.img"
