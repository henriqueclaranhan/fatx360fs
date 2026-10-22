use crate::fatx_structs::{
    FATX_DELETED_FILE_MARKER, FATX_END_OF_DIR_MARKER, FATX_END_OF_DIR_MARKER2,
    FATX_SUPERBLOCK_SIZE, FatxRawDirectoryEntry, FatxSuperblock,
};

use lru::LruCache;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
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
    pub next_free_cluster: u32,
    pub free_clusters: u32,
    pub cluster_cache: LruCache<u32, Vec<u8>>,
    pub cluster_chain_hints: LruCache<u32, (usize, u32)>,
}

impl FatxFileSystem {
    pub fn new<P: AsRef<Path>>(path: P, read_write: bool) -> io::Result<Self> {
        let mut device = if read_write {
            OpenOptions::new().read(true).write(true).open(&path)?
        } else {
            File::open(&path)?
        };

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

        let is_fat32 = num_clusters >= 0xFFF0;

        let fat_offset = 4096;
        let fat_size_bytes = (num_clusters as u64 + 1) * (if is_fat32 { 4 } else { 2 });
        let fat_size_aligned = (fat_size_bytes + 4095) / 4096 * 4096;
        let cluster_offset = fat_offset + fat_size_aligned;

        let mut fat_data = vec![0u8; fat_size_aligned as usize];
        device.seek(SeekFrom::Start(fat_offset))?;
        device.read_exact(&mut fat_data)?;

        let entry_size = if is_fat32 { 4 } else { 2 };
        let mut free_clusters = 0u32;
        for cluster in 2..=num_clusters {
            let index = cluster as usize * entry_size;
            let value = if is_fat32 {
                u32::from_be_bytes(fat_data[index..index + 4].try_into().unwrap())
            } else {
                u16::from_be_bytes(fat_data[index..index + 2].try_into().unwrap()) as u32
            };
            if value == 0 {
                free_clusters += 1;
            }
        }

        let cluster_cache = LruCache::new(NonZeroUsize::new(128).unwrap());
        let cluster_chain_hints = LruCache::new(NonZeroUsize::new(256).unwrap());

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
            next_free_cluster: 2,
            free_clusters,
            cluster_cache,
            cluster_chain_hints,
        })
    }

    pub fn read_cluster(&mut self, cluster: u32) -> io::Result<Vec<u8>> {
        if cluster == 0 || cluster > self.num_clusters {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Cluster {} is invalid (bounds: 1..={})",
                    cluster, self.num_clusters
                ),
            ));
        }

        if let Some(cached) = self.cluster_cache.get(&cluster) {
            return Ok(cached.clone());
        }

        let offset = self.cluster_offset + ((cluster as u64 - 1) * self.cluster_size);
        self.device.seek(SeekFrom::Start(offset))?;
        let mut data = vec![0u8; self.cluster_size as usize];
        self.device.read_exact(&mut data)?;

        self.cluster_cache.put(cluster, data.clone());
        Ok(data)
    }

    pub fn get_next_cluster(&self, cluster: u32) -> io::Result<Option<u32>> {
        if cluster == 0 || cluster > self.num_clusters {
            return Ok(None);
        }

        let index = cluster as usize * (if self.is_fat32 { 4 } else { 2 });
        if index + (if self.is_fat32 { 4 } else { 2 }) > self.fat_data.len() {
            return Ok(None);
        }

        let next_cluster = if self.is_fat32 {
            u32::from_be_bytes(self.fat_data[index..index + 4].try_into().unwrap())
        } else {
            u16::from_be_bytes(self.fat_data[index..index + 2].try_into().unwrap()) as u32
        };

        if (self.is_fat32 && next_cluster >= 0xFFFFFFF7)
            || (!self.is_fat32 && next_cluster >= 0xFFF7)
        {
            return Ok(None);
        }
        if next_cluster == 0 || next_cluster > self.num_clusters {
            return Ok(None);
        }

        Ok(Some(next_cluster))
    }

    pub fn find_cluster_at_index(
        &mut self,
        start_cluster: u32,
        target_idx: usize,
    ) -> io::Result<Option<u32>> {
        if start_cluster == 0 {
            return Ok(None);
        }
        if target_idx == 0 {
            self.cluster_chain_hints
                .put(start_cluster, (0, start_cluster));
            return Ok(Some(start_cluster));
        }

        let (mut current_idx, mut current_cluster) =
            match self.cluster_chain_hints.get(&start_cluster) {
                Some(&(hint_idx, hint_cluster)) if hint_idx <= target_idx => {
                    (hint_idx, hint_cluster)
                }
                _ => (0, start_cluster),
            };

        while current_idx < target_idx {
            match self.get_next_cluster(current_cluster)? {
                Some(next) => {
                    current_cluster = next;
                    current_idx += 1;
                }
                None => return Ok(None),
            }
        }

        self.cluster_chain_hints
            .put(start_cluster, (target_idx, current_cluster));
        Ok(Some(current_cluster))
    }

    pub fn read_file_chunk(
        &mut self,
        start_cluster: u32,
        offset: u64,
        size: u32,
    ) -> io::Result<Vec<u8>> {
        if start_cluster == 0 {
            return Ok(Vec::new());
        }

        let start_cluster_idx = (offset / self.cluster_size) as usize;
        let mut current_offset_in_cluster = (offset % self.cluster_size) as usize;

        let mut current_cluster = match self.find_cluster_at_index(start_cluster, start_cluster_idx)? {
            Some(c) => c,
            None => return Ok(Vec::new()),
        };

        let mut current_cluster_idx = start_cluster_idx;
        let mut data = Vec::with_capacity(size as usize);
        let mut remaining_size = size as u64;

        while remaining_size > 0 {
            let cluster_data = self.read_cluster(current_cluster)?;

            let available_in_cluster = (self.cluster_size as usize) - current_offset_in_cluster;
            let to_read = std::cmp::min(remaining_size as usize, available_in_cluster);

            data.extend_from_slice(
                &cluster_data[current_offset_in_cluster..current_offset_in_cluster + to_read],
            );

            remaining_size -= to_read as u64;
            current_offset_in_cluster = 0;

            if remaining_size > 0 {
                match self.get_next_cluster(current_cluster)? {
                    Some(next) => {
                        current_cluster = next;
                        current_cluster_idx += 1;
                        self.cluster_chain_hints
                            .put(start_cluster, (current_cluster_idx, current_cluster));
                    }
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
            let chunk = &data[i..i + 64];
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

    pub fn write_cluster(&mut self, cluster: u32, data: &[u8]) -> io::Result<()> {
        if cluster == 0 || cluster > self.num_clusters {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Cluster {} is invalid (bounds: 1..={})",
                    cluster, self.num_clusters
                ),
            ));
        }
        if data.len() as u64 != self.cluster_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Data size does not match cluster size",
            ));
        }
        let offset = self.cluster_offset + ((cluster as u64 - 1) * self.cluster_size);
        self.device.seek(SeekFrom::Start(offset))?;
        self.device.write_all(data)?;

        self.cluster_cache.put(cluster, data.to_vec());
        Ok(())
    }

    pub fn set_fat_entry(&mut self, cluster: u32, value: u32) -> io::Result<()> {
        if cluster == 0 || cluster > self.num_clusters {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Cluster {} is out of bounds (1..={})",
                    cluster, self.num_clusters
                ),
            ));
        }

        let entry_size = if self.is_fat32 { 4 } else { 2 };
        let index = cluster as usize * entry_size;
        if index + entry_size > self.fat_data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cluster out of bounds",
            ));
        }

        let old_value = if self.is_fat32 {
            u32::from_be_bytes(self.fat_data[index..index + 4].try_into().unwrap())
        } else {
            u16::from_be_bytes(self.fat_data[index..index + 2].try_into().unwrap()) as u32
        };

        if old_value == 0 && value != 0 {
            self.free_clusters = self.free_clusters.saturating_sub(1);
        } else if old_value != 0 && value == 0 {
            self.free_clusters += 1;
        }

        if self.is_fat32 {
            self.fat_data[index..index + 4].copy_from_slice(&value.to_be_bytes());
        } else {
            self.fat_data[index..index + 2].copy_from_slice(&(value as u16).to_be_bytes());
        }

        Ok(())
    }

    pub fn allocate_cluster(&mut self) -> io::Result<u32> {
        let entry_size = if self.is_fat32 { 4 } else { 2 };
        if self.num_clusters < 2 {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "No free clusters",
            ));
        }

        let total_clusters = self.num_clusters;
        let cluster_count = total_clusters - 1;

        let start_search = std::cmp::max(2, self.next_free_cluster);

        for i in 0..cluster_count {
            let mut cluster = start_search + i;
            if cluster > total_clusters {
                cluster = 2 + (cluster - total_clusters - 1);
            }

            let index = cluster as usize * entry_size;
            let value = if self.is_fat32 {
                u32::from_be_bytes(self.fat_data[index..index + 4].try_into().unwrap())
            } else {
                u16::from_be_bytes(self.fat_data[index..index + 2].try_into().unwrap()) as u32
            };

            if value == 0 {
                let eoc = if self.is_fat32 { 0xFFFFFFFF } else { 0xFFFF };
                self.set_fat_entry(cluster, eoc)?;
                self.next_free_cluster = if cluster >= total_clusters {
                    2
                } else {
                    cluster + 1
                };
                return Ok(cluster);
            }
        }

        Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "No free clusters",
        ))
    }

    pub fn free_chain(&mut self, start_cluster: u32) -> io::Result<()> {
        self.cluster_chain_hints.pop(&start_cluster);
        let mut current_cluster = start_cluster;
        while current_cluster != 0 {
            let next_cluster = self.get_next_cluster(current_cluster)?;
            self.set_fat_entry(current_cluster, 0)?;
            self.cluster_cache.pop(&current_cluster);
            match next_cluster {
                Some(next) => current_cluster = next,
                None => break,
            }
        }
        Ok(())
    }

    pub fn get_cluster_chain(&self, start_cluster: u32) -> io::Result<Vec<u32>> {
        let mut chain = Vec::new();
        if start_cluster == 0 {
            return Ok(chain);
        }

        let mut current = start_cluster;
        chain.push(current);

        while let Some(next) = self.get_next_cluster(current)? {
            chain.push(next);
            current = next;
        }

        Ok(chain)
    }

    pub fn write_file_data(
        &mut self,
        start_cluster: u32,
        offset: u64,
        data: &[u8],
    ) -> io::Result<(u32, u32)> {
        let mut first_cluster = start_cluster;
        if first_cluster == 0 {
            first_cluster = self.allocate_cluster()?;
        }

        let start_cluster_idx = (offset / self.cluster_size) as usize;
        let mut current_offset_in_cluster = (offset % self.cluster_size) as usize;

        let (mut current_idx, mut current_cluster) =
            match self.cluster_chain_hints.get(&first_cluster) {
                Some(&(hint_idx, hint_cluster)) if hint_idx <= start_cluster_idx => {
                    (hint_idx, hint_cluster)
                }
                _ => (0, first_cluster),
            };

        while current_idx < start_cluster_idx {
            match self.get_next_cluster(current_cluster)? {
                Some(next) => {
                    current_cluster = next;
                    current_idx += 1;
                }
                None => {
                    let new_cluster = self.allocate_cluster()?;
                    self.set_fat_entry(current_cluster, new_cluster)?;
                    current_cluster = new_cluster;
                    current_idx += 1;
                }
            }
        }

        self.cluster_chain_hints
            .put(first_cluster, (start_cluster_idx, current_cluster));

        let mut data_offset = 0;
        let mut bytes_written = 0;
        let mut current_cluster_idx = start_cluster_idx;

        while data_offset < data.len() {
            let bytes_to_write = std::cmp::min(
                self.cluster_size as usize - current_offset_in_cluster,
                data.len() - data_offset,
            );

            if current_offset_in_cluster == 0 && bytes_to_write == self.cluster_size as usize {
                self.write_cluster(
                    current_cluster,
                    &data[data_offset..data_offset + bytes_to_write],
                )?;
            } else {
                let mut cluster_data = self.read_cluster(current_cluster)?;
                cluster_data[current_offset_in_cluster..current_offset_in_cluster + bytes_to_write]
                    .copy_from_slice(&data[data_offset..data_offset + bytes_to_write]);
                self.write_cluster(current_cluster, &cluster_data)?;
            }

            data_offset += bytes_to_write;
            bytes_written += bytes_to_write;
            current_offset_in_cluster = 0;

            if data_offset < data.len() {
                match self.get_next_cluster(current_cluster)? {
                    Some(next) => {
                        current_cluster = next;
                        current_cluster_idx += 1;
                    }
                    None => {
                        let new_cluster = self.allocate_cluster()?;
                        self.set_fat_entry(current_cluster, new_cluster)?;
                        current_cluster = new_cluster;
                        current_cluster_idx += 1;
                    }
                }
                self.cluster_chain_hints
                    .put(first_cluster, (current_cluster_idx, current_cluster));
            }
        }

        self.cluster_chain_hints
            .put(first_cluster, (current_cluster_idx, current_cluster));
        Ok((first_cluster, offset as u32 + bytes_written as u32))
    }

    pub fn truncate_chain(
        &mut self,
        start_cluster: u32,
        new_cluster_count: u32,
    ) -> io::Result<Option<u32>> {
        self.cluster_chain_hints.pop(&start_cluster);
        if new_cluster_count == 0 {
            if start_cluster != 0 {
                self.free_chain(start_cluster)?;
            }
            return Ok(None);
        }

        if start_cluster == 0 {
            let first_cluster = self.allocate_cluster()?;
            let mut prev_cluster = first_cluster;

            for _ in 1..new_cluster_count {
                let new_cluster = self.allocate_cluster()?;
                self.set_fat_entry(prev_cluster, new_cluster)?;
                prev_cluster = new_cluster;
            }
            return Ok(Some(first_cluster));
        }

        let chain = self.get_cluster_chain(start_cluster)?;

        if (new_cluster_count as usize) < chain.len() {
            let last_keep = chain[new_cluster_count as usize - 1];
            let first_free = chain[new_cluster_count as usize];

            let eoc = if self.is_fat32 { 0xFFFFFFFF } else { 0xFFFF };
            self.set_fat_entry(last_keep, eoc)?;
            self.free_chain(first_free)?;
        } else if (new_cluster_count as usize) > chain.len() {
            let mut prev_cluster = *chain.last().unwrap();
            let add_count = new_cluster_count as usize - chain.len();

            for _ in 0..add_count {
                let new_cluster = self.allocate_cluster()?;
                self.set_fat_entry(prev_cluster, new_cluster)?;
                prev_cluster = new_cluster;
            }
        }

        Ok(Some(start_cluster))
    }

    pub fn add_directory_entry(
        &mut self,
        dir_start_cluster: u32,
        entry: &FatxRawDirectoryEntry,
    ) -> io::Result<()> {
        let chain = self.get_cluster_chain(dir_start_cluster)?;

        let serialized_entry = entry.write_to();

        for cluster in &chain {
            let mut data = self.read_cluster(*cluster)?;
            for i in (0..data.len()).step_by(64) {
                let filename_len = data[i];
                if filename_len == FATX_END_OF_DIR_MARKER
                    || filename_len == FATX_END_OF_DIR_MARKER2
                    || filename_len == FATX_DELETED_FILE_MARKER
                {
                    data[i..i + 64].copy_from_slice(&serialized_entry);
                    self.write_cluster(*cluster, &data)?;
                    return Ok(());
                }
            }
        }

        // In FATX, the root directory is strictly restricted to a single cluster
        if dir_start_cluster == self.superblock.root_cluster {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "FATX root directory is full (limited to 1 cluster)",
            ));
        }

        let new_cluster = self.allocate_cluster()?;
        let last_cluster = *chain.last().unwrap();
        self.set_fat_entry(last_cluster, new_cluster)?;

        let mut new_data = vec![FATX_END_OF_DIR_MARKER; self.cluster_size as usize];
        new_data[0..64].copy_from_slice(&serialized_entry);
        self.write_cluster(new_cluster, &new_data)?;

        Ok(())
    }

    pub fn remove_directory_entry(
        &mut self,
        dir_start_cluster: u32,
        filename: &str,
    ) -> io::Result<()> {
        let chain = self.get_cluster_chain(dir_start_cluster)?;

        for cluster in &chain {
            let mut data = self.read_cluster(*cluster)?;
            let mut modified = false;

            for i in (0..data.len()).step_by(64) {
                let filename_len = data[i];
                if filename_len == FATX_END_OF_DIR_MARKER || filename_len == FATX_END_OF_DIR_MARKER2
                {
                    break;
                }
                if filename_len == FATX_DELETED_FILE_MARKER || filename_len == 0 {
                    continue;
                }

                let mut entry_data = [0u8; 64];
                entry_data.copy_from_slice(&data[i..i + 64]);
                if let Ok(entry) = FatxRawDirectoryEntry::read_from(&entry_data) {
                    if entry.filename == filename {
                        data[i] = FATX_DELETED_FILE_MARKER;
                        modified = true;
                        break;
                    }
                }
            }

            if modified {
                self.write_cluster(*cluster, &data)?;
                return Ok(());
            }
        }

        Err(io::Error::new(io::ErrorKind::NotFound, "Entry not found"))
    }

    pub fn update_directory_entry(
        &mut self,
        dir_start_cluster: u32,
        filename: &str,
        new_entry: &FatxRawDirectoryEntry,
    ) -> io::Result<()> {
        let chain = self.get_cluster_chain(dir_start_cluster)?;
        let serialized_entry = new_entry.write_to();

        for cluster in &chain {
            let mut data = self.read_cluster(*cluster)?;
            let mut modified = false;

            for i in (0..data.len()).step_by(64) {
                let filename_len = data[i];
                if filename_len == FATX_END_OF_DIR_MARKER || filename_len == FATX_END_OF_DIR_MARKER2
                {
                    break;
                }
                if filename_len == FATX_DELETED_FILE_MARKER || filename_len == 0 {
                    continue;
                }

                let mut entry_data = [0u8; 64];
                entry_data.copy_from_slice(&data[i..i + 64]);
                if let Ok(entry) = FatxRawDirectoryEntry::read_from(&entry_data) {
                    if entry.filename == filename {
                        data[i..i + 64].copy_from_slice(&serialized_entry);
                        modified = true;
                        break;
                    }
                }
            }

            if modified {
                self.write_cluster(*cluster, &data)?;
                return Ok(());
            }
        }

        Err(io::Error::new(io::ErrorKind::NotFound, "Entry not found"))
    }

    pub fn flush_fat(&mut self) -> io::Result<()> {
        self.device.seek(SeekFrom::Start(self.fat_offset))?;
        self.device.write_all(&self.fat_data)?;
        self.device.sync_all()?;
        Ok(())
    }
}
