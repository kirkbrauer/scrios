#!/usr/bin/env bash
# Smoke test of a release binary without a device:
#   * --help and --version work;
#   * `scrios list` and --list-devices read a fake usbmuxd, on a unix socket (a path or `UNIX:path`) and on TCP
#     (`host:port` or `TCP:host:port`), and --usbmuxd wins over $USBMUXD_SOCKET_ADDRESS;
#   * a start-up failure (no usbmuxd at the given path, or no such UDID) in the framed format writes exactly one
#     `error` control frame to stdout, logs to stderr, and exits 2; in the raw format stdout stays empty;
#   * the device commands (`ddi-status`, `mount-ddi`, `unmount-ddi`, `developer-mode`, `doctor`) say what is missing
#     (a device with no pair record in usbmuxd is "not trusted"), and a DDI download without its SHA-256 is refused;
#   * a plain `scrios` waits for what is missing and says so (once), and a SIGTERM while it waits is a clean stop;
#     with --no-wait it fails at once with the same words;
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
# A standard input that never ends: in the framed format end of input is a `stop`, which would race with a start-up
# failure.
exec 3< <(sleep 600)
sleeper=${!:-}
mux_pid=
waiter=
cleanup() {
  rm -rf "$out" "$err" "$dir"
  [ -z "$sleeper" ] || kill "$sleeper" 2>/dev/null || true
  [ -z "$mux_pid" ] || kill "$mux_pid" 2>/dev/null || true
  [ -z "$waiter" ] || kill "$waiter" 2>/dev/null || true
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
run $BIN stream --format framed --usbmuxd /nonexistent/usbmuxd --stats-interval-ms 0 <&3 > "$out" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] || { echo "expected exit 2, got $code"; cat "$err"; exit 1; }
one_error_frame usbmuxd
grep -q "cannot connect to usbmuxd" "$err" || { echo "no log on stderr"; cat "$err"; exit 1; }

# The same failure in the default (raw) format: nothing on stdout, the same log and status.
set +e
run $BIN stream --usbmuxd /nonexistent/usbmuxd <&3 > "$out" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] || { echo "expected exit 2, got $code"; cat "$err"; exit 1; }
[ ! -s "$out" ] || { echo "the raw format wrote something on a failed start"; exit 1; }
grep -q "cannot connect to usbmuxd" "$err" || { echo "no log on stderr"; cat "$err"; exit 1; }
echo "ok: a failed start writes nothing in the raw format"

# The old spellings still parse: --hid and --codec h264 get as far as the missing usbmuxd.
set +e
run $BIN stream --hid --codec h264 --usbmuxd /nonexistent/usbmuxd <&3 > "$out" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] && grep -q "cannot connect to usbmuxd" "$err" || { echo "the old flags are refused"; cat "$err"; exit 1; }

# Without a subcommand a build with the viewer opens a window and a build without it streams; with an option that only
# a written stream means (--format), the first refuses and the second streams: both fail to start here, with exit 2 and
# no window and no video on stdout (this runs where there is no display).
set +e
run $BIN --format framed --usbmuxd /nonexistent/usbmuxd <&3 > "$out" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] || { echo "expected exit 2 for scrios --format framed, got $code"; cat "$err"; exit 1; }
echo "ok: scrios without a subcommand, with a stream's option, fails to start without a window"

# Options that do not go together are refused with an `error` event (framed) and exit 2, before any device is touched.
refused() {
  local want="$1"; shift
  set +e
  run $BIN stream "$@" <&3 > "$out" 2> "$err"
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
p = subprocess.run([sys.argv[1], 'stream', '--usbmuxd', '/nonexistent/usbmuxd'], stdout=slave, stderr=subprocess.PIPE,
                   stdin=subprocess.DEVNULL, timeout=30)
assert p.returncode == 2, p.returncode
assert b'refusing to write the video to a terminal' in p.stderr, p.stderr
# Nothing reached the terminal, and the device was never looked for.
assert b'usbmuxd' not in p.stderr, p.stderr
print('ok: stdout is a terminal: refused')
PY

# A fake usbmuxd: answers ListDevices with a device that is attached over USB and over the network. It listens on a
# unix socket and on a TCP port of 127.0.0.1 (the port is written to "$sock.port" before the socket appears).
sock="$dir/usbmuxd"
python3 -I - "$sock" <<'PY' &
import os, plistlib, socket, struct, sys, threading
path = sys.argv[1]
tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
tcp.bind(('127.0.0.1', 0))
tcp.listen(8)
open(path + '.port', 'w').write(str(tcp.getsockname()[1]))
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
def accept(server):
    while True:
        conn, _ = server.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()
threading.Thread(target=accept, args=(tcp,), daemon=True).start()
accept(srv)
PY
mux_pid=$!
for _ in $(seq 1 100); do [ -S "$sock" ] && break; sleep 0.05; done
[ -S "$sock" ] || { echo "the fake usbmuxd did not start"; exit 1; }

want="UDID                       CONNECTION
00008110-001234567890401E  network 192.168.1.5
00008110-001234567890401E  usb"
port="$(cat "$sock.port")"
for args in "list --usbmuxd $sock" "--list-devices --usbmuxd $sock" "list --usbmuxd UNIX:$sock" \
    "list --usbmuxd 127.0.0.1:$port" "list --usbmuxd TCP:127.0.0.1:$port"; do
  got="$(run $BIN $args 2> "$err")" || { echo "scrios $args failed"; cat "$err"; exit 1; }
  [ "$got" = "$want" ] || { echo "scrios $args printed:"; echo "$got"; exit 1; }
done
echo "ok: scrios list shows the fake usbmuxd's devices (unix socket, UNIX:, host:port, TCP:)"
# USBMUXD_SOCKET_ADDRESS is honoured as it is for the stream, in both of its forms, and --usbmuxd wins over it.
for env in "$sock" "UNIX:$sock" "127.0.0.1:$port"; do
  got="$(USBMUXD_SOCKET_ADDRESS=$env run $BIN list 2> "$err")" || { cat "$err"; exit 1; }
  [ "$got" = "$want" ] || { echo "list with USBMUXD_SOCKET_ADDRESS=$env printed:"; echo "$got"; exit 1; }
done
got="$(USBMUXD_SOCKET_ADDRESS=/nonexistent/usbmuxd run $BIN list --usbmuxd "$sock" 2> "$err")" || { cat "$err"; exit 1; }
[ "$got" = "$want" ] || { echo "--usbmuxd did not win over USBMUXD_SOCKET_ADDRESS"; exit 1; }
echo "ok: USBMUXD_SOCKET_ADDRESS is honoured, and --usbmuxd wins over it"

# A UDID that usbmuxd does not list is a start-up failure that names what it lists (framed: as the error event).
set +e
run $BIN stream --format framed --usbmuxd "$sock" --udid NOSUCHDEVICE <&3 > "$out" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] || { echo "expected exit 2, got $code"; cat "$err"; exit 1; }
one_error_frame "no device with UDID NOSUCHDEVICE (usbmuxd lists: 00008110-001234567890401E" > /dev/null

# The device commands: the fake usbmuxd lists a device but holds no pair record for it, so it does not trust us.
for cmd in ddi-status mount-ddi unmount-ddi; do
  set +e
  run $BIN $cmd --usbmuxd "$sock" 2> "$err"
  code=$?
  set -e
  [ "$code" -eq 2 ] && grep -q "tap Trust" "$err" || { echo "scrios $cmd: expected exit 2 and the Trust hint, got $code"; cat "$err"; exit 1; }
done
echo "ok: the device commands say that the device does not trust this computer"
set +e
run $BIN mount-ddi --usbmuxd "$sock" --ddi-download-url https://example.com/ddi.tar.gz 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] && grep -q "SHA-256" "$err" || { echo "a download without a hash was not refused"; cat "$err"; exit 1; }
echo "ok: a DDI download without its SHA-256 is refused"

# The stream's own preflight: a device that is listed but not trusted. With --no-wait it fails at once (exit 2) ...
set +e
run $BIN stream --no-wait --usbmuxd "$sock" --record "$dir/never.h264" < /dev/null 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] && grep -q "tap Trust" "$err" || { echo "--no-wait: expected exit 2 and the Trust hint, got $code"; cat "$err"; exit 1; }
[ ! -e "$dir/never.h264" ] || [ ! -s "$dir/never.h264" ] || { echo "a recording was written"; exit 1; }
# The same for `scrios` without a subcommand, which is the window in a build with the viewer: the set-up runs first, so
# the device that is not trusted is refused at once, before any window, whatever display there is (none, here).
set +e
run $BIN --no-wait --usbmuxd "$sock" < /dev/null > "$out" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] && grep -q "tap Trust" "$err" || { echo "scrios --no-wait: expected exit 2 and the Trust hint, got $code"; cat "$err"; exit 1; }
# ... and without it, it waits, says what it waits for once, and a SIGTERM is a clean stop.
"$BIN" stream --usbmuxd "$sock" --record "$dir/waits.h264" < /dev/null 2> "$err" &
waiter=$!
sleep 2.5
kill -0 "$waiter" 2>/dev/null || { echo "scrios stopped instead of waiting for Trust"; cat "$err"; exit 1; }
[ "$(grep -c "Unlock the device and tap Trust" "$err")" -eq 1 ] || { echo "the wait was not said exactly once"; cat "$err"; exit 1; }
kill -TERM "$waiter"
set +e
wait "$waiter"
code=$?
set -e
waiter=
[ "$code" -eq 0 ] || { echo "a SIGTERM while waiting should be a clean stop, got $code"; cat "$err"; exit 1; }
echo "ok: scrios waits for Trust, says so once, and stops cleanly on SIGTERM"
# --no-preflight skips the checks: the stream starts, and the fake usbmuxd has no tunnel to give it. Standard input is
# closed, as under nohup or in a script: that is not a stop, so the start fails for its own reason (exit 2), not with
# "stop requested while starting" (exit 0), which is what the end of input used to do.
set +e
run $BIN stream --no-preflight --no-control --usbmuxd "$sock" --record "$dir/nopre.h264" < /dev/null 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] && ! grep -q "tap Trust" "$err" || { echo "--no-preflight still made the checks (exit $code)"; cat "$err"; exit 1; }
! grep -q "stop requested" "$err" || { echo "the end of stdin stopped a raw stream"; cat "$err"; exit 1; }
echo "ok: --no-preflight does not make the checks, and a closed stdin does not stop a raw stream"

# doctor: every check, with its fix; exit 1 if one fails.
set +e
got="$(run $BIN doctor --usbmuxd "$sock" 2> "$err")"
code=$?
set -e
[ "$code" -eq 1 ] || { echo "doctor: expected exit 1, got $code"; echo "$got"; exit 1; }
echo "$got" | grep -q "^ok    usbmuxd" && echo "$got" | grep -q "^ok    device" && echo "$got" | grep -q "^FAIL  pairing" \
  && echo "$got" | grep -q "fix: .*tap Trust" || { echo "doctor printed:"; echo "$got"; exit 1; }
set +e
got="$(run $BIN doctor --usbmuxd /nonexistent/usbmuxd 2> "$err")"
code=$?
set -e
[ "$code" -eq 1 ] && echo "$got" | grep -q "^FAIL  usbmuxd" && echo "$got" | grep -q "^skip  device" || { echo "doctor without usbmuxd printed:"; echo "$got"; exit 1; }
echo "ok: scrios doctor reports each check with its fix"

# Developer Mode is never turned on without --enable and --yes.
set +e
run $BIN developer-mode --enable --usbmuxd "$sock" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] && grep -q -- "--yes" "$err" || { echo "--enable without --yes was not refused"; cat "$err"; exit 1; }
set +e
run $BIN developer-mode --yes --usbmuxd "$sock" 2> "$err"
code=$?
set -e
[ "$code" -eq 2 ] || { echo "--yes alone should be a usage error, got $code"; exit 1; }
echo "ok: developer-mode --enable needs --yes"
echo "smoke test passed"
