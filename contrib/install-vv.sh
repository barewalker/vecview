#!/usr/bin/env bash
# Build vv against an old glibc and install it into ~/.cargo/bin.
#
# Who this is for: a split build/run environment where you BUILD inside a newer
# distro (e.g. an Arch distrobox/podman container, glibc 2.4x) but RUN on an older
# host (e.g. Ubuntu, glibc 2.39) that shares ~/.cargo/bin. A plain `cargo install`
# inside the container links against the container's glibc and then fails to start
# on the host ("version `GLIBC_2.4x' not found"). cargo-zigbuild lets us target an
# old glibc from inside the container, so the single shared binary runs on both.
#
# Not needed for normal single-machine installs — just use `cargo install vecview`.
#
# Prerequisites:
#   - cargo-zigbuild   (cargo install cargo-zigbuild)
#   - zig              (on PATH, or unpacked under ~/.local/share/zig-*/)
#   - the rust std for x86_64-unknown-linux-gnu (default on most toolchains)
#
# Usage:
#   contrib/install-vv.sh            # build for GLIBC_TARGET (default below) and install
#   GLIBC_TARGET=2.39 contrib/install-vv.sh
set -euo pipefail

# Lowest glibc we need to run on. 2.35 gives headroom over a 2.39 host and matches
# the prebuilt release binaries. A binary built for an old glibc runs on every newer one.
GLIBC_TARGET="${GLIBC_TARGET:-2.35}"
RUST_TARGET="x86_64-unknown-linux-gnu"
ZIGBUILD_TARGET="${RUST_TARGET}.${GLIBC_TARGET}"

# Repo root = parent of this script's directory (contrib/..).
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"

# --- locate zig (cargo-zigbuild needs it as the linker) ----------------------
# Prefer a pinned zig under ~/.local/share/zig-*; fall back to one on PATH.
if ! command -v zig >/dev/null 2>&1; then
  PINNED_ZIG="$(ls -d "$HOME"/.local/share/zig-*/zig 2>/dev/null | sort -V | tail -1 || true)"
  if [ -n "${PINNED_ZIG:-}" ] && [ -x "$PINNED_ZIG" ]; then
    export PATH="$(dirname "$PINNED_ZIG"):$PATH"
  else
    echo "error: 'zig' not found. Install it first, e.g.:" >&2
    echo "  ZIG_VER=0.14.1" >&2
    echo "  curl -fsSL https://ziglang.org/download/\$ZIG_VER/zig-x86_64-linux-\$ZIG_VER.tar.xz | \\" >&2
    echo "    tar -xJ -C \"\$HOME/.local/share\" && mv \"\$HOME/.local/share/zig-x86_64-linux-\$ZIG_VER\" \"\$HOME/.local/share/zig-\$ZIG_VER\"" >&2
    echo "  (or: sudo pacman -S zig)" >&2
    exit 1
  fi
fi

command -v cargo-zigbuild >/dev/null 2>&1 || {
  echo "error: cargo-zigbuild not found. Install with: cargo install cargo-zigbuild" >&2
  exit 1
}

echo ">> zig:          $(command -v zig) ($(zig version))"
echo ">> glibc target: $ZIGBUILD_TARGET"

# --- build -------------------------------------------------------------------
cd "$REPO_DIR"
cargo zigbuild --release -p vecview --target "$ZIGBUILD_TARGET"

# cargo-zigbuild strips the .<glibc> suffix from the artifact directory.
BIN_SRC="$REPO_DIR/target/$RUST_TARGET/release/vv"
[ -x "$BIN_SRC" ] || { echo "error: built binary not found at $BIN_SRC" >&2; exit 1; }

# --- install -----------------------------------------------------------------
mkdir -p "$CARGO_BIN"
install -m755 "$BIN_SRC" "$CARGO_BIN/vv"

echo ">> installed:    $CARGO_BIN/vv"
MAXGLIBC="$(objdump -T "$CARGO_BIN/vv" 2>/dev/null | grep -oE 'GLIBC_[0-9.]+' | sort -V | tail -1 || echo '?')"
echo ">> max glibc req: ${MAXGLIBC:-?}  (must be <= host glibc)"
"$CARGO_BIN/vv" --version
