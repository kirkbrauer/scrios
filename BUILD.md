# Building

You need [Rust](https://www.rust-lang.org/tools/install) 1.89 or newer, a C compiler, and a C++ compiler for the window.

```
cargo build --release --locked
```

The binary is `target/release/scrios`.

## Without the window

```
cargo build --release --locked --no-default-features
```

This builds only `scrios stream` and the device commands. It needs no C++ compiler, no GPU and no window system, and
the binary is about half the size.

## Static Linux binaries

The release binaries are static musl builds without the window, cross-compiled with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild):

```
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo zigbuild --release --locked --no-default-features --target x86_64-unknown-linux-musl
cargo zigbuild --release --locked --no-default-features --target aarch64-unknown-linux-musl
```

## Windows

Use the MSVC toolchain: [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with the C++ workload.

```
cargo build --release --locked
```

## Development

```
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
cargo test --no-default-features --locked
cargo deny check
```

`cargo deny` is [cargo-deny](https://github.com/EmbarkStudios/cargo-deny). The tests need no device and no network. CI runs them on Linux, macOS and Windows.

`scripts/smoke.sh target/release/scrios` runs a release binary through its commands without a device.

## Updating idevice

[idevice](https://github.com/jkcoxson/idevice) is pinned to an exact version because its API changes between
releases. To update it:

1. Change the version in `Cargo.toml`.
2. Change the "Built against" line in `README.md`. `scripts/check-pins.sh` checks that they match.
3. Run the tests, then try it on a real device.

## Updating the developer disk image

scrios downloads the image from one pinned commit of
[doronz88/DeveloperDiskImage](https://github.com/doronz88/DeveloperDiskImage). The commit and the checksum of every
file are in `src/ddi/pin.rs`. Update them when a newer iOS needs a newer image. The steps are in the comment at the
top of that file.

A weekly workflow, `.github/workflows/ddi-pin.yml`, checks that the pinned files still download and reports when the
repository has a newer image.

## Releases

Pushing a tag `vX.Y.Z` that matches the version in `Cargo.toml` builds the binaries for Linux, macOS and Windows,
creates a GitHub release, and publishes the crate.
