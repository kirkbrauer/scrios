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
request to Apple's signing server that mounting makes; it brings idevice's small HTTP client, `webpki-roots` for https,
and its CDLA-Permissive-2.0 licence, which `deny.toml` allows for that crate only; scrios's own download of the developer
disk image uses `webpki-roots` too, see [below](#the-developer-disk-image-pin)) and
`amfi` (revealing and, on request, turning on Developer Mode).

### The developer disk image pin

scrios downloads Apple's developer disk image, when the host has none, from **one commit** of the pymobiledevice3
project's [`DeveloperDiskImage`](https://github.com/doronz88/DeveloperDiskImage) repository, and uses a file only if its size
and SHA-256 are the ones in `src/ddi/pin.rs` (the commit, the Xcode build, and a table of the five files of
`PersonalizedImages/Xcode_iOS_DDI_Cryptex` and the three of `PersonalizedImages/Xcode_iOS_DDI_Personalized`). Updating it is
a deliberate change of that file, when a device on a newer iOS wants a newer image; the steps are in the comment at the
top of the file: find the commit (`gh api 'repos/doronz88/DeveloperDiskImage/commits?path=PersonalizedImages/Xcode_iOS_DDI_Cryptex&per_page=3'`),
download its files into an empty folder, check each against the repository's own blob hash (`git hash-object`), put the
commit, the build, and each file's size and `shasum -a 256` in the tables, then run `scrios ddi-fetch --variant all` with an
empty cache (`XDG_CACHE_HOME=$(mktemp -d)`). `.github/workflows/ddi-pin.yml` runs that every week, and says when the
repository has a newer commit than the pin.

The https client that downloads (`src/ddi/http.rs`) is scrios's own, on `rustls` (the `ring` backend, as idevice's) and the
Mozilla roots (`webpki-roots`): the three crates are in the tree already through idevice, at these features, and are
dependencies of scrios itself only so that the download can have limits that idevice's own HTTP client has not (https
only, a redirect only to the same host or GitHub's raw hosts, a size cap, timeouts).

The download also trusts what the host trusts: `rustls-native-certs` adds the host's trust store (the keychain on macOS,
the certificate store on Windows, the OpenSSL folders on Linux) and the files that `SSL_CERT_FILE` and `SSL_CERT_DIR`
name to the Mozilla roots, so a network whose proxy inspects TLS with the company's CA works once that CA is installed
on the host or named in `SSL_CERT_FILE`. It is the one crate the download adds (with `openssl-probe`, and
`security-framework` on macOS and `schannel` on Windows). An https archive (`--ddi-download-url`) is downloaded by the
same client.

## Releases

A tag `vX.Y.Z` (it must equal `version` in `Cargo.toml`) runs `.github/workflows/release.yml`: the static musl
binaries, the macOS one and the Windows zip (`scrios-x86_64-pc-windows-msvc.zip`, and an arm64 one if its cross-build
works), `SHA256SUMS`, a GitHub release, and `cargo publish` to crates.io.
