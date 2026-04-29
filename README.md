# v4l2top

An `htop`-like utility to show v4l2 mem2mem hardware and memory usage.

## Dependency

Linux needs to be patched to be able to retrieve data from the driver.
A work in progress branch can be found [here](https://gitlab.collabora.com/detlev/linux/-/tree/rkvdec/list-hantro-buffers).
It only supports the rkvdec driver, and only h264 on RK3588 for buffer details.

## Installation

The crate is not published yet, use the git url to install locally with cargo:
```bash
cargo install https://github.com/cazou/v4l2top.git
```

### Cross compiling

To build an arm64 binary, install an arm64 Rust toolchain with rustup:
```bash
rustup target add aarch64-unknown-linux-gnu
```

And that you have an arm64 linker.
Debian:
```bash
apt install gcc-aarch64-linux-gnu
```
Arch Linux:
```bash
pacman -S aarch64-linux-gnu-gcc
```

Other linkers can be used, but adapt the `.cargo/config.toml` file.

Then you can cross compile for arm64 with:
```bash
cargo build --target aarch64-unknown-linux-gnu
```

Add `--release` for a smaller binary.
You can retrieve it in `target/aarch64-unknown-linux-gnu/[release|debug]/v4l2top`
