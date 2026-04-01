use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;

use std::ffi::OsStr;
use std::time::{Duration, UNIX_EPOCH};

use fuser::experimental::{
    AsyncFilesystem, DirEntListBuilder, GetAttrResponse, LookupResponse, RequestContext,
};

use fuser::{Errno, FileAttr, FileHandle, FileType, INodeNo, LockOwner, OpenFlags};

const TTL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct File {
    size: u64,
    url: String,
}

#[derive(Debug, Clone)]
struct Dir {
    files: HashMap<String, u64>,
    name: String,
}

#[derive(Debug, Clone)]
enum Node {
    File(File),
    Dir(Dir),
}

impl Node {
    fn attr(&self, ino: u64) -> FileAttr {
        let ino = INodeNo(ino);
        match self {
            Node::Dir(_) => FileAttr {
                ino,
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
            },
            Node::File(file) => {
                let size = file.size;
                FileAttr {
                    ino,
                    size,
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
                }
            }
        }
    }

    fn content(&self) -> Option<String> {
        match self {
            Self::Dir(_) => None,
            Self::File(file) => {
                let string = "Test".to_string();
                Some(string)
            }
        }
    }
}

pub struct GithubFS {
    nodes: HashMap<u64, Node>,
    needle: u64,
}

impl GithubFS {
    const ROOT: u64 = 1;
    fn find(&self, parent: u64, path_string: String) -> Option<u64> {
        let mut needle = parent;
        for file in path_string.split('/') {
            let Some(Node::Dir(dir)) = self.nodes.get(&needle) else {
                return None;
            };
            needle = *dir.files.get(file)?;
        }
        Some(needle)
    }

    fn get_parent_and_node(&self, parent: u64, path_string: String) -> Option<(u64, String)> {
        let Some((dir_path, file_name)) = path_string.rsplit_once('/') else {
            return Some((parent, path_string));
        };
        self.find(parent, dir_path.into())
            .map(|parent| (parent, String::from(file_name)))
    }

    pub fn new() -> Self {
        let mut nodes = HashMap::new();
        let name = String::from("/tmp/github");
        let root = Dir {
            name,
            files: HashMap::new(),
        };
        let root = Node::Dir(root);
        nodes.insert(1, root);
        Self { nodes, needle: 1 }
    }

    fn insert(&mut self, node: Node) -> u64 {
        self.needle += 1;
        self.nodes.insert(self.needle, node);
        self.needle
    }

    fn add_dir(&mut self, parent: u64, dir_name: String, dir: Dir) -> u64 {
        let node = Node::Dir(dir);
        let dir = self.insert(node);
        let Some(Node::Dir(parent)) = self.nodes.get_mut(&parent) else {
            return 0; // not posssible
        };
        parent.files.insert(dir_name, dir);
        self.needle
    }

    fn new_dir(&mut self, parent: u64, dir_name: String) -> u64 {
        let name = dir_name.clone();
        let dir = Dir {
            name,
            files: HashMap::new(),
        };
        self.add_dir(parent, dir_name, dir)
    }

    fn add_file(&mut self, parent: u64, file_name: String, file: File) -> u64 {
        let node = Node::File(file);
        let file = self.insert(node);
        let Some(Node::Dir(parent)) = self.nodes.get_mut(&parent) else {
            return 0; // not posssible
        };
        parent.files.insert(file_name, file);
        file
    }

    pub fn add_repo(&mut self, response: GithubTreeResponse, author: String, repo: String) {
        let author = self.new_dir(1, author);
        let repo = self.new_dir(author, repo);
        for node in response.tree {
            match node.node_type {
                GithubNodeType::Blob => {
                    let size = node.size.unwrap();
                    let file_path = node.path;
                    let (parent, file_name) = self.get_parent_and_node(repo, file_path).unwrap();
                    let url = node.url;
                    let file = File { size, url, };
                    self.add_file(parent, file_name, file);
                }
                GithubNodeType::Tree => {
                    let file_path = node.path;
                    let (parent, dir_name) = self.get_parent_and_node(repo, file_path).unwrap();
                    self.new_dir(parent, dir_name);
                }
            }
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum GithubNodeType {
    Blob,
    Tree,
}

#[derive(Debug, Deserialize)]
pub struct GithubNode {
    pub path: String,
    #[serde(rename = "type")]
    pub node_type: GithubNodeType,
    pub size: Option<u64>,
    pub url: String,
}

#[derive(Deserialize)]
pub struct GithubTreeResponse {
    pub sha: String,
    pub url: String,
    pub tree: Vec<GithubNode>,
}

pub fn run() {
    let response = include_str!("../a.json");
    let payload: GithubTreeResponse = serde_json::from_str(&response).unwrap();

    let mut ghfs = GithubFS::new();
    ghfs.add_repo(payload, "mTvare6".into(), "hello-world.rs".into());

    println!("{:?}", ghfs.nodes);
}

#[async_trait::async_trait]
impl AsyncFilesystem for GithubFS {
    async fn lookup(
        &self,
        _context: &RequestContext,
        parent: INodeNo,
        name: &OsStr,
    ) -> fuser::experimental::Result<LookupResponse> {
        let Some(Node::Dir(dir)) = self.nodes.get(&parent.0) else {
            return Err(Errno::ENOENT);
        };

        let name_str = name.to_str().ok_or(Errno::EINVAL)?;
        let file_ino = dir.files.get(name_str).ok_or(Errno::ENOENT)?;

        let node = self.nodes.get(file_ino).ok_or(Errno::ENOENT)?;
        let attr = node.attr(*file_ino);

        Ok(LookupResponse::new(TTL, attr, fuser::Generation(0)))
    }

    async fn getattr(
        &self,
        _context: &RequestContext,
        ino: INodeNo,
        _file_handle: Option<FileHandle>,
    ) -> fuser::experimental::Result<GetAttrResponse> {
        let Some(node) = self.nodes.get(&ino.0) else {
            return Err(Errno::ENOENT);
        };

        let attr = node.attr(ino.0);
        Ok(GetAttrResponse::new(TTL, attr))
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
        let Some(node) = self.nodes.get(&ino.0) else {
            return Err(Errno::ENOENT);
        };

        let content = node.content().ok_or(Errno::EINVAL)?;
        let content = content.as_bytes();
        let offset = offset as usize;
        if offset < content.len() {
            out_data.extend_from_slice(&content[offset..]);
        }
        Ok(())
    }

    async fn readdir(
        &self,
        _context: &RequestContext,
        ino: INodeNo,
        _file_handle: FileHandle,
        offset: u64,
        mut builder: DirEntListBuilder<'_>,
    ) -> fuser::experimental::Result<()> {
        let Some(Node::Dir(dir)) = self.nodes.get(&ino.0) else {
            return Err(Errno::ENOTDIR);
        };

        let mut entries = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (1, FileType::Directory, "..".to_string()), // hardcoding parent to root
        ];

        for (child_name, child_ino) in &dir.files {
            let child_node = self.nodes.get(child_ino).unwrap();
            let kind = match child_node {
                Node::Dir(_) => FileType::Directory,
                Node::File(_) => FileType::RegularFile,
            };
            entries.push((*child_ino, kind, child_name.clone()));
        }

        for (i, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if builder.add(INodeNo(child_ino), (i + 1) as u64, kind, name) {
                break;
            }
        }

        Ok(())
    }
}
