#!/usr/bin/env bash
# Smoke test of the release binary without a phone, in the rust container: --help works, and a
# start-up failure (no usbmuxd at the given path) writes exactly one `error` control frame to
# stdout, logs to stderr, and exits 2. Run ./build.sh first.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_IMAGE="${RUST_IMAGE:-docker.io/library/rust:1-bookworm}"
BIN=/work/target/release/jumpstarter-ios-display
run() { podman run --rm -i -v "$ROOT:/work:ro" "$RUST_IMAGE" "$@"; }

run $BIN --help > /dev/null
run $BIN --version > /dev/null

out="$(mktemp)"; err="$(mktemp)"
trap 'rm -f "$out" "$err"' EXIT
set +e
run $BIN --usbmuxd /nonexistent/usbmuxd --stats-interval-ms 0 < /dev/null > "$out" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] || { echo "expected exit 2, got $code"; cat "$err"; exit 1; }
python3 -I - "$out" <<'PY'
import json, struct, sys
data = open(sys.argv[1], 'rb').read()
frames = []
while data:
    n, pts, flags = struct.unpack('>IQB', data[:13])
    frames.append((pts, flags, data[13:13 + n]))
    data = data[13 + n:]
assert len(frames) == 1, frames
pts, flags, payload = frames[0]
assert (pts, flags) == (0, 0x80), (pts, flags)
ev = json.loads(payload)
assert ev['event'] == 'error' and 'usbmuxd' in ev['message'], ev
print('ok: one error control frame:', ev['message'])
PY
grep -q "cannot connect to usbmuxd" "$err" || { echo "no log on stderr"; cat "$err"; exit 1; }
echo "smoke test passed"
