# Building and testing

The binary is built and tested in a container, so no Rust toolchain is needed on the host. The target
is an **aarch64 Linux** ELF (it runs on a Raspberry Pi 5 with Debian bookworm; an arm64 container on an
Apple-silicon Mac builds it natively).

```
./build.sh          # -> target/release/jumpstarter-ios-display
./test.sh           # cargo test, in the same container
./scripts/smoke.sh  # runs the built binary without a phone: --help, and a start-up failure
```

A first build fetches and compiles the dependencies (about 2.5 minutes for the debug build of
`./test.sh`, then about 1.5 minutes for the release build on an 8-core arm64 VM); later builds only
compile the crate (seconds). Run them in the background and wait; there is nothing to poll.

## What the scripts do

`build.sh` and `test.sh` call `scripts/in-container.sh <cargo args>`, which is, in effect:

```
podman run --rm \
  -v "$PWD:/work" \
  -e CARGO_HOME=/work/.build/cargo-home \
  -e CARGO_TARGET_DIR=/work/target \
  -e CARGO_HTTP_CAINFO=/work/.build/ca-certificates.crt \
  -e SSL_CERT_FILE=/work/.build/ca-certificates.crt \
  -e GIT_SSL_CAINFO=/work/.build/ca-certificates.crt \
  -e CARGO_REGISTRIES_MIRROR_TOKEN \
  -w /work \
  docker.io/library/rust:1-bookworm \
  /work/scripts/container-cargo.sh build --release --locked
```

(`test.sh` runs `... test --locked` instead.) Before it starts the container the script copies
`ci/cargo-config.toml` to `.build/cargo-home/config.toml` and the CA bundle to `.build/ca-certificates.crt`.
`.build/` and `target/` are git-ignored; `CARGO_HOME` and the target directory live inside the mounted
project directory, so the registry cache and the compiled dependencies survive between runs.

| Variable | Meaning |
| --- | --- |
| `RUST_IMAGE` | container image (default `docker.io/library/rust:1-bookworm`) |
| `CA_BUNDLE` | CA bundle for the registry and GitHub proxy (default `~/ca-certificates.crt`) |
| `CARGO_REGISTRIES_MIRROR_TOKEN` | registry token; if unset, `CARGO_REGISTRIES_MIRROR_TOKEN` is used. Passed to the container **by name** (`-e NAME`), so the value is never on a command line or in a file |
| `IDEVICE_SRC` | optional: a local checkout of `idevice`, see below |

### Crates

Crates come from the mirror `remote-cargo` repository only: `ci/cargo-config.toml` replaces crates.io with
the sparse registry `https://mirror.example.com/api/cargo/crates-io/index/` (named `mirror`,
which is why the token variable is `CARGO_REGISTRIES_MIRROR_TOKEN`). It is the same configuration as the
earlier `idevice-tools` build. No credential is stored anywhere in the repository.

### `idevice`

`Cargo.toml` depends on `idevice` **by git rev**:

```toml
idevice = { git = "https://github.com/jkcoxson/idevice", rev = "2dca9a58a9a1dd02ec7d6213fcfd8fe861e74132", default-features = false, features = [
  "aws-lc", "display_stream", "core_device_proxy", "tunnel_tcp_stack", "usbmuxd", "rsd", "xpc",
] }
```

The container can fetch it from GitHub through the corporate proxy (`git ls-remote` and cargo's own
fetch both worked with `CARGO_HTTP_CAINFO` pointing at the CA bundle), so the default build needs no
override and `Cargo.lock` records the git source and commit. `--locked` is used whenever `Cargo.lock`
exists.

#### If the container cannot reach GitHub

Build against a local checkout of `idevice` at the pinned commit (`git -C <checkout> rev-parse HEAD`
must print `2dca9a58a9a1dd02ec7d6213fcfd8fe861e74132`):

```
IDEVICE_SRC=/path/to/idevice ./build.sh
IDEVICE_SRC=/path/to/idevice ./test.sh
```

`in-container.sh` mounts the checkout read-only at `/idevice-src`, and `container-cargo.sh` runs

```
cargo --config "patch.'https://github.com/jkcoxson/idevice'.idevice.path='/idevice-src/idevice'" build --release
```

The patch replaces the git source by the checkout's `idevice` crate. It changes how `Cargo.lock` names
`idevice`, so in this mode `--locked` is not passed and the script puts the committed `Cargo.lock` back
when cargo exits. (This path was exercised with `cargo check` against the earlier `idevice-tools` checkout,
which is at the same commit.) Nothing in the repository changes between the two modes.

### Features

`idevice` is built with `default-features = false` plus only what the sidecar uses: `aws-lc` (the TLS
backend of the lockdown session with the phone; the `idevice-tools` build used the same default),
`display_stream` (the display service client, offers, RTP/RTCP helpers), `core_device_proxy` and
`tunnel_tcp_stack` (the CoreDevice tunnel with its userspace TCP/UDP stack), `usbmuxd`, `rsd`, `xpc`.

## Output

```
$ file target/release/jumpstarter-ios-display
ELF 64-bit LSB pie executable, ARM aarch64, version 1 (SYSV), dynamically linked, interpreter /lib/ld-linux-aarch64.so.1, ... for GNU/Linux 3.7.0, stripped
```

About 6 MB, dynamically linked against glibc (Debian bookworm's is enough: the build image is bookworm).

## A native build on the host (optional)

The same sources build natively, for running the sidecar on a Mac against a device without a container
(crates.io directly, or through any mirror you configure; the lock file names crates.io):

```
CARGO_HOME=$PWD/.build/cargo-home-mac CARGO_TARGET_DIR=$PWD/target-mac cargo build --release --locked
```

`target-mac/` and `.build/` are git-ignored. This needs a Rust toolchain and a C compiler on the host (for `aws-lc`).

## Tests

`./test.sh` runs the unit tests (no phone, no network at run time): frame encoding, the RTP/HEVC
depacketizer (single NAL unit, aggregation, fragmentation, loss, resync, parameter-set insertion, malformed
input), the SPS parser and codec string, the keyframe rate limit and coalescing, command parsing (including the
`touch` and `key` commands) and end of stdin, RTCP packet bytes against the RFC layouts, the offer's protobuf fields,
the display info reply and the change detection of the `config` event, the touch mapping for all four rotations, the
touch controller (coalescing, lifting, limits), the engine and the session loop with a fake socket, a fake display
source, a fake HID service and a paused clock, the output queue and writer, device selection and answer parsing.

`./scripts/smoke.sh` additionally runs the release binary in the container and checks the start-up
failure path (one `error` frame on stdout, the log on stderr, exit status 2).

Not covered by any of this, because it needs a device: connecting through usbmuxd and the tunnel, the
device accepting the offer, the real RTCP behaviour, the contents of the device's answers, the `deviceinfo`
reply (the tests use its documented shape), and what the HID services do with the reports (above all, whether the
digitizer's axes are the panel's or the UI's: see `--hid-space`).
