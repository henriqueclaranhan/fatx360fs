mod fatx_structs;
mod filesystem;
mod fuse_driver;

use fuser::MountOption;
use std::env;

fn main() {
    env_logger::init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <device> <mountpoint> [-r|--readonly]", args[0]);
        std::process::exit(1);
    }

    let device_path = &args[1];
    let mountpoint = &args[2];

    let readonly = args.iter().any(|a| a == "-r" || a == "--readonly");

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
    config.n_threads = Some(16);
    fuser::mount(driver, mountpoint, &config).unwrap();
}
