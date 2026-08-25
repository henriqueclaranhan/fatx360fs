# fatx360fs

A FUSE driver written in Rust for mounting the FATX filesystem (used by Xbox 360). This driver allows you to mount FATX partitions on Linux.

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
To mount a FATX block device (like a USB drive or HDD) to a destination directory:
```bash
fatx360fs /dev/sdX1 /mnt/fatx
```

#### Command Line Options

You can view all available options by passing `-h` or `--help`:
```bash
fatx360fs --help
```

**Options:**

| Option / Argument | Description | Default |
| :--- | :--- | :--- |
| `<DEVICE>` | Device to mount (e.g., `/dev/sdX1`) | *Required* |
| `<MOUNTPOINT>` | Mountpoint directory (e.g., `/mnt/fatx`) | *Required* |
| `-r, --readonly` | Mount the filesystem in read-only mode | `false` (read-write) |
| `-t, --threads <THREADS>`| Number of threads for the FUSE driver | `16` |
| `-h, --help` | Print help information | - |
| `-V, --version` | Print version information | - |
