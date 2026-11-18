#!/usr/bin/env bash
# Smoke test of a release binary without a device: --help and --version work, and a start-up failure
# (no usbmuxd at the given path) writes exactly one `error` control frame to stdout, logs to stderr,
# and exits 2. Runs the binary on this host, so give it one built for this host:
#
#   scripts/smoke.sh [BINARY]   (default: target/release/scrios)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-${ROOT}/target/release/scrios}"
[ -x "$BIN" ] || { echo "no binary at $BIN: build it first (cargo build --release)" >&2; exit 2; }
run() { "$@"; }

run $BIN --help > /dev/null
run $BIN --version > /dev/null

out="$(mktemp)"; err="$(mktemp)"
# A standard input that never ends: end of input is a `stop`, which would race with a start-up failure.
exec 3< <(sleep 600)
sleeper=${!:-}
trap 'rm -f "$out" "$err"; [ -z "$sleeper" ] || kill "$sleeper" 2>/dev/null || true' EXIT
set +e
run $BIN --usbmuxd /nonexistent/usbmuxd --stats-interval-ms 0 <&3 > "$out" 2> "$err"
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

# Options that do not go together are refused with an `error` event and exit 2, before any device is touched.
refused() {
  local want="$1"; shift
  set +e
  run $BIN "$@" <&3 > "$out" 2> "$err"
  code=$?
  set -e
  [ "$code" -eq 2 ] || { echo "expected exit 2 for $*, got $code"; cat "$err"; exit 1; }
  python3 -I - "$out" "$want" <<'PY'
import json, struct, sys
data = open(sys.argv[1], 'rb').read()
n, pts, flags = struct.unpack('>IQB', data[:13])
assert (pts, flags) == (0, 0x80) and len(data) == 13 + n, "one control frame"
ev = json.loads(data[13:])
assert ev['event'] == 'error' and sys.argv[2] in ev['message'], ev
PY
}
refused "--codec h264" --codec hevc --probe-secs 3
refused "--probe-raw is for the probe" --probe-raw /tmp/never-written.raw
refused "cannot write --probe-raw" --codec h264 --probe-secs 3 --probe-raw /nonexistent-dir/x.raw
# And the probe itself fails like any start-up when there is no usbmuxd.
refused "usbmuxd" --codec h264 --probe-secs 3 --usbmuxd /nonexistent/usbmuxd
echo "smoke test passed"
