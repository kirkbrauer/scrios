#!/usr/bin/env bash
# Fails unless idevice is pinned exactly: Cargo.toml asks for `=X.Y.Z`, Cargo.lock resolves exactly X.Y.Z from
# crates.io, and the README's "Compatibility" line names the same version. A bump must change all three.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
want="$(sed -n -E 's/^idevice = \{ version = "=([0-9]+\.[0-9]+\.[0-9]+)".*/\1/p' Cargo.toml)"
[ -n "$want" ] || { echo "Cargo.toml: idevice is not pinned with an exact version (version = \"=X.Y.Z\")" >&2; exit 1; }
locked="$(awk '/^\[\[package\]\]/{n=""} /^name = /{n=$3} n=="\"idevice\"" && /^version = /{gsub(/"/,"",$3); print $3}' Cargo.lock | sort -u)"
[ "$locked" = "$want" ] || { echo "Cargo.lock has idevice '${locked}', Cargo.toml pins ${want}" >&2; exit 1; }
src="$(awk '/^\[\[package\]\]/{n=""} /^name = /{n=$3} n=="\"idevice\"" && /^source = /{print $3}' Cargo.lock)"
[ "$src" = '"registry+https://github.com/rust-lang/crates.io-index"' ] || { echo "Cargo.lock: idevice comes from ${src}, not crates.io" >&2; exit 1; }
grep -q "Built against \*\*idevice ${want}\*\*" README.md || { echo "README.md: the Compatibility line does not name idevice ${want}" >&2; exit 1; }
echo "idevice pinned exactly: ${want} (Cargo.toml, Cargo.lock, README)"
