# Building

scrios builds with a plain Rust toolchain (1.89 or newer, edition 2024) and a C compiler: `ring`, the TLS backend
scrios selects in `idevice`, compiles a little C and assembly with `cc`. No CMake, NASM or bindgen is needed, on Linux,
macOS or Windows.

```
cargo build --release --locked     # -> target/release/scrios, with the viewer window
cargo test --locked                # unit tests: no device, no network at run time
```

## The viewer feature

The cargo feature `viewer` (on by default) is the window: winit and wgpu, the VideoToolbox decoder on macOS, and
OpenH264, which is **built from source** by the `openh264-sys2` crate and so needs a **C++ compiler** as well (`c++` or
`clang++` on Linux and macOS, Visual C++ on Windows). A `--no-default-features` build has none of that: no window, no GPU
or window-system code, no decoder, and a binary about half the size; `scrios` without a subcommand is then `scrios stream`.

```
cargo build --release --locked --no-default-features
cargo test --no-default-features --locked
```

On Linux the viewer is built to need no window-system or GPU library at build time: winit (X11, Wayland), wgpu (Vulkan,
OpenGL) and the key-map library load theirs when the program runs, so the window needs a Wayland or X11 session and a
Vulkan or OpenGL driver. (This is how their features are set; the viewer has been built and run on macOS only so far.)
The tests that draw to a texture use whichever adapter wgpu finds, and say so and pass if there is none.

## Static Linux binaries (musl)

The release binaries are static musl builds, **without the viewer** (`--no-default-features`: a static binary has no use
for a window system), cross-compiled with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)
(zig as the C compiler and linker), from Linux or macOS:

```
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo zigbuild --release --locked --no-default-features --target x86_64-unknown-linux-musl
cargo zigbuild --release --locked --no-default-features --target aarch64-unknown-linux-musl
```

```
$ file target/aarch64-unknown-linux-musl/release/scrios
ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked, stripped
```

About 6.5 MB (aarch64) and 8 MB (x86_64). A glibc build (`--target <arch>-unknown-linux-gnu`, or a native
`cargo build --no-default-features` on Linux) works too and is dynamically linked against glibc only.

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
`rsd` and `xpc`, and for the device set-up `mobile_image_mounter` (mounting the developer disk image on iOS 17.0 to
17.3.1), `cryptexd` (installing it as a cryptex, what Xcode does on iOS 17.4 and later; it adds no crate: it is `xpc`, `rsd`
and `tss`), `tss` (the
request to Apple's signing server that mounting makes; it brings idevice's small HTTP client, `webpki-roots` for https
downloads of a DDI archive, and its CDLA-Permissive-2.0 licence, which `deny.toml` allows for that crate only) and
`amfi` (revealing and, on request, turning on Developer Mode).

## Releases

A tag `vX.Y.Z` (it must equal `version` in `Cargo.toml`) runs `.github/workflows/release.yml`: the static musl
binaries, the macOS one and the Windows zip (`scrios-x86_64-pc-windows-msvc.zip`, and an arm64 one if its cross-build
works), `SHA256SUMS`, a GitHub release, and `cargo publish` to crates.io.
