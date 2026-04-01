use reqwest::{Client, ClientBuilder};
use serde::Deserialize;
use std::collections::HashMap;

use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::{OnceCell, RwLock};

use fuser::experimental::{
    AsyncFilesystem, DirEntListBuilder, GetAttrResponse, LookupResponse, RequestContext,
};

use fuser::{Errno, FileAttr, FileHandle, FileType, INodeNo, LockOwner, OpenFlags};

const TTL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct File {
    size: u64,
    url: String,
    cache: Arc<OnceCell<Vec<u8>>>,
}

#[derive(Debug, Clone)]
struct Dir {
    files: HashMap<String, u64>,
    name: String,
    kind: DirKind,
    parent: u64,
    hydrated: bool,
}

#[derive(Debug, Clone)]
enum DirKind {
    Root,
    Standard,
    Repo { owner: String, branch: String },
    User,
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
}

impl File {
    async fn content(&self, client: &Client) -> Result<&[u8], Errno> {
        let bytes = self
            .cache
            .get_or_try_init(|| async {
                let response = client
                    .get(&self.url)
                    .header("Accept", "application/vnd.github.v3.raw")
                    .send()
                    .await
                    .map_err(|_| Errno::EIO)?;

                if !response.status().is_success() {
                    return Err(Errno::EIO);
                }

                let bytes = response.bytes().await.map_err(|_| Errno::EIO)?;
                Ok(bytes.to_vec())
            })
            .await?;

        Ok(bytes.as_slice())
    }
}

pub struct GithubFS {
    nodes: Arc<RwLock<HashMap<u64, Node>>>,
    needle: Arc<AtomicU64>,
    client: Client,
}

impl GithubFS {
    const ROOT: u64 = 1;

    pub fn new() -> Self {
        let mut nodes = HashMap::new();
        let name = String::from("/tmp/github");
        let kind = DirKind::Root;
        let parent = Self::ROOT;

        let root = Dir {
            name,
            kind,
            parent,
            files: HashMap::new(),
            hydrated: true, // never checked
        };

        let root = Node::Dir(root);
        nodes.insert(Self::ROOT, root);

        let nodes = Arc::new(RwLock::new(nodes));
        let needle = Arc::new(AtomicU64::new(1));

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("User-Agent", "rust-ghfs".parse().unwrap());

        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            let auth_value = format!("Bearer {}", token);
            headers.insert("Authorization", auth_value.parse().unwrap());
            println!("Using github token: {}", token);
        }

        let client = ClientBuilder::new()
            .default_headers(headers)
            .build()
            .unwrap();

        Self {
            nodes,
            needle,
            client,
        }
    }

    async fn find(&self, parent: u64, path_string: String) -> Option<u64> {
        let mut needle = parent;
        let map = self.nodes.read().await;
        for file in path_string.split('/') {
            let Some(Node::Dir(dir)) = map.get(&needle) else {
                return None;
            };
            needle = *dir.files.get(file)?;
        }
        Some(needle)
    }

    async fn get_parent_and_node(&self, parent: u64, path_string: String) -> Option<(u64, String)> {
        let Some((dir_path, file_name)) = path_string.rsplit_once('/') else {
            return Some((parent, path_string));
        };
        self.find(parent, dir_path.into())
            .await
            .map(|parent| (parent, String::from(file_name)))
    }

    async fn insert(&self, node: Node) -> u64 {
        let id = self.needle.fetch_add(1, Ordering::Relaxed) + 1;

        let mut map_lock = self.nodes.write().await;
        map_lock.insert(id, node);

        id
    }

    async fn add_dir(&self, parent: u64, dir_name: String, dir: Dir) -> u64 {
        let node = Node::Dir(dir);
        let dir = self.insert(node).await;

        let mut map = self.nodes.write().await;
        let Some(Node::Dir(parent)) = map.get_mut(&parent) else {
            return 0; // not posssible
        };

        parent.files.insert(dir_name, dir);
        dir
    }

    async fn new_dir(&self, parent: u64, dir_name: String, kind: DirKind, hydrated: bool) -> u64 {
        let name = dir_name.clone();
        let dir = Dir {
            name,
            parent,
            kind,
            files: HashMap::new(),
            hydrated,
        };

        self.add_dir(parent, dir_name, dir).await
    }

    async fn add_file(&self, parent: u64, file_name: String, file: File) -> u64 {
        let node = Node::File(file);
        let file = self.insert(node).await;

        let mut map = self.nodes.write().await;
        let Some(Node::Dir(parent)) = map.get_mut(&parent) else {
            return 0; // not posssible
        };

        parent.files.insert(file_name, file);
        file
    }

    pub async fn add_repo(&self, response: GithubTreeResponse, repo: u64) {
        for node in response.tree {
            match node.node_type {
                GithubNodeType::Blob => {
                    let size = node.size.unwrap();
                    let file_path = node.path;
                    let (parent, file_name) =
                        self.get_parent_and_node(repo, file_path).await.unwrap();
                    let url = node.url;
                    let cache = Arc::new(OnceCell::new());
                    let file = File { size, url, cache };
                    self.add_file(parent, file_name, file).await;
                }
                GithubNodeType::Tree => {
                    let file_path = node.path;
                    let (parent, dir_name) =
                        self.get_parent_and_node(repo, file_path).await.unwrap();
                    self.new_dir(parent, dir_name, DirKind::Standard, true)
                        .await;
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
    pub tree: Vec<GithubNode>,
}

#[derive(Deserialize)]
pub struct GithubRepoResponse {
    pub name: String,
    pub owner: GithubOwner,
    #[serde(rename = "default_branch")]
    pub branch: String,
}

#[derive(Deserialize)]
pub struct GithubOwner {
    pub login: String,
}

#[async_trait::async_trait]
impl AsyncFilesystem for GithubFS {
    async fn lookup(
        &self,
        _context: &RequestContext,
        parent: INodeNo,
        name: &OsStr,
    ) -> fuser::experimental::Result<LookupResponse> {
        let name_str = name.to_str().ok_or(Errno::EINVAL)?;
        {
            let map = self.nodes.read().await;

            let Some(Node::Dir(dir)) = map.get(&parent.0) else {
                return Err(Errno::ENOENT);
            };

            if let Some(file_ino) = dir.files.get(name_str) {
                let node = map.get(file_ino).unwrap();
                let attr = node.attr(*file_ino);

                return Ok(LookupResponse::new(TTL, attr, fuser::Generation(0)));
            }
        }

        if parent.0 == GithubFS::ROOT {
            let user = format!("https://api.github.com/users/{}", name_str);
            let response = self
                .client
                .get(&user)
                .send()
                .await
                .map_err(|_| Errno::EIO)?;

            if response.status().is_success() {
                let new_id = self
                    .new_dir(parent.0, name_str.into(), DirKind::User, false)
                    .await;

                let map = self.nodes.read().await;
                let node = map.get(&new_id).unwrap();

                return Ok(LookupResponse::new(
                    TTL,
                    node.attr(new_id),
                    fuser::Generation(0),
                ));
            } else {
                return Err(Errno::ENOENT);
            }
        }

        Err(Errno::ENOENT)
    }

    async fn getattr(
        &self,
        _context: &RequestContext,
        ino: INodeNo,
        _file_handle: Option<FileHandle>,
    ) -> fuser::experimental::Result<GetAttrResponse> {
        let map = self.nodes.read().await;
        let Some(node) = map.get(&ino.0) else {
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
        let file = {
            let map = self.nodes.read().await;
            match map.get(&ino.0) {
                Some(Node::File(f)) => f.clone(),
                Some(Node::Dir(_)) => return Err(Errno::EISDIR),
                None => return Err(Errno::ENOENT),
            }
        };
        // preventing rwlock from hanging the whole fs when content is being downloaded, so
        // hashmap is lock-free by then

        let content = file.content(&self.client).await?;

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
        let (kind, hydrated, name, parent) = {
            let map = self.nodes.read().await;
            let Some(Node::Dir(dir)) = map.get(&ino.0) else {
                return Err(Errno::ENOTDIR);
            };
            (dir.kind.clone(), dir.hydrated, dir.name.clone(), dir.parent)
        };

        if !hydrated {
            match kind {
                DirKind::User => {
                    let url = format!("https://api.github.com/users/{}/repos?per_page=100", name);
                    let response = self.client.get(&url).send().await.map_err(|_| Errno::EIO)?;
                    let repos: Vec<GithubRepoResponse> =
                        response.json().await.map_err(|_| Errno::EIO)?;

                    for repo in repos {
                        let kind = DirKind::Repo {
                            owner: repo.owner.login,
                            branch: repo.branch,
                        };
                        let name = repo.name;
                        self.new_dir(ino.0, name, kind, false).await;
                    }
                }
                DirKind::Repo { owner, branch } => {
                    let url = format!(
                        "https://api.github.com/repos/{}/{}/git/trees/{}?recursive=1",
                        owner, name, branch
                    );
                    let response = self.client.get(&url).send().await.map_err(|_| Errno::EIO)?;
                    let tree: GithubTreeResponse = response.json().await.map_err(|_| Errno::EIO)?;
                    self.add_repo(tree, ino.0).await;
                }
                _ => {}
            }

            let mut mmap = self.nodes.write().await;
            if let Some(Node::Dir(dir)) = mmap.get_mut(&ino.0) {
                dir.hydrated = true;
            }
        }

        let mut entries = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (parent, FileType::Directory, "..".to_string()), // hardcoding parent to root
        ];

        let map = self.nodes.read().await;
        let Some(Node::Dir(dir)) = map.get(&ino.0) else {
            return Err(Errno::ENOTDIR);
        };

        for (child_name, child_ino) in &dir.files {
            let child_node = map.get(child_ino).unwrap();
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
