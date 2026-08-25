use crate::fatx_structs::{FatxRawDirectoryEntry, FatxSuperblock, FATX_SUPERBLOCK_SIZE, FATX_END_OF_DIR_MARKER, FATX_END_OF_DIR_MARKER2, FATX_DELETED_FILE_MARKER};
use byteorder::{BigEndian, ReadBytesExt};
use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

#[cfg(target_os = "linux")]
const BLKGETSIZE64: libc::c_ulong = 2148012658; // 0x80081272

#[allow(dead_code)]
pub struct FatxFileSystem {
    pub device: File,
    pub superblock: FatxSuperblock,
    pub partition_size: u64,
    pub num_clusters: u32,
    pub is_fat32: bool,
    pub fat_offset: u64,
    pub cluster_offset: u64,
    pub cluster_size: u64,
    pub fat_data: Vec<u8>,
}

impl FatxFileSystem {
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let mut device = File::open(&path)?;
        
        let metadata = device.metadata()?;
        let partition_size = if metadata.file_type().is_block_device() {
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::io::AsRawFd;
                let mut size: u64 = 0;
                unsafe {
                    if libc::ioctl(device.as_raw_fd(), BLKGETSIZE64, &mut size) == 0 {
                        size
                    } else {
                        return Err(io::Error::last_os_error());
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                0
            }
        } else {
            metadata.len()
        };

        let mut sb_data = [0u8; FATX_SUPERBLOCK_SIZE];
        device.seek(SeekFrom::Start(0))?;
        device.read_exact(&mut sb_data)?;

        let superblock = FatxSuperblock::read_from(&sb_data)?;

        let cluster_size = (superblock.sectors_per_cluster as u64) * 512;
        let num_clusters = (partition_size / cluster_size) as u32;
        
        let is_fat32 = num_clusters > 65524;
        
        let fat_offset = 4096;
        let fat_size_bytes = (num_clusters as u64 + 1) * (if is_fat32 { 4 } else { 2 });
        let fat_size_aligned = (fat_size_bytes + 4095) / 4096 * 4096;
        let cluster_offset = fat_offset + fat_size_aligned;

        // Load the entire FAT into memory for blazing fast reads
        let mut fat_data = vec![0u8; fat_size_aligned as usize];
        device.seek(SeekFrom::Start(fat_offset))?;
        device.read_exact(&mut fat_data)?;

        Ok(Self {
            device,
            superblock,
            partition_size,
            num_clusters,
            is_fat32,
            fat_offset,
            cluster_offset,
            cluster_size,
            fat_data,
        })
    }

    pub fn read_cluster(&mut self, cluster: u32) -> io::Result<Vec<u8>> {
        if cluster == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Cluster 0 is invalid"));
        }
        let offset = self.cluster_offset + ((cluster as u64 - 1) * self.cluster_size);
        self.device.seek(SeekFrom::Start(offset))?;
        let mut data = vec![0u8; self.cluster_size as usize];
        self.device.read_exact(&mut data)?;
        Ok(data)
    }

    pub fn get_next_cluster(&self, cluster: u32) -> io::Result<Option<u32>> {
        let index = cluster as usize * (if self.is_fat32 { 4 } else { 2 });
        if index + 4 > self.fat_data.len() {
            return Ok(None);
        }

        let mut cursor = Cursor::new(&self.fat_data[index..index + 4]);
        let next_cluster = if self.is_fat32 {
            cursor.read_u32::<BigEndian>()?
        } else {
            cursor.read_u16::<BigEndian>()? as u32
        };

        if (self.is_fat32 && next_cluster >= 0xFFFFFFF8) || (self.is_fat32 == false && next_cluster >= 0xFFF8) {
            return Ok(None);
        }
        if next_cluster == 0 {
            return Ok(None);
        }

        Ok(Some(next_cluster))
    }

    pub fn read_file_chunk(&mut self, start_cluster: u32, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        let mut current_cluster = start_cluster;
        let mut bytes_to_skip = offset;
        
        while bytes_to_skip >= self.cluster_size {
            bytes_to_skip -= self.cluster_size;
            match self.get_next_cluster(current_cluster)? {
                Some(next) => current_cluster = next,
                None => return Ok(Vec::new()),
            }
        }
        
        let mut data = Vec::with_capacity(size as usize);
        let mut remaining_size = size as u64;
        
        while remaining_size > 0 {
            let cluster_data = self.read_cluster(current_cluster)?;
            
            let start = bytes_to_skip as usize;
            let available_in_cluster = (self.cluster_size as usize) - start;
            let to_read = std::cmp::min(remaining_size as usize, available_in_cluster);
            
            data.extend_from_slice(&cluster_data[start..start + to_read]);
            
            remaining_size -= to_read as u64;
            bytes_to_skip = 0; 
            
            if remaining_size > 0 {
                match self.get_next_cluster(current_cluster)? {
                    Some(next) => current_cluster = next,
                    None => break,
                }
            }
        }
        
        Ok(data)
    }

    pub fn read_chain(&mut self, start_cluster: u32) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        let mut current_cluster = start_cluster;
        
        loop {
            let mut cluster_data = self.read_cluster(current_cluster)?;
            data.append(&mut cluster_data);
            
            match self.get_next_cluster(current_cluster)? {
                Some(next) => current_cluster = next,
                None => break,
            }
        }
        Ok(data)
    }

    pub fn read_directory(&mut self, start_cluster: u32) -> io::Result<Vec<FatxRawDirectoryEntry>> {
        let data = self.read_chain(start_cluster)?;
        let mut entries = Vec::new();
        
        let mut i = 0;
        while i + 64 <= data.len() {
            let chunk = &data[i..i+64];
            let filename_len = chunk[0];
            
            if filename_len == FATX_END_OF_DIR_MARKER || filename_len == FATX_END_OF_DIR_MARKER2 {
                break;
            }
            if filename_len == FATX_DELETED_FILE_MARKER || filename_len == 0 {
                i += 64;
                continue;
            }
            
            let mut entry_data = [0u8; 64];
            entry_data.copy_from_slice(chunk);
            if let Ok(entry) = FatxRawDirectoryEntry::read_from(&entry_data) {
                entries.push(entry);
            }
            i += 64;
        }
        Ok(entries)
    }
}
