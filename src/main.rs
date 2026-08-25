mod fatx_structs;
mod filesystem;
mod fuse_driver;

use fuser::MountOption;
use std::env;

fn main() {
    env_logger::init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <device> <mountpoint>", args[0]);
        std::process::exit(1);
    }

    let device_path = &args[1];
    let mountpoint = &args[2];

    println!("Initializing FATX360FS driver on {}", device_path);
    let fs = match filesystem::FatxFileSystem::new(device_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to open filesystem: {}", e);
            std::process::exit(1);
        }
    };

    println!("FATX360 Superblock detected!");
    println!(" - Sectors per cluster: {}", fs.superblock.sectors_per_cluster);
    println!(" - Root cluster: {}", fs.superblock.root_cluster);
    println!(" - FAT Type: {}", if fs.is_fat32 { "FAT32" } else { "FAT16" });

    let driver = fuse_driver::FatxDriver::new(fs);
    
    let options = vec![
        MountOption::RO, 
        MountOption::FSName("fatx360fs".to_string()), 
    ];
    
    println!("Mounting to {}...", mountpoint);
    let mut config = fuser::Config::default();
    config.mount_options = options;
    fuser::mount(driver, mountpoint, &config).unwrap();
}
