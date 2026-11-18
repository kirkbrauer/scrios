#!/usr/bin/env bash
# Smoke test of a release binary without a device:
#   * --help and --version work;
#   * `scrios list` and --list-devices read a fake usbmuxd;
#   * a start-up failure (no usbmuxd at the given path, or no such UDID) in the framed format writes exactly one
#     `error` control frame to stdout, logs to stderr, and exits 2; in the raw format stdout stays empty;
#   * options that do not go together are refused, with an `error` event in the framed format;
#   * the video is never written to a terminal.
# Runs the binary on this host, so give it one built for this host:
#
#   scripts/smoke.sh [BINARY]   (default: target/release/scrios)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-${ROOT}/target/release/scrios}"
[ -x "$BIN" ] || { echo "no binary at $BIN: build it first (cargo build --release)" >&2; exit 2; }
run() { "$@"; }

run $BIN --help > /dev/null
run $BIN -h > /dev/null
run $BIN stream --help > /dev/null
run $BIN list --help > /dev/null
run $BIN --version > /dev/null

out="$(mktemp)"; err="$(mktemp)"; dir="$(mktemp -d)"
# A standard input that never ends: end of input is a `stop`, which would race with a start-up failure.
exec 3< <(sleep 600)
sleeper=${!:-}
mux_pid=
cleanup() {
  rm -rf "$out" "$err" "$dir"
  [ -z "$sleeper" ] || kill "$sleeper" 2>/dev/null || true
  [ -z "$mux_pid" ] || kill "$mux_pid" 2>/dev/null || true
}
trap cleanup EXIT

# The one error control frame that a framed stdout holds after a failure that exit status 2 reports.
one_error_frame() {
  python3 -I - "$out" "$1" <<'PY'
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
assert ev['event'] == 'error' and sys.argv[2] in ev['message'], ev
print('ok: one error control frame:', ev['message'])
PY
}

# A start-up failure in the framed format: one `error` frame on stdout, a log on stderr, exit 2.
set +e
run $BIN --format framed --usbmuxd /nonexistent/usbmuxd --stats-interval-ms 0 <&3 > "$out" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] || { echo "expected exit 2, got $code"; cat "$err"; exit 1; }
one_error_frame usbmuxd
grep -q "cannot connect to usbmuxd" "$err" || { echo "no log on stderr"; cat "$err"; exit 1; }

# The same failure in the default (raw) format: nothing on stdout, the same log and status.
set +e
run $BIN --usbmuxd /nonexistent/usbmuxd <&3 > "$out" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] || { echo "expected exit 2, got $code"; cat "$err"; exit 1; }
[ ! -s "$out" ] || { echo "the raw format wrote something on a failed start"; exit 1; }
grep -q "cannot connect to usbmuxd" "$err" || { echo "no log on stderr"; cat "$err"; exit 1; }
echo "ok: a failed start writes nothing in the raw format"

# The old spellings still parse: --hid and --codec h264 get as far as the missing usbmuxd.
set +e
run $BIN --hid --codec h264 --usbmuxd /nonexistent/usbmuxd <&3 > "$out" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] && grep -q "cannot connect to usbmuxd" "$err" || { echo "the old flags are refused"; cat "$err"; exit 1; }

# Options that do not go together are refused with an `error` event (framed) and exit 2, before any device is touched.
refused() {
  local want="$1"; shift
  set +e
  run $BIN "$@" <&3 > "$out" 2> "$err"
  code=$?
  set -e
  [ "$code" -eq 2 ] || { echo "expected exit 2 for $*, got $code"; cat "$err"; exit 1; }
  one_error_frame "$want" > /dev/null
}
refused "--video-codec h264" --format framed --video-codec hevc --probe-secs 3
refused "--video-codec h264" --format framed --codec hevc --probe-secs 3
refused "--probe-raw is for the probe" --format framed --probe-raw /tmp/never-written.raw
refused "cannot write the probe's raw" --format framed --probe-secs 3 --probe-raw /nonexistent-dir/x.raw
refused "raw Annex-B" --format framed --record /tmp/never-written.mp4
refused "cannot write --record" --format framed --record /nonexistent-dir/x.h264
# And the probe itself fails like any start-up when there is no usbmuxd.
refused "usbmuxd" --format framed --probe-secs 3 --usbmuxd /nonexistent/usbmuxd

# The video is never written to a terminal unless --output asks for it: run with a pseudo terminal for stdout.
python3 -I - "$BIN" <<'PY'
import os, pty, subprocess, sys
master, slave = pty.openpty()
p = subprocess.run([sys.argv[1], '--usbmuxd', '/nonexistent/usbmuxd'], stdout=slave, stderr=subprocess.PIPE,
                   stdin=subprocess.DEVNULL, timeout=30)
assert p.returncode == 2, p.returncode
assert b'refusing to write the video to a terminal' in p.stderr, p.stderr
# Nothing reached the terminal, and the device was never looked for.
assert b'usbmuxd' not in p.stderr, p.stderr
print('ok: stdout is a terminal: refused')
PY

# A fake usbmuxd: answers ListDevices with a device that is attached over USB and over the network.
sock="$dir/usbmuxd"
python3 -I - "$sock" <<'PY' &
import os, plistlib, socket, struct, sys, threading
path = sys.argv[1]
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(path)
srv.listen(8)
udid = '00008110-001234567890401E'
addr = bytes([0x10, 0x02, 0, 0, 192, 168, 1, 5]) + bytes(8)
devices = {'DeviceList': [
    {'DeviceID': 1, 'MessageType': 'Attached',
     'Properties': {'ConnectionType': 'USB', 'DeviceID': 1, 'SerialNumber': udid}},
    {'DeviceID': 2, 'MessageType': 'Attached',
     'Properties': {'ConnectionType': 'Network', 'DeviceID': 2, 'SerialNumber': udid, 'NetworkAddress': addr}},
]}
def serve(c):
    with c:
        while True:
            head = c.recv(16, socket.MSG_WAITALL)
            if len(head) < 16:
                return
            size, _version, _message, tag = struct.unpack('<IIII', head)
            c.recv(size - 16, socket.MSG_WAITALL)
            body = plistlib.dumps(devices)
            c.sendall(struct.pack('<IIII', 16 + len(body), 1, 8, tag) + body)
while True:
    conn, _ = srv.accept()
    threading.Thread(target=serve, args=(conn,), daemon=True).start()
PY
mux_pid=$!
for _ in $(seq 1 100); do [ -S "$sock" ] && break; sleep 0.05; done
[ -S "$sock" ] || { echo "the fake usbmuxd did not start"; exit 1; }

want="UDID                       CONNECTION
00008110-001234567890401E  network 192.168.1.5
00008110-001234567890401E  usb"
for args in "list --usbmuxd $sock" "--list-devices --usbmuxd $sock"; do
  got="$(run $BIN $args 2> "$err")" || { echo "scrios $args failed"; cat "$err"; exit 1; }
  [ "$got" = "$want" ] || { echo "scrios $args printed:"; echo "$got"; exit 1; }
done
echo "ok: scrios list shows the fake usbmuxd's devices"
# USBMUXD_SOCKET_ADDRESS is honoured as it is for the stream.
got="$(USBMUXD_SOCKET_ADDRESS=$sock run $BIN list 2> "$err")" || { cat "$err"; exit 1; }
[ "$got" = "$want" ] || { echo "list with USBMUXD_SOCKET_ADDRESS printed:"; echo "$got"; exit 1; }

# A UDID that usbmuxd does not list is a start-up failure that names what it lists (framed: as the error event).
set +e
run $BIN --format framed --usbmuxd "$sock" --udid NOSUCHDEVICE <&3 > "$out" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] || { echo "expected exit 2, got $code"; cat "$err"; exit 1; }
one_error_frame "no device with UDID NOSUCHDEVICE (usbmuxd lists: 00008110-001234567890401E" > /dev/null
echo "smoke test passed"
