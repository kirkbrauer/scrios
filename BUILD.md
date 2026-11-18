# Building

scrios builds with a plain Rust toolchain (1.87 or newer, edition 2024) and a C compiler: `aws-lc-sys`, the TLS
backend `idevice` uses, compiles its C sources with `cc`. CMake and bindgen are not needed for the targets below.

```
cargo build --release --locked     # -> target/release/scrios
cargo test --locked                # unit tests: no device, no network at run time
```

## Static Linux binaries (musl)

The release binaries are static musl builds, cross-compiled with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)
(zig as the C compiler and linker), from Linux or macOS:

```
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo zigbuild --release --locked --target x86_64-unknown-linux-musl
cargo zigbuild --release --locked --target aarch64-unknown-linux-musl
```

```
$ file target/aarch64-unknown-linux-musl/release/scrios
ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked, stripped
```

About 6.5 MB (aarch64) and 8 MB (x86_64). A glibc build (`--target <arch>-unknown-linux-gnu`, or a native
`cargo build` on Linux) works too and is dynamically linked against glibc only.

## Mirrors and offline builds

`Cargo.lock` names crates.io only; the repository has no Cargo configuration. To build through a mirror of
crates.io, configure it in `$CARGO_HOME/config.toml` (`[source.crates-io] replace-with = ...`); `--locked` still
works, since source replacement does not change the lock. A CA bundle for a TLS-intercepting proxy goes in
`CARGO_HTTP_CAINFO`. For a fully offline tree, `cargo vendor`.

### `idevice`

`idevice` (the CoreDevice, RemoteXPC, usbmuxd and lockdown client) is pinned **exactly**, `version = "=0.1.68"`,
because its API changes between releases. A bump is deliberate: change it in `Cargo.toml` and in the README's
"Compatibility" line, run the tests, and smoke-test against a real device. Only the features scrios uses are on:
`aws-lc` (TLS for the lockdown session), `display_stream` (the display service client, offers, RTP/RTCP helpers),
`core_device_proxy` and `tunnel_tcp_stack` (the CoreDevice tunnel with its userspace TCP/UDP stack), `usbmuxd`,
`rsd` and `xpc`.

## Releases

A tag `vX.Y.Z` (it must equal `version` in `Cargo.toml`) runs `.github/workflows/release.yml`: the static musl
binaries and the macOS one, `SHA256SUMS`, a GitHub release, and `cargo publish` to crates.io.
