mod fs;

use std::ffi::OsStr;
use std::time::{Duration, UNIX_EPOCH};

use fuser::experimental::{
    AsyncFilesystem, DirEntListBuilder, GetAttrResponse, LookupResponse, RequestContext, TokioAdapter,
};
use fuser::{Errno, FileAttr, FileHandle, FileType, INodeNo, LockOwner, MountOption, OpenFlags, SessionACL};

const TTL: Duration = Duration::from_secs(1);

const HELLO_DIR_ATTR: FileAttr = FileAttr {
    ino: INodeNo::ROOT,
    size: 0,
    blocks: 0,
    atime: UNIX_EPOCH,
    mtime: UNIX_EPOCH,
    ctime: UNIX_EPOCH,
    crtime: UNIX_EPOCH,
    kind: FileType::Directory,
    perm: 0o755,
    nlink: 2,
    uid: 501,
    gid: 20,
    rdev: 0,
    flags: 0,
    blksize: 512,
};

const HELLO_TXT_CONTENT: &[u8] = b"Hello, async world!\n";

const HELLO_TXT_ATTR: FileAttr = FileAttr {
    ino: INodeNo(2),
    size: 20, // Exact byte length of HELLO_TXT_CONTENT
    blocks: 1,
    atime: UNIX_EPOCH,
    mtime: UNIX_EPOCH,
    ctime: UNIX_EPOCH,
    crtime: UNIX_EPOCH,
    kind: FileType::RegularFile,
    perm: 0o444,
    nlink: 1,
    uid: 501,
    gid: 20,
    rdev: 0,
    flags: 0,
    blksize: 512,
};

struct GithubFS;

#[async_trait::async_trait]
impl AsyncFilesystem for GithubFS {
    async fn lookup(
        &self,
        _context: &RequestContext,
        parent: INodeNo,
        name: &OsStr,
    ) -> fuser::experimental::Result<LookupResponse> {
        if parent == INodeNo::ROOT && name.to_str() == Some("hello.txt") {
            Ok(LookupResponse::new(TTL, HELLO_TXT_ATTR, fuser::Generation(0)))
        } else {
            Err(Errno::ENOENT)
        }
    }

    async fn getattr(
        &self,
        _context: &RequestContext,
        ino: INodeNo,
        _file_handle: Option<FileHandle>,
    ) -> fuser::experimental::Result<GetAttrResponse> {
        match ino.0 {
            1 => Ok(GetAttrResponse::new(TTL, HELLO_DIR_ATTR)),
            2 => Ok(GetAttrResponse::new(TTL, HELLO_TXT_ATTR)),
            _ => Err(Errno::ENOENT),
        }
    }

    async fn read(
        &self,
        _context: &RequestContext,
        ino: INodeNo,
        _file_handle: FileHandle,
        offset: u64,
        _size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        out_data: &mut Vec<u8>,
    ) -> fuser::experimental::Result<()> {
        if ino.0 == 2 {
            let offset = offset as usize;
            if offset < HELLO_TXT_CONTENT.len() {
                out_data.extend_from_slice(&HELLO_TXT_CONTENT[offset..]);
            }
            Ok(())
        } else {
            Err(Errno::ENOENT)
        }
    }

    async fn readdir(
        &self,
        _context: &RequestContext,
        ino: INodeNo,
        _file_handle: FileHandle,
        offset: u64,
        mut builder: DirEntListBuilder<'_>,
    ) -> fuser::experimental::Result<()> {
        if ino != INodeNo::ROOT {
            return Err(Errno::ENOENT);
        }

        let entries = vec![
            (1, FileType::Directory, "."),
            (1, FileType::Directory, ".."),
            (2, FileType::RegularFile, "hello.txt"),
        ];

        for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
            // builder.add returns true if the kernel buffer is full
            if builder.add(INodeNo(entry.0), (i + 1) as u64, entry.1, entry.2) {
                break;
            }
        }
        Ok(())
    }
}

fn main() {
    env_logger::init();
    let mountpoint = "/tmp/github";
    
    std::fs::create_dir_all(mountpoint).unwrap();

    let mut config = fuser::Config::default();
    config.mount_options.push(MountOption::RO);
    config.mount_options.push(MountOption::FSName("hello".to_string()));
    config.mount_options.push(MountOption::AutoUnmount);
    config.acl = SessionACL::RootAndOwner;


    fs::run();

    // fuser::mount2(TokioAdapter::new(GithubFS), mountpoint, &config).unwrap();
}
