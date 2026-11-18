#!/usr/bin/env bash
# A smoke test of a release binary without a device, for hosts where scripts/smoke.sh cannot run: it needs no Python,
# no unix socket and no signals, so it runs in Git Bash on Windows (where CI runs it) as well as on unix.
#   * --help and --version work;
#   * a start-up failure against a usbmuxd that is not there (a closed TCP port; --no-preflight skips the waiting) exits 2,
#     writes nothing to stdout in the raw format, and in the framed format writes exactly one `error` control frame
#     (which also proves that the process can take its own stdout, as the stream does);
#   * `list` and `doctor` say where usbmuxd was looked for, and `doctor` fails with a fix;
#   * options that do not go together are refused (exit 2);
#   * on Windows: there is no `UNIX:` address, and the help names 127.0.0.1:27015 as the default.
# It never contacts the usbmuxd of the host: every run is given an address, or none that gets as far as connecting.
#
#   scripts/smoke-minimal.sh BINARY   (for example target/release/scrios, or target/release/scrios.exe)
set -euo pipefail
BIN="${1:?usage: smoke-minimal.sh BINARY}"
[ -f "$BIN" ] || { echo "no binary at $BIN: build it first (cargo build --release)" >&2; exit 2; }
case "$(uname -s)" in MINGW* | MSYS* | CYGWIN*) windows=1 ;; *) windows=0 ;; esac

dir="$(mktemp -d)"
out="$dir/out"
err="$dir/err"
trap 'rm -rf "$dir"' EXIT
# An address nobody listens on. Port 1 is not used by anything that runs on a build machine.
CLOSED=127.0.0.1:1

fail() { echo "FAIL: $1" >&2; for f in "$out" "$err"; do [ ! -s "$f" ] || { echo "--- ${f##*/}:" >&2; cat -v "$f" >&2; }; done; exit 1; }
# run EXPECTED_STATUS ARGS...: runs the binary, stdout to $out and stderr to $err, and checks the exit status.
# Standard input is empty (and closed), as in a script.
run() {
  local want="$1"; shift
  local code=0
  "$BIN" "$@" < /dev/null > "$out" 2> "$err" || code=$?
  [ "$code" -eq "$want" ] || fail "scrios $* exited $code, expected $want"
}

run 0 --version
grep -q "^scrios " "$out" || fail "--version does not say scrios"
run 0 --help
run 0 -h
run 0 stream --help
run 0 list --help
run 0 doctor --help
run 0 mount-ddi --help
echo "ok: --version and --help"

# The help names this host's usbmuxd default.
run 0 --help
if [ "$windows" -eq 1 ]; then
  grep -q "127.0.0.1:27015" "$out" || fail "the help does not name 127.0.0.1:27015 as the default usbmuxd"
  ! grep -q "UNIX:" "$out" || fail "the help offers a UNIX: address on Windows"
  grep -q "Ctrl-C" "$out" || fail "the help does not name Ctrl-C"
else
  grep -q "/var/run/usbmuxd" "$out" || fail "the help does not name /var/run/usbmuxd as the default usbmuxd"
fi
echo "ok: the help names this host's usbmuxd default"

# (`scrios stream`: without a subcommand a build with the viewer opens a window, which a runner has no display for.)
# A start-up failure, in the raw format: nothing on stdout, the log on stderr, exit 2.
run 2 stream --usbmuxd "$CLOSED" --no-preflight
[ ! -s "$out" ] || fail "the raw format wrote something to stdout on a failed start"
grep -q "cannot connect to usbmuxd at $CLOSED" "$err" || fail "no 'cannot connect' on stderr"
echo "ok: a failed start writes nothing to stdout in the raw format"

# The same in the framed format: one `error` control frame (13 header bytes and a JSON payload) on stdout.
run 2 stream --format framed --no-stdin --usbmuxd "$CLOSED"
grep -a -q '"event":"error"' "$out" || fail "no error event on stdout"
[ "$(grep -a -o '"event"' "$out" | wc -l)" -eq 1 ] || fail "not exactly one event on stdout"
grep -a -q "cannot connect to usbmuxd" "$out" || fail "the error event does not say why"
grep -q "cannot connect to usbmuxd at $CLOSED" "$err" || fail "no 'cannot connect' on stderr"
echo "ok: a failed start in the framed format is one error frame on stdout"

# USBMUXD_SOCKET_ADDRESS is honoured when --usbmuxd is not given.
code=0
USBMUXD_SOCKET_ADDRESS="$CLOSED" "$BIN" list < /dev/null > "$out" 2> "$err" || code=$?
[ "$code" -eq 2 ] && grep -q "cannot connect to usbmuxd at $CLOSED" "$err" || fail "USBMUXD_SOCKET_ADDRESS was not honoured (exit $code)"
echo "ok: USBMUXD_SOCKET_ADDRESS is honoured"

# list and doctor.
run 2 list --usbmuxd "$CLOSED"
grep -q "cannot connect to usbmuxd at $CLOSED" "$err" || fail "list does not say where it looked"
run 1 doctor --usbmuxd "$CLOSED"
grep -q "^FAIL  usbmuxd" "$out" && grep -q "fix: " "$out" && grep -q "^skip  device" "$out" || fail "doctor printed something else"
echo "ok: list and doctor report an unreachable usbmuxd"

# Options that do not go together are refused before any device is touched.
run 2 stream --format framed --no-stdin --video-codec hevc --probe-secs 3
grep -a -q "\-\-video-codec h264" "$out" || fail "--probe-secs with hevc was not refused with an error event"
run 2 stream --format framed --no-stdin --record "$dir/never.mp4"
grep -a -q "raw Annex-B" "$out" || fail "--record to a container was not refused"
echo "ok: options that do not go together are refused"

if [ "$windows" -eq 1 ]; then
  # No unix sockets: a UNIX: address (or a bare path) is refused, and the message says what to give instead.
  run 2 list --usbmuxd UNIX:/var/run/usbmuxd
  grep -q "unix socket" "$err" || fail "a UNIX: address was not refused on Windows"
  echo "ok: no UNIX: addresses on Windows"
fi
echo "smoke test passed"
