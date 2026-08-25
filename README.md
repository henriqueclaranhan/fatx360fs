# fatx360fs

A FUSE driver written in Rust for mounting the FATX filesystem (used by Xbox 360). This driver allows you to mount FATX partitions on Linux in read-only mode.

## Build and Usage

### Prerequisites
- Rust toolchain (Cargo)
- `libfuse` development headers (e.g., `libfuse-dev` or `fuse3` on Linux)

### Building & Installation
Compile the project in release mode:
```bash
cargo build --release
```

To install the driver system-wide:
```bash
sudo cp target/release/fatx360fs /usr/local/bin/
```

### Mounting
To mount a FATX block device or a raw image file to a destination directory:
```bash
fatx360fs /dev/sdX1 /mnt/fatx
```
