use crate::filesystem::FatxFileSystem;
use std::sync::Mutex;
use fuser::{
    Errno, Generation, INodeNo, FileHandle, LockOwner, OpenFlags,
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request,
    };
use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH};
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;

const FATX_ATTR_DIRECTORY: u8 = 0x10;

#[derive(Clone, Debug)]
struct InodeData {
    ino: u64,
    parent_ino: u64,
    start_cluster: u32,
    size: u32,
    is_dir: bool,
    lookup_count: u64,
}

pub struct FatxDriver {
    fs: Mutex<FatxFileSystem>,
    inodes: Mutex<HashMap<u64, InodeData>>,
    uid: u32,
    gid: u32,
}

fn generate_ino(parent_ino: u64, name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    parent_ino.hash(&mut hasher);
    name.hash(&mut hasher);
    let mut hash = hasher.finish();
    if hash == 1 {
        hash = 2; // Avoid collision with root
    }
    hash
}

impl FatxDriver {
    pub fn new(fs: FatxFileSystem) -> Self {
        let mut inodes = HashMap::new();
        // Root inode
        inodes.insert(
            1,
            InodeData {
                ino: 1,
                parent_ino: 1,
                start_cluster: fs.superblock.root_cluster,
                size: 0,
                is_dir: true,
                lookup_count: 1,
            },
        );

        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };

        Self {
            fs: Mutex::new(fs),
            inodes: Mutex::new(inodes),
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
            kind: if data.is_dir { FileType::Directory } else { FileType::RegularFile },
            perm: if data.is_dir { 0o755 } else { 0o644 },
            nlink: if data.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

impl Filesystem for FatxDriver {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEntry) {
        let name_str = name.to_string_lossy().to_string();
        let parent: u64 = parent.into();
        let target_ino = generate_ino(parent, &name_str);

        // If it's already cached, just increment refcount and return
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
                kind: if data.is_dir { FileType::Directory } else { FileType::RegularFile },
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

        // Otherwise, we need to read the parent directory to find it
        let parent_data = match self.inodes.lock().unwrap().get(&parent) {
            Some(d) if d.is_dir => d.clone(),
            _ => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match self.fs.lock().unwrap().read_directory(parent_data.start_cluster) {
            Ok(entries) => {
                for entry in entries {
                    if entry.filename == name_str {
                        let is_dir = (entry.attributes & FATX_ATTR_DIRECTORY) != 0;
                        let new_data = InodeData {
                            ino: target_ino,
                            parent_ino: parent,
                            start_cluster: entry.first_cluster,
                            size: entry.file_size,
                            is_dir,
                            lookup_count: 1,
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
        
        let mut remove = false;
        if let Some(data) = self.inodes.lock().unwrap().get_mut(&ino_u64) {
            data.lookup_count = data.lookup_count.saturating_sub(nlookup);
            if data.lookup_count == 0 {
                remove = true;
            }
        }
        if remove {
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
                    if is_dir { FileType::Directory } else { FileType::RegularFile },
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

        if let Ok(file_data) = self.fs.lock().unwrap().read_file_chunk(data.start_cluster, offset as u64, max_size) {
            reply.data(&file_data);
        } else {
            reply.error(Errno::EIO);
        }
    }
}