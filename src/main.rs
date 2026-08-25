mod fatx_structs;
mod filesystem;
mod fuse_driver;

use clap::Parser;
use fuser::MountOption;

/// A FUSE driver for mounting the FATX filesystem (Xbox 360).
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Device to mount
    #[arg(required = true)]
    device: String,

    /// Mountpoint directory
    #[arg(required = true)]
    mountpoint: String,

    /// Mount the filesystem in read-only mode
    #[arg(short, long, default_value_t = false)]
    readonly: bool,

    /// Number of threads for the FUSE driver
    #[arg(short, long, default_value_t = 16)]
    threads: usize,
}

fn main() {
    env_logger::init();

    let args = Args::parse();

    let device_path = &args.device;
    let mountpoint = &args.mountpoint;
    let readonly = args.readonly;

    println!("Initializing FATX360FS driver on {}", device_path);
    let fs = match filesystem::FatxFileSystem::new(device_path, !readonly) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to open filesystem: {}", e);
            std::process::exit(1);
        }
    };

    println!("FATX360 Superblock detected!");
    println!(
        " - Sectors per cluster: {}",
        fs.superblock.sectors_per_cluster
    );
    println!(" - Root cluster: {}", fs.superblock.root_cluster);
    println!(
        " - FAT Type: {}",
        if fs.is_fat32 { "FAT32" } else { "FAT16" }
    );
    println!(
        " - Mode: {}",
        if readonly { "read-only" } else { "read-write" }
    );

    let driver = fuse_driver::FatxDriver::new(fs);

    let mut options = vec![MountOption::FSName("fatx360fs".to_string())];

    if readonly {
        options.push(MountOption::RO);
    }

    println!("Mounting to {}...", mountpoint);
    let mut config = fuser::Config::default();
    config.mount_options = options;
    config.n_threads = Some(args.threads);

    if let Err(e) = fuser::mount(driver, mountpoint, &config) {
        eprintln!("Failed to mount filesystem: {}", e);
        std::process::exit(1);
    }
}
