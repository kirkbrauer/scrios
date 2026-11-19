# Building

scrios builds with a plain Rust toolchain (1.89 or newer, edition 2024) and a C compiler: `ring`, the TLS backend
scrios selects in `idevice`, compiles a little C and assembly with `cc`. No CMake, NASM or bindgen is needed, on Linux,
macOS or Windows.

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

## Windows

`cargo build --release --locked` with the MSVC toolchain (Visual Studio Build Tools with the C++ workload, and
`rustup default stable-x86_64-pc-windows-msvc`) builds `target\release\scrios.exe`; that is what CI does on
`windows-latest`, and the release zip is built the same way with `--target x86_64-pc-windows-msvc`. `ring` ships the
assembly it needs for Windows as object files, so NASM is not needed. An arm64 build is a cross-build from an x64
host: `rustup target add aarch64-pc-windows-msvc`, then `cargo build --release --locked --target aarch64-pc-windows-msvc`
(with the MSVC ARM64 build tools; the release workflow tries it and goes on without it if it fails).

The sources can be checked for Windows from another host: `rustup target add x86_64-pc-windows-gnu`, then
`cargo check --target x86_64-pc-windows-gnu --all-targets --locked` with a C compiler for the target in
`CC_x86_64_pc_windows_gnu` (for `ring`'s build script; `zig cc -target x86_64-windows-gnu` does). That checks that it
compiles and passes clippy; it does not run anything.

## Mirrors and offline builds

`Cargo.lock` names crates.io only; the repository has no Cargo configuration. To build through a mirror of
crates.io, configure it in `$CARGO_HOME/config.toml` (`[source.crates-io] replace-with = ...`); `--locked` still
works, since source replacement does not change the lock. A CA bundle for a TLS-intercepting proxy goes in
`CARGO_HTTP_CAINFO`. For a fully offline tree, `cargo vendor`.

### `idevice`

`idevice` (the CoreDevice, RemoteXPC, usbmuxd and lockdown client) is pinned **exactly**, `version = "=0.1.68"`,
because its API changes between releases. A bump is deliberate: change it in `Cargo.toml` and in the README's
"Compatibility" line, run the tests, and smoke-test against a real device. Only the features scrios uses are on:
`ring` (rustls on ring: TLS for the lockdown session; idevice's default, aws-lc, needs CMake/NASM on Windows), `display_stream` (the display service client, offers, RTP/RTCP helpers),
`core_device_proxy` and `tunnel_tcp_stack` (the CoreDevice tunnel with its userspace TCP/UDP stack), `usbmuxd`,
`rsd` and `xpc`, and for the device set-up `mobile_image_mounter` (mounting the developer disk image), `tss` (the
request to Apple's signing server that mounting makes; it brings idevice's small HTTP client, `webpki-roots` for https
downloads of a DDI archive, and its CDLA-Permissive-2.0 licence, which `deny.toml` allows for that crate only) and
`amfi` (revealing and, on request, turning on Developer Mode).

## Releases

A tag `vX.Y.Z` (it must equal `version` in `Cargo.toml`) runs `.github/workflows/release.yml`: the static musl
binaries, the macOS one and the Windows zip (`scrios-x86_64-pc-windows-msvc.zip`, and an arm64 one if its cross-build
works), `SHA256SUMS`, a GitHub release, and `cargo publish` to crates.io.
