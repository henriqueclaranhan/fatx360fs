use crate::fatx_structs::FatxRawDirectoryEntry;
use crate::filesystem::FatxFileSystem;
use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const FATX_ATTR_DIRECTORY: u8 = 0x10;

#[derive(Clone, Debug)]
struct InodeData {
    ino: u64,
    parent_ino: u64,
    parent_cluster: u32,
    name: String,
    start_cluster: u32,
    size: u32,
    is_dir: bool,
    lookup_count: u64,
    dirty: bool,
}

pub struct FatxDriver {
    fs: Mutex<FatxFileSystem>,
    inodes: Mutex<HashMap<u64, InodeData>>,
    dirty_inodes: Mutex<HashSet<u64>>,
    uid: u32,
    gid: u32,
}

fn generate_ino(parent_ino: u64, name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    parent_ino.hash(&mut hasher);
    name.hash(&mut hasher);
    let mut hash = hasher.finish();
    if hash == 1 {
        hash = 2;
    }
    hash
}

impl FatxDriver {
    pub fn new(fs: FatxFileSystem) -> Self {
        let mut inodes = HashMap::new();
        inodes.insert(
            1,
            InodeData {
                ino: 1,
                parent_ino: 1,
                parent_cluster: 0,
                name: "".to_string(),
                start_cluster: fs.superblock.root_cluster,
                size: 0,
                is_dir: true,
                lookup_count: 1,
                dirty: false,
            },
        );

        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };

        Self {
            fs: Mutex::new(fs),
            inodes: Mutex::new(inodes),
            dirty_inodes: Mutex::new(HashSet::new()),
            uid,
            gid,
        }
    }

    fn make_attr(&self, data: &InodeData) -> FileAttr {
        FileAttr {
            ino: INodeNo(data.ino),
            size: data.size as u64,
            blocks: (data.size as u64 + 511) / 512,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: if data.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if data.is_dir { 0o755 } else { 0o644 },
            nlink: if data.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Flush all dirty inode metadata to disk directory entries
    fn sync_dirty_inodes(&self) -> std::io::Result<()> {
        let dirty_inos: Vec<u64> = self.dirty_inodes.lock().unwrap().iter().copied().collect();
        if dirty_inos.is_empty() {
            return Ok(());
        }

        let mut fs = self.fs.lock().unwrap();
        let inodes = self.inodes.lock().unwrap();
        let (mod_time, mod_date) =
            FatxRawDirectoryEntry::systemtime_to_fatx_datetime(SystemTime::now());

        let mut synced_inos = Vec::new();

        for ino in &dirty_inos {
            if let Some(data) = inodes.get(ino) {
                if !data.dirty || data.parent_cluster == 0 {
                    synced_inos.push(*ino);
                    continue;
                }
                if let Ok(entries) = fs.read_directory(data.parent_cluster) {
                    if let Some(entry) = entries.iter().find(|e| e.filename == data.name) {
                        let mut updated = entry.clone();
                        updated.file_size = data.size;
                        updated.first_cluster = data.start_cluster;
                        updated.modified_time = mod_time;
                        updated.modified_date = mod_date;
                        updated.accessed_time = mod_time;
                        updated.accessed_date = mod_date;
                        if fs
                            .update_directory_entry(data.parent_cluster, &data.name, &updated)
                            .is_ok()
                        {
                            synced_inos.push(*ino);
                        }
                    }
                }
            }
        }
        drop(inodes);

        fs.flush_fat()?;

        let mut inodes = self.inodes.lock().unwrap();
        let mut dirty_inodes = self.dirty_inodes.lock().unwrap();
        for ino in synced_inos {
            if let Some(data) = inodes.get_mut(&ino) {
                data.dirty = false;
            }
            dirty_inodes.remove(&ino);
        }

        Ok(())
    }
}

impl Filesystem for FatxDriver {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name_str = name.to_string_lossy().to_string();
        let parent: u64 = parent.into();
        let target_ino = generate_ino(parent, &name_str);

        if let Some(data) = self.inodes.lock().unwrap().get_mut(&target_ino) {
            data.lookup_count += 1;
            let attr = FileAttr {
                ino: INodeNo(data.ino),
                size: data.size as u64,
                blocks: (data.size as u64 + 511) / 512,
                atime: UNIX_EPOCH,
                mtime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                kind: if data.is_dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                },
                perm: if data.is_dir { 0o755 } else { 0o644 },
                nlink: if data.is_dir { 2 } else { 1 },
                uid: self.uid,
                gid: self.gid,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            };
            reply.entry(&Duration::from_secs(1), &attr, Generation(0));
            return;
        }

        let parent_data = match self.inodes.lock().unwrap().get(&parent) {
            Some(d) if d.is_dir => d.clone(),
            _ => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match self
            .fs
            .lock()
            .unwrap()
            .read_directory(parent_data.start_cluster)
        {
            Ok(entries) => {
                for entry in entries {
                    if entry.filename == name_str {
                        let is_dir = (entry.attributes & FATX_ATTR_DIRECTORY) != 0;
                        let new_data = InodeData {
                            ino: target_ino,
                            parent_ino: parent,
                            parent_cluster: parent_data.start_cluster,
                            name: entry.filename.clone(),
                            start_cluster: entry.first_cluster,
                            size: entry.file_size,
                            is_dir,
                            lookup_count: 1,
                            dirty: false,
                        };

                        let attr = self.make_attr(&new_data);
                        self.inodes.lock().unwrap().insert(target_ino, new_data);

                        reply.entry(&Duration::from_secs(1), &attr, Generation(0));
                        return;
                    }
                }
                reply.error(Errno::ENOENT);
            }
            Err(_) => {
                reply.error(Errno::EIO);
            }
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let ino_u64: u64 = ino.into();
        if ino_u64 == 1 {
            return;
        }

        let mut should_remove = false;
        let mut is_dirty = false;
        if let Some(data) = self.inodes.lock().unwrap().get_mut(&ino_u64) {
            data.lookup_count = data.lookup_count.saturating_sub(nlookup);
            if data.lookup_count == 0 {
                if data.dirty {
                    is_dirty = true;
                } else {
                    should_remove = true;
                }
            }
        }

        if is_dirty {
            let _ = self.sync_dirty_inodes();
            let mut inodes = self.inodes.lock().unwrap();
            if let Some(data) = inodes.get(&ino_u64) {
                if data.lookup_count == 0 && !data.dirty {
                    inodes.remove(&ino_u64);
                }
            }
        } else if should_remove {
            self.inodes.lock().unwrap().remove(&ino_u64);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino_u64: u64 = ino.into();
        if let Some(data) = self.inodes.lock().unwrap().get(&ino_u64) {
            reply.attr(&Duration::from_secs(1), &self.make_attr(data));
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino_u64: u64 = ino.into();
        let parent_ino = match self.inodes.lock().unwrap().get(&ino_u64) {
            Some(d) if d.is_dir => d.parent_ino,
            _ => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let start_cluster = match self.inodes.lock().unwrap().get(&ino_u64) {
            Some(d) => d.start_cluster,
            None => return,
        };

        let mut entries = vec![
            (ino.into(), FileType::Directory, ".".to_string()),
            (parent_ino, FileType::Directory, "..".to_string()),
        ];

        if let Ok(dir_entries) = self.fs.lock().unwrap().read_directory(start_cluster) {
            for entry in dir_entries {
                let is_dir = (entry.attributes & FATX_ATTR_DIRECTORY) != 0;
                let child_ino = generate_ino(ino_u64, &entry.filename);
                entries.push((
                    child_ino,
                    if is_dir {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    },
                    entry.filename,
                ));
            }
        }

        for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(entry.0), (i + 1) as u64, entry.1, entry.2) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let ino_u64: u64 = ino.into();
        let data = match self.inodes.lock().unwrap().get(&ino_u64) {
            Some(d) => d.clone(),
            _ => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if data.is_dir {
            reply.error(Errno::EISDIR);
            return;
        }

        if offset as u64 >= data.size as u64 {
            reply.data(&[]);
            return;
        }

        let max_size = std::cmp::min(size as u64, data.size as u64 - offset as u64) as u32;

        if let Ok(file_data) =
            self.fs
                .lock()
                .unwrap()
                .read_file_chunk(data.start_cluster, offset as u64, max_size)
        {
            reply.data(&file_data);
        } else {
            reply.error(Errno::EIO);
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let ino_u64: u64 = ino.into();
        let inode_data = match self.inodes.lock().unwrap().get(&ino_u64) {
            Some(d) => d.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if inode_data.is_dir {
            reply.error(Errno::EISDIR);
            return;
        }

        let mut fs = self.fs.lock().unwrap();
        match fs.write_file_data(inode_data.start_cluster, offset, data) {
            Ok((new_start_cluster, _)) => {
                let new_size = std::cmp::max(inode_data.size, (offset + data.len() as u64) as u32);

                if let Some(d) = self.inodes.lock().unwrap().get_mut(&ino_u64) {
                    d.size = new_size;
                    d.start_cluster = new_start_cluster;
                    d.dirty = true;
                }
                self.dirty_inodes.lock().unwrap().insert(ino_u64);

                reply.written(data.len() as u32);
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name_str = name.to_string_lossy().to_string();
        if name_str.len() > 42 || !name_str.is_ascii() {
            reply.error(Errno::ENAMETOOLONG);
            return;
        }

        let parent_u64: u64 = parent.into();
        let parent_data = match self.inodes.lock().unwrap().get(&parent_u64) {
            Some(d) if d.is_dir => d.clone(),
            Some(_) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let target_ino = generate_ino(parent_u64, &name_str);
        let (now_time, now_date) =
            FatxRawDirectoryEntry::systemtime_to_fatx_datetime(SystemTime::now());

        let entry = FatxRawDirectoryEntry {
            filename_len: name_str.len() as u8,
            attributes: 0,
            filename: name_str.clone(),
            first_cluster: 0,
            file_size: 0,
            modified_time: now_time,
            modified_date: now_date,
            created_time: now_time,
            created_date: now_date,
            accessed_time: now_time,
            accessed_date: now_date,
        };

        let mut fs = self.fs.lock().unwrap();
        if fs
            .add_directory_entry(parent_data.start_cluster, &entry)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }

        if let Err(_) = fs.flush_fat() {
            reply.error(Errno::EIO);
            return;
        }

        let new_data = InodeData {
            ino: target_ino,
            parent_ino: parent_u64,
            parent_cluster: parent_data.start_cluster,
            name: name_str,
            start_cluster: 0,
            size: 0,
            is_dir: false,
            lookup_count: 1,
            dirty: false,
        };
        let attr = self.make_attr(&new_data);
        self.inodes.lock().unwrap().insert(target_ino, new_data);

        reply.created(
            &Duration::from_secs(1),
            &attr,
            Generation(0),
            FileHandle(0),
            FopenFlags::empty(),
        );
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name_str = name.to_string_lossy().to_string();
        if name_str.len() > 42 || !name_str.is_ascii() {
            reply.error(Errno::ENAMETOOLONG);
            return;
        }

        let parent_u64: u64 = parent.into();
        let parent_data = match self.inodes.lock().unwrap().get(&parent_u64) {
            Some(d) if d.is_dir => d.clone(),
            Some(_) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let target_ino = generate_ino(parent_u64, &name_str);

        let mut fs = self.fs.lock().unwrap();
        let new_cluster = match fs.allocate_cluster() {
            Ok(c) => c,
            Err(_) => {
                reply.error(Errno::ENOSPC);
                return;
            }
        };

        let cluster_size = fs.cluster_size as usize;
        let empty_dir_data = vec![0xFFu8; cluster_size];
        if fs.write_cluster(new_cluster, &empty_dir_data).is_err() {
            reply.error(Errno::EIO);
            return;
        }

        let (now_time, now_date) =
            FatxRawDirectoryEntry::systemtime_to_fatx_datetime(SystemTime::now());

        let entry = FatxRawDirectoryEntry {
            filename_len: name_str.len() as u8,
            attributes: FATX_ATTR_DIRECTORY,
            filename: name_str.clone(),
            first_cluster: new_cluster,
            file_size: 0,
            modified_time: now_time,
            modified_date: now_date,
            created_time: now_time,
            created_date: now_date,
            accessed_time: now_time,
            accessed_date: now_date,
        };

        if fs
            .add_directory_entry(parent_data.start_cluster, &entry)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }

        if let Err(_) = fs.flush_fat() {
            reply.error(Errno::EIO);
            return;
        }

        let new_data = InodeData {
            ino: target_ino,
            parent_ino: parent_u64,
            parent_cluster: parent_data.start_cluster,
            name: name_str,
            start_cluster: new_cluster,
            size: 0,
            is_dir: true,
            lookup_count: 1,
            dirty: false,
        };
        let attr = self.make_attr(&new_data);
        self.inodes.lock().unwrap().insert(target_ino, new_data);

        reply.entry(&Duration::from_secs(1), &attr, Generation(0));
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name_str = name.to_string_lossy().to_string();
        let parent_u64: u64 = parent.into();
        let target_ino = generate_ino(parent_u64, &name_str);

        let parent_data = match self.inodes.lock().unwrap().get(&parent_u64) {
            Some(d) => d.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let mut fs = self.fs.lock().unwrap();

        let target_cluster = match fs.read_directory(parent_data.start_cluster) {
            Ok(entries) => match entries.iter().find(|e| e.filename == name_str) {
                Some(entry) => entry.first_cluster,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            },
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        if target_cluster != 0 {
            if let Err(_) = fs.free_chain(target_cluster) {
                reply.error(Errno::EIO);
                return;
            }
        }

        if fs
            .remove_directory_entry(parent_data.start_cluster, &name_str)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }

        let _ = fs.flush_fat();

        self.dirty_inodes.lock().unwrap().remove(&target_ino);
        self.inodes.lock().unwrap().remove(&target_ino);
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name_str = name.to_string_lossy().to_string();
        let parent_u64: u64 = parent.into();
        let target_ino = generate_ino(parent_u64, &name_str);

        let parent_data = match self.inodes.lock().unwrap().get(&parent_u64) {
            Some(d) => d.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let mut fs = self.fs.lock().unwrap();

        let target_cluster = match fs.read_directory(parent_data.start_cluster) {
            Ok(entries) => match entries.iter().find(|e| e.filename == name_str) {
                Some(entry) => {
                    if (entry.attributes & FATX_ATTR_DIRECTORY) == 0 {
                        reply.error(Errno::ENOTDIR);
                        return;
                    }
                    entry.first_cluster
                }
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            },
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        match fs.read_directory(target_cluster) {
            Ok(child_entries) => {
                if !child_entries.is_empty() {
                    reply.error(Errno::ENOTEMPTY);
                    return;
                }
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        }

        if target_cluster != 0 {
            if let Err(_) = fs.free_chain(target_cluster) {
                reply.error(Errno::EIO);
                return;
            }
        }

        if fs
            .remove_directory_entry(parent_data.start_cluster, &name_str)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }

        let _ = fs.flush_fat();

        self.dirty_inodes.lock().unwrap().remove(&target_ino);
        self.inodes.lock().unwrap().remove(&target_ino);
        reply.ok();
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let name_str = name.to_string_lossy().to_string();
        let newname_str = newname.to_string_lossy().to_string();
        if newname_str.len() > 42 || !newname_str.is_ascii() {
            reply.error(Errno::ENAMETOOLONG);
            return;
        }

        let parent_u64: u64 = parent.into();
        let newparent_u64: u64 = newparent.into();

        let parent_data = match self.inodes.lock().unwrap().get(&parent_u64) {
            Some(d) => d.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let newparent_data = match self.inodes.lock().unwrap().get(&newparent_u64) {
            Some(d) => d.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let mut fs = self.fs.lock().unwrap();

        let old_entry = match fs.read_directory(parent_data.start_cluster) {
            Ok(entries) => match entries.into_iter().find(|e| e.filename == name_str) {
                Some(entry) => entry,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            },
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        if let Ok(new_entries) = fs.read_directory(newparent_data.start_cluster) {
            if let Some(existing) = new_entries.iter().find(|e| e.filename == newname_str) {
                if existing.first_cluster != 0 {
                    let _ = fs.free_chain(existing.first_cluster);
                }
                let _ = fs.remove_directory_entry(newparent_data.start_cluster, &newname_str);
                let existing_ino = generate_ino(newparent_u64, &newname_str);
                self.dirty_inodes.lock().unwrap().remove(&existing_ino);
                self.inodes.lock().unwrap().remove(&existing_ino);
            }
        }

        let (mod_time, mod_date) =
            FatxRawDirectoryEntry::systemtime_to_fatx_datetime(SystemTime::now());
        let new_entry = FatxRawDirectoryEntry {
            filename_len: newname_str.len() as u8,
            filename: newname_str.clone(),
            modified_time: mod_time,
            modified_date: mod_date,
            accessed_time: mod_time,
            accessed_date: mod_date,
            ..old_entry
        };

        if fs
            .add_directory_entry(newparent_data.start_cluster, &new_entry)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }

        if fs
            .remove_directory_entry(parent_data.start_cluster, &name_str)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }

        let _ = fs.flush_fat();

        let old_ino = generate_ino(parent_u64, &name_str);
        let new_ino = generate_ino(newparent_u64, &newname_str);

        let mut dirty_inodes = self.dirty_inodes.lock().unwrap();
        if dirty_inodes.remove(&old_ino) {
            dirty_inodes.insert(new_ino);
        }

        let mut inodes = self.inodes.lock().unwrap();
        if let Some(mut data) = inodes.remove(&old_ino) {
            data.ino = new_ino;
            data.parent_ino = newparent_u64;
            data.parent_cluster = newparent_data.start_cluster;
            data.name = newname_str;
            inodes.insert(new_ino, data);
        }

        reply.ok();
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino_u64: u64 = ino.into();
        let mut inode_data = match self.inodes.lock().unwrap().get(&ino_u64) {
            Some(d) => d.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if let Some(new_size) = size {
            if inode_data.is_dir {
                reply.error(Errno::EISDIR);
                return;
            }

            let mut fs = self.fs.lock().unwrap();
            let cluster_size = fs.cluster_size;
            let needed_clusters = if new_size == 0 {
                0
            } else {
                ((new_size + cluster_size - 1) / cluster_size) as u32
            };

            match fs.truncate_chain(inode_data.start_cluster, needed_clusters) {
                Ok(new_start) => {
                    inode_data.start_cluster = new_start.unwrap_or(0);
                    inode_data.size = new_size as u32;
                }
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            }

            if let Ok(entries) = fs.read_directory(inode_data.parent_cluster) {
                if let Some(entry) = entries.iter().find(|e| e.filename == inode_data.name) {
                    let (mod_time, mod_date) =
                        FatxRawDirectoryEntry::systemtime_to_fatx_datetime(SystemTime::now());
                    let mut updated = entry.clone();
                    updated.file_size = inode_data.size;
                    updated.first_cluster = inode_data.start_cluster;
                    updated.modified_time = mod_time;
                    updated.modified_date = mod_date;
                    let _ = fs.update_directory_entry(
                        inode_data.parent_cluster,
                        &inode_data.name,
                        &updated,
                    );
                }
            }

            if let Some(d) = self.inodes.lock().unwrap().get_mut(&ino_u64) {
                d.size = inode_data.size;
                d.start_cluster = inode_data.start_cluster;
                d.dirty = false;
            }
            self.dirty_inodes.lock().unwrap().remove(&ino_u64);
            let _ = fs.flush_fat();
        }

        let attr = self.make_attr(&inode_data);
        reply.attr(&Duration::from_secs(1), &attr);
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let ino_u64: u64 = ino.into();
        if self.inodes.lock().unwrap().contains_key(&ino_u64) {
            reply.opened(FileHandle(0), FopenFlags::empty());
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        if self.sync_dirty_inodes().is_ok() {
            reply.ok();
        } else {
            reply.error(Errno::EIO);
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        if self.sync_dirty_inodes().is_ok() {
            reply.ok();
        } else {
            reply.error(Errno::EIO);
        }
    }

    fn destroy(&mut self) {
        let _ = self.sync_dirty_inodes();
        if let Ok(mut fs) = self.fs.lock() {
            let _ = fs.flush_fat();
        }
    }
}
