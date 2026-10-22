use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Cursor, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};
pub const FATX_SIGNATURE: u32 = 0x58544146; // 'XTAF'
pub const FATX_SUPERBLOCK_SIZE: usize = 4096;
pub const FATX_DELETED_FILE_MARKER: u8 = 0xE5;
pub const FATX_END_OF_DIR_MARKER: u8 = 0xFF;
pub const FATX_END_OF_DIR_MARKER2: u8 = 0x00;

#[allow(dead_code)]
pub const FATX_ATTR_READONLY: u8 = 0x01;
#[allow(dead_code)]
pub const FATX_ATTR_HIDDEN: u8 = 0x02;
#[allow(dead_code)]
pub const FATX_ATTR_SYSTEM: u8 = 0x04;
pub const FATX_ATTR_DIRECTORY: u8 = 0x10;
#[allow(dead_code)]
pub const FATX_ATTR_ARCHIVE: u8 = 0x20;

pub fn is_valid_fatx_filename(name: &str) -> bool {
    if name.is_empty() || name.len() > 42 {
        return false;
    }
    if !name.is_ascii() {
        return false;
    }
    const ILLEGAL_CHARS: &[char] = &[
        '"', '*', '+', ',', '/', ':', ';', '<', '=', '>', '?', '\\', '|',
    ];
    for c in name.chars() {
        if (c as u32) < 32 || ILLEGAL_CHARS.contains(&c) {
            return false;
        }
    }
    true
}

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
    pub created_date: u16,
    pub created_time: u16,
    pub modified_date: u16,
    pub modified_time: u16,
    pub accessed_date: u16,
    pub accessed_time: u16,
}

impl FatxRawDirectoryEntry {
    pub fn read_from(data: &[u8; 64]) -> io::Result<Self> {
        let mut cursor = Cursor::new(data.as_ref());

        let filename_len = cursor.read_u8()?;
        let attributes = cursor.read_u8()?;

        let mut filename_bytes = [0u8; 42];
        cursor.read_exact(&mut filename_bytes)?;

        let actual_len = std::cmp::min(filename_len as usize, 42);
        let filename =
            String::from_utf8_lossy(&filename_bytes.as_slice()[..actual_len]).to_string();

        let first_cluster = cursor.read_u32::<BigEndian>()?;
        let file_size = cursor.read_u32::<BigEndian>()?;
        let created_date = cursor.read_u16::<BigEndian>()?;
        let created_time = cursor.read_u16::<BigEndian>()?;
        let modified_date = cursor.read_u16::<BigEndian>()?;
        let modified_time = cursor.read_u16::<BigEndian>()?;
        let accessed_date = cursor.read_u16::<BigEndian>()?;
        let accessed_time = cursor.read_u16::<BigEndian>()?;

        Ok(Self {
            filename_len,
            attributes,
            filename,
            first_cluster,
            file_size,
            created_date,
            created_time,
            modified_date,
            modified_time,
            accessed_date,
            accessed_time,
        })
    }

    pub fn write_to(&self) -> [u8; 64] {
        let mut data = [0u8; 64];
        let mut cursor = Cursor::new(&mut data[..]);

        cursor.write_u8(self.filename_len).unwrap();
        cursor.write_u8(self.attributes).unwrap();

        let mut name_bytes = self.filename.as_bytes().to_vec();
        name_bytes.resize(42, 0);
        cursor.write_all(&name_bytes).unwrap();

        cursor.write_u32::<BigEndian>(self.first_cluster).unwrap();
        cursor.write_u32::<BigEndian>(self.file_size).unwrap();
        cursor.write_u16::<BigEndian>(self.created_date).unwrap();
        cursor.write_u16::<BigEndian>(self.created_time).unwrap();
        cursor.write_u16::<BigEndian>(self.modified_date).unwrap();
        cursor.write_u16::<BigEndian>(self.modified_time).unwrap();
        cursor.write_u16::<BigEndian>(self.accessed_date).unwrap();
        cursor.write_u16::<BigEndian>(self.accessed_time).unwrap();

        data
    }

    pub fn systemtime_to_fatx_datetime(time: SystemTime) -> (u16, u16) {
        let duration = time
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let seconds_since_1970 = duration as i64;

        let seconds_per_day = 86400;
        let mut days = seconds_since_1970 / seconds_per_day;
        let mut seconds_in_day = seconds_since_1970 % seconds_per_day;

        let mut year = 1970;
        loop {
            let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
            let days_in_year = if is_leap { 366 } else { 365 };
            if days >= days_in_year {
                days -= days_in_year;
                year += 1;
            } else {
                break;
            }
        }

        let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let days_in_month = [
            31,
            if is_leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];

        let mut month = 1;
        for &dim in &days_in_month {
            if days >= dim {
                days -= dim;
                month += 1;
            } else {
                break;
            }
        }

        let day = days + 1;

        let hours = seconds_in_day / 3600;
        seconds_in_day %= 3600;
        let minutes = seconds_in_day / 60;
        let seconds = seconds_in_day % 60;

        let fat_year = std::cmp::max(1980, year) - 1980;

        let fat_date =
            ((fat_year as u16 & 0x7F) << 9) | ((month as u16 & 0x0F) << 5) | (day as u16 & 0x1F);
        let fat_time = ((hours as u16 & 0x1F) << 11)
            | ((minutes as u16 & 0x3F) << 5)
            | ((seconds as u16 / 2) & 0x1F);

        (fat_date, fat_time)
    }

    pub fn fatx_datetime_to_systemtime(fat_date: u16, fat_time: u16) -> SystemTime {
        if fat_date == 0 {
            return UNIX_EPOCH;
        }

        let year = (((fat_date >> 9) & 0x7F) as i64) + 1980;
        let month = ((fat_date >> 5) & 0x0F) as usize;
        let day = (fat_date & 0x1F) as i64;

        if month < 1 || month > 12 || day < 1 || day > 31 {
            return UNIX_EPOCH;
        }

        let hours = ((fat_time >> 11) & 0x1F) as i64;
        let minutes = ((fat_time >> 5) & 0x3F) as i64;
        let seconds = ((fat_time & 0x1F) as i64) * 2;

        let mut days = 0i64;
        for y in 1970..year {
            let is_leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
            days += if is_leap { 366 } else { 365 };
        }

        let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let days_in_month = [
            31,
            if is_leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];

        for m in 1..month {
            days += days_in_month[m - 1];
        }
        days += day - 1;

        let total_seconds = days * 86400 + hours * 3600 + minutes * 60 + seconds;
        if total_seconds < 0 {
            return UNIX_EPOCH;
        }

        UNIX_EPOCH + std::time::Duration::from_secs(total_seconds as u64)
    }
}
