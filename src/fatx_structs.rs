use byteorder::{BigEndian, ReadBytesExt};
use std::io::{self, Cursor, Read};

pub const FATX_SIGNATURE: u32 = 0x58544146; // 'XTAF'
pub const FATX_SUPERBLOCK_SIZE: usize = 4096;
pub const FATX_DELETED_FILE_MARKER: u8 = 0xE5;
pub const FATX_END_OF_DIR_MARKER: u8 = 0xFF;
pub const FATX_END_OF_DIR_MARKER2: u8 = 0x00;

#[allow(dead_code)]
#[derive(Debug)]
pub struct FatxSuperblock {
    pub signature: u32,
    pub volume_id: u32,
    pub sectors_per_cluster: u32,
    pub root_cluster: u32,
    pub unknown1: u16,
}

impl FatxSuperblock {
    pub fn read_from(data: &[u8; FATX_SUPERBLOCK_SIZE]) -> io::Result<Self> {
        let mut cursor = Cursor::new(data.as_ref());
        let signature = cursor.read_u32::<BigEndian>()?;
        let volume_id = cursor.read_u32::<BigEndian>()?;
        let sectors_per_cluster = cursor.read_u32::<BigEndian>()?;
        let root_cluster = cursor.read_u32::<BigEndian>()?;
        let unknown1 = cursor.read_u16::<BigEndian>()?;

        if signature != FATX_SIGNATURE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid FATX signature: {:#x}", signature),
            ));
        }

        Ok(Self {
            signature,
            volume_id,
            sectors_per_cluster,
            root_cluster,
            unknown1,
        })
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct FatxRawDirectoryEntry {
    pub filename_len: u8,
    pub attributes: u8,
    pub filename: String,
    pub first_cluster: u32,
    pub file_size: u32,
    pub modified_time: u16,
    pub modified_date: u16,
    pub created_time: u16,
    pub created_date: u16,
    pub accessed_time: u16,
    pub accessed_date: u16,
}

impl FatxRawDirectoryEntry {
    pub fn read_from(data: &[u8; 64]) -> io::Result<Self> {
        let mut cursor = Cursor::new(data.as_ref());
        
        let filename_len = cursor.read_u8()?;
        let attributes = cursor.read_u8()?;
        
        let mut filename_bytes = [0u8; 42];
        cursor.read_exact(&mut filename_bytes)?;
        
        let actual_len = std::cmp::min(filename_len as usize, 42);
        let filename = String::from_utf8_lossy(&filename_bytes.as_slice()[..actual_len]).to_string();
        
        let first_cluster = cursor.read_u32::<BigEndian>()?;
        let file_size = cursor.read_u32::<BigEndian>()?;
        let modified_time = cursor.read_u16::<BigEndian>()?;
        let modified_date = cursor.read_u16::<BigEndian>()?;
        let created_time = cursor.read_u16::<BigEndian>()?;
        let created_date = cursor.read_u16::<BigEndian>()?;
        let accessed_time = cursor.read_u16::<BigEndian>()?;
        let accessed_date = cursor.read_u16::<BigEndian>()?;

        Ok(Self {
            filename_len,
            attributes,
            filename,
            first_cluster,
            file_size,
            modified_time,
            modified_date,
            created_time,
            created_date,
            accessed_time,
            accessed_date,
        })
    }
}
