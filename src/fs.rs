use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Client, ClientBuilder, Url};
use serde::Deserialize;
use std::collections::HashMap;

use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, OnceCell, RwLock};

use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

const TTL: Duration = Duration::from_secs(1);

// SAFETY: very safe
fn getuid() -> u32 {
    unsafe { libc::getuid() }
}

// SAFETY: very safe
fn getgid() -> u32 {
    unsafe { libc::getgid() }
}

#[derive(Debug, Clone)]
struct File {
    size: u64,
    url: String,
    cache: Arc<OnceCell<Vec<u8>>>,
    sha: Option<String>,
    dirty: bool,
    name: String,
    parent: u64,
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
        let (size, blocks, kind, perm, nlink) = match self {
            Node::Dir(_) => (0, 0, FileType::Directory, 0o755, 2),
            Node::File(file) => (file.size, 1, FileType::RegularFile, 0o644, 1),
        };

        FileAttr {
            ino,
            size,
            blocks,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            perm,
            nlink,
            uid: getuid(),
            gid: getgid(),
            rdev: 0,
            flags: 0,
            blksize: 512,
        }
    }
}

impl File {
    async fn content(&self, client: &Client) -> Result<&[u8], Errno> {
        let bytes = self
            .cache
            .get_or_try_init(|| async {
                let request = client
                    .get(&self.url)
                    .header("Accept", "application/vnd.github.v3.raw");
                let response = request.send().await.map_err(|_| Errno::EIO)?;

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
    nodes: RwLock<HashMap<u64, Node>>,
    needle: AtomicU64,
    client: Client,
    login: String,
    operations: Mutex<()>,
}

impl GithubFS {
    const ROOT: u64 = 1;

    pub async fn new() -> Self {
        let mut nodes = HashMap::new();
        let name = String::from("/tmp/github");
        let kind = DirKind::Root;
        let parent = Self::ROOT;

        let root = Dir {
            name,
            kind,
            parent,
            files: HashMap::new(),
            hydrated: true,
        };

        let root = Node::Dir(root);
        nodes.insert(Self::ROOT, root);

        let nodes = RwLock::new(nodes);
        let needle = AtomicU64::new(1);

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("User-Agent", "ghfs".parse().unwrap());

        let token = std::env::var("GITHUB_TOKEN").expect("Set GITHUB_TOKEN to access Github");
        let auth_value = format!("Bearer {}", token);
        let mut auth_value = reqwest::header::HeaderValue::from_str(&auth_value).unwrap();
        auth_value.set_sensitive(true);
        headers.insert("Authorization", auth_value);

        let builder = ClientBuilder::new().default_headers(headers);
        let client = builder.build().unwrap();

        let request = client.get("https://api.github.com/user");
        let response = request.send().await.unwrap();
        let response = response.error_for_status().unwrap();
        let user: GithubOwner = response.json().await.unwrap();

        let fs = Self {
            nodes,
            needle,
            client,
            login: user.login.clone(),
            operations: Mutex::new(()),
        };
        fs.new_dir(Self::ROOT, user.login, DirKind::User, false)
            .await;
        fs
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

    async fn insert(&self, parent: u64, name: String, node: Node) -> u64 {
        let id = self.needle.fetch_add(1, Ordering::Relaxed) + 1;
        let mut map = self.nodes.write().await;
        let Some(Node::Dir(parent)) = map.get_mut(&parent) else {
            return 0; // not posssible
        };
        parent.files.insert(name, id);
        map.insert(id, node);
        id
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

        self.insert(parent, dir_name, Node::Dir(dir)).await
    }

    pub async fn add_repo(&self, mut response: GithubTreeResponse, repo: u64) {
        response.tree.sort_by(|a, b| a.path.cmp(&b.path));

        for node in response.tree {
            match node.node_type {
                GithubNodeType::Blob => {
                    let size = node.size.unwrap();
                    let file_path = node.path;
                    let (parent, file_name) =
                        self.get_parent_and_node(repo, file_path).await.unwrap();
                    let url = node.url;
                    let cache = Arc::new(OnceCell::new());
                    let file = File {
                        size,
                        url,
                        cache,
                        sha: Some(node.sha),
                        dirty: false,
                        name: file_name.clone(),
                        parent,
                    };
                    self.insert(parent, file_name, Node::File(file)).await;
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
    pub sha: String,
    pub mode: String,
}

#[derive(Deserialize)]
pub struct GithubTreeResponse {
    pub tree: Vec<GithubNode>,
    pub truncated: bool,
}

#[derive(Deserialize)]
pub struct GithubRepoResponse {
    pub name: String,
    pub owner: GithubOwner,
    #[serde(rename = "default_branch")]
    pub branch: String,
    pub size: u64,
}

#[derive(Deserialize)]
pub struct GithubOwner {
    pub login: String,
}

impl GithubFS {
    async fn lookup(&self, parent: INodeNo, name: &OsStr) -> Result<FileAttr, Errno> {
        self.hydrate(parent).await?;
        let name_str = name.to_str().ok_or(Errno::EINVAL)?;
        {
            let map = self.nodes.read().await;

            let Some(Node::Dir(dir)) = map.get(&parent.0) else {
                return Err(Errno::ENOENT);
            };

            if let Some(file_ino) = dir.files.get(name_str) {
                let node = map.get(file_ino).unwrap();
                let attr = node.attr(*file_ino);

                return Ok(attr);
            }
        }

        if parent.0 == GithubFS::ROOT {
            let user = format!("https://api.github.com/users/{}", name_str);
            let request = self.client.get(&user);
            let response = request.send().await.map_err(|_| Errno::EIO)?;

            if response.status().is_success() {
                let new_id = self
                    .new_dir(parent.0, name_str.into(), DirKind::User, false)
                    .await;

                let map = self.nodes.read().await;
                let node = map.get(&new_id).unwrap();

                return Ok(node.attr(new_id));
            } else {
                return Err(Errno::ENOENT);
            }
        }

        Err(Errno::ENOENT)
    }

    async fn getattr(&self, ino: INodeNo) -> Result<FileAttr, Errno> {
        let map = self.nodes.read().await;
        let Some(node) = map.get(&ino.0) else {
            return Err(Errno::ENOENT);
        };

        let attr = node.attr(ino.0);
        Ok(attr)
    }

    async fn read(
        &self,
        ino: INodeNo,
        offset: u64,
        size: u32,
        out_data: &mut Vec<u8>,
    ) -> Result<(), Errno> {
        let file = self.file(ino).await?;
        let content = file.content(&self.client).await?;

        let offset = offset as usize;
        if offset < content.len() {
            let end = offset.saturating_add(size as usize).min(content.len());
            out_data.extend_from_slice(&content[offset..end]);
        }

        Ok(())
    }

    async fn readdir(&self, ino: INodeNo) -> Result<Vec<(u64, FileType, String)>, Errno> {
        self.hydrate(ino).await?;
        let map = self.nodes.read().await;
        let Some(Node::Dir(dir)) = map.get(&ino.0) else {
            return Err(Errno::ENOTDIR);
        };
        let mut entries = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (dir.parent, FileType::Directory, "..".to_string()),
        ];

        for (child_name, child_ino) in &dir.files {
            let child_node = map.get(child_ino).unwrap();
            let kind = match child_node {
                Node::Dir(_) => FileType::Directory,
                Node::File(_) => FileType::RegularFile,
            };
            entries.push((*child_ino, kind, child_name.clone()));
        }

        entries[2..].sort_by(|a, b| a.2.cmp(&b.2));
        Ok(entries)
    }

    async fn hydrate(&self, ino: INodeNo) -> Result<(), Errno> {
        let _guard = self.operations.lock().await;
        let (kind, hydrated, name) = {
            let map = self.nodes.read().await;
            let Some(Node::Dir(dir)) = map.get(&ino.0) else {
                return Err(Errno::ENOTDIR);
            };
            (dir.kind.clone(), dir.hydrated, dir.name.clone())
        };

        if !hydrated {
            match kind {
                DirKind::User => {
                    let url = if name.eq_ignore_ascii_case(&self.login) {
                        String::from(
                            "https://api.github.com/user/repos?affiliation=owner&per_page=100",
                        )
                    } else {
                        format!("https://api.github.com/users/{}/repos?per_page=100", name)
                    };
                    let mut page = 1;
                    loop {
                        let request = self.client.get(&url).query(&[("page", page)]);
                        let response = request.send().await.map_err(|_| Errno::EIO)?;
                        let response = response.error_for_status().map_err(|_| Errno::EIO)?;
                        let repos: Vec<GithubRepoResponse> =
                            response.json().await.map_err(|_| Errno::EIO)?;
                        let done = repos.len() < 100;
                        for repo in repos {
                            let owner = repo.owner.login;
                            let branch = repo.branch;
                            let kind = DirKind::Repo { owner, branch };
                            let name = repo.name;
                            self.new_dir(ino.0, name, kind, false).await;
                        }
                        if done {
                            break;
                        }
                        page += 1;
                    }
                }
                DirKind::Repo { owner, branch } => {
                    let mut url = Url::parse("https://api.github.com/repos/").unwrap();
                    url.path_segments_mut()
                        .unwrap()
                        .pop_if_empty()
                        .extend([&owner, &name, "git", "trees", &branch]);
                    url.set_query(Some("recursive=1"));
                    let response = self.client.get(url).send().await.map_err(|_| Errno::EIO)?;
                    if response.status() == reqwest::StatusCode::CONFLICT {
                        let url = format!("https://api.github.com/repos/{}/{}", owner, name);
                        let request = self.client.get(url);
                        let response = request.send().await.map_err(|_| Errno::EIO)?;
                        let response = response.error_for_status().map_err(|_| Errno::EIO)?;
                        let repo: GithubRepoResponse =
                            response.json().await.map_err(|_| Errno::EIO)?;

                        if repo.size != 0 {
                            return Err(Errno::EIO);
                        }
                    } else {
                        let response = response.error_for_status().map_err(|_| Errno::EIO)?;
                        let tree: GithubTreeResponse =
                            response.json().await.map_err(|_| Errno::EIO)?;

                        if tree.truncated {
                            return Err(Errno::EFBIG);
                        }
                        self.add_repo(tree, ino.0).await;
                    }
                }
                _ => {}
            }

            let mut mmap = self.nodes.write().await;
            if let Some(Node::Dir(dir)) = mmap.get_mut(&ino.0) {
                dir.hydrated = true;
            }
        }

        Ok(())
    }

    async fn file(&self, ino: INodeNo) -> Result<File, Errno> {
        let map = self.nodes.read().await;
        match map.get(&ino.0) {
            Some(Node::File(file)) => Ok(file.clone()),
            Some(Node::Dir(_)) => Err(Errno::EISDIR),
            None => Err(Errno::ENOENT),
        }
    }

    async fn repo_path(&self, parent: u64, name: &str) -> Result<(Url, String, String), Errno> {
        let map = self.nodes.read().await;
        let mut needle = parent;
        let mut path = vec![name.to_string()];

        loop {
            let Some(Node::Dir(dir)) = map.get(&needle) else {
                return Err(Errno::ENOTDIR);
            };

            if let DirKind::Repo { owner, branch } = &dir.kind {
                path.reverse();
                let mut url = Url::parse("https://api.github.com/repos/").unwrap();
                let path = [owner.as_str(), &dir.name, ""];
                url.path_segments_mut().unwrap().pop_if_empty().extend(path);
                return Ok((url, branch.clone(), path.join("/")));
            }

            if !matches!(dir.kind, DirKind::Standard) {
                return Err(Errno::EACCES);
            }

            path.push(dir.name.clone());
            needle = dir.parent;
        }
    }

    async fn contents_url(&self, parent: u64, name: &str) -> Result<(Url, String), Errno> {
        let (mut url, branch, path) = self.repo_path(parent, name).await?;

        url.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .push("contents")
            .extend(path.split('/'));

        Ok((url, branch))
    }

    async fn push(
        &self,
        parent: u64,
        name: &str,
        sha: Option<&str>,
        content: &[u8],
    ) -> Result<GithubFileResponse, Errno> {
        let (url, branch) = self.contents_url(parent, name).await?;
        let action = if sha.is_some() { "update" } else { "add" };

        let mut body = serde_json::json!({
            "message": format!("{}: {}", action, name),
            "content": STANDARD.encode(content),
            "branch": branch,
        });

        if let Some(sha) = sha {
            body["sha"] = serde_json::json!(sha);
        }

        let request = self.client.put(url).json(&body);
        let response = request.send().await.map_err(|_| Errno::EIO)?;

        if !response.status().is_success() {
            return Err(Errno::EIO);
        }

        let response: GithubWriteResponse = response.json().await.map_err(|_| Errno::EIO)?;
        Ok(response.content)
    }

    async fn mkdir(&self, parent: INodeNo, name: &OsStr) -> Result<FileAttr, Errno> {
        self.hydrate(parent).await?;
        let _guard = self.operations.lock().await;
        let name = name.to_str().ok_or(Errno::EINVAL)?;

        let dir = {
            let map = self.nodes.read().await;
            let Some(Node::Dir(dir)) = map.get(&parent.0) else {
                return Err(Errno::ENOTDIR);
            };
            if dir.files.contains_key(name) {
                return Err(Errno::EEXIST);
            }
            dir.clone()
        };

        let kind = match dir.kind {
            // todo: does ignore ascii work fine across unicode names
            DirKind::User if dir.name.eq_ignore_ascii_case(&self.login) => {
                let request = self
                    .client
                    .post("https://api.github.com/user/repos")
                    .json(&serde_json::json!({"name": name}));
                let response = request.send().await.map_err(|_| Errno::EIO)?;
                let response = response.error_for_status().map_err(|_| Errno::EIO)?;

                let repo: GithubRepoResponse = response.json().await.map_err(|_| Errno::EIO)?;
                let owner = repo.owner.login;
                let branch = repo.branch;
                DirKind::Repo { owner, branch }
            }
            DirKind::Repo { .. } | DirKind::Standard => DirKind::Standard,
            _ => return Err(Errno::EACCES),
        };

        // Git tracks nested directories once they contain a file
        let ino = self.new_dir(parent.0, name.into(), kind, true).await;
        let map = self.nodes.read().await;
        Ok(map.get(&ino).unwrap().attr(ino))
    }

    async fn create(&self, parent: INodeNo, name: &OsStr) -> Result<FileAttr, Errno> {
        self.hydrate(parent).await?;
        let _guard = self.operations.lock().await;
        let name = name.to_str().ok_or(Errno::EINVAL)?;

        {
            let map = self.nodes.read().await;
            let Some(Node::Dir(dir)) = map.get(&parent.0) else {
                return Err(Errno::ENOTDIR);
            };
            if !matches!(dir.kind, DirKind::Repo { .. } | DirKind::Standard) {
                return Err(Errno::EACCES);
            }
            if dir.files.contains_key(name) {
                return Err(Errno::EEXIST);
            }
        }

        let file = File {
            size: 0,
            url: String::new(),
            sha: None,
            dirty: true,
            cache: Arc::new(OnceCell::new_with(Some(Vec::new()))),
            name: name.into(),
            parent: parent.0,
        };

        let ino = self.insert(parent.0, name.into(), Node::File(file)).await;
        let map = self.nodes.read().await;
        Ok(map.get(&ino).unwrap().attr(ino))
    }

    async fn save(
        &self,
        ino: INodeNo,
        mut file: File,
        content: Vec<u8>,
    ) -> Result<FileAttr, Errno> {
        let linked = {
            let map = self.nodes.read().await;
            matches!(map.get(&file.parent), Some(Node::Dir(dir))
                if dir.files.get(&file.name) == Some(&ino.0))
        };

        if linked {
            let response = self
                .push(file.parent, &file.name, file.sha.as_deref(), &content)
                .await?;
            file.sha = Some(response.sha);
            file.url = response.git_url;
        }

        file.size = content.len() as u64;
        file.dirty = false;
        file.cache = Arc::new(OnceCell::new_with(Some(content)));

        let node = Node::File(file);
        let attr = node.attr(ino.0);
        let mut map = self.nodes.write().await;

        map.insert(ino.0, node);
        Ok(attr)
    }

    async fn write(&self, ino: INodeNo, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        let _guard = self.operations.lock().await;
        let file = self.file(ino).await?;

        if data.is_empty() {
            return Ok(0);
        }

        let offset = usize::try_from(offset).map_err(|_| Errno::EFBIG)?;
        let end = offset.checked_add(data.len()).ok_or(Errno::EFBIG)?;
        let mut content = file.content(&self.client).await?.to_vec();

        if end > content.len() {
            content
                .try_reserve(end - content.len())
                .map_err(|_| Errno::EFBIG)?;
            content.resize(end, 0);
        }
        content[offset..end].copy_from_slice(data);
        self.save(ino, file, content).await?;
        Ok(data.len() as u32)
    }

    async fn truncate(&self, ino: INodeNo, size: u64) -> Result<FileAttr, Errno> {
        let _guard = self.operations.lock().await;
        let mut file = self.file(ino).await?;
        let size = usize::try_from(size).map_err(|_| Errno::EFBIG)?;

        let mut content = if size == 0 {
            Vec::new()
        } else {
            file.content(&self.client).await?.to_vec()
        };

        if size > content.len() {
            content
                .try_reserve(size - content.len())
                .map_err(|_| Errno::EFBIG)?;
        }

        content.resize(size, 0);
        file.size = size as u64;
        file.cache = Arc::new(OnceCell::new_with(Some(content)));
        file.dirty = true;

        let node = Node::File(file);
        let attr = node.attr(ino.0);
        let mut map = self.nodes.write().await;
        map.insert(ino.0, node);
        Ok(attr)
    }

    async fn rename(
        &self,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
    ) -> Result<(), Errno> {
        self.hydrate(parent).await?;
        self.hydrate(newparent).await?;

        let _guard = self.operations.lock().await;
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        let newname = newname.to_str().ok_or(Errno::EINVAL)?;

        let (ino, mut node, target) = {
            let map = self.nodes.read().await;
            let Some(Node::Dir(dir)) = map.get(&parent.0) else {
                return Err(Errno::ENOTDIR);
            };
            let ino = *dir.files.get(name).ok_or(Errno::ENOENT)?;
            let node = map.get(&ino).ok_or(Errno::ENOENT)?.clone();
            let Some(Node::Dir(dir)) = map.get(&newparent.0) else {
                return Err(Errno::ENOTDIR);
            };
            let target = dir.files.get(newname).and_then(|ino| map.get(ino)).cloned();
            (ino, node, target)
        };

        // the other two aren't supported (atomic/exchange)
        // !empty ~ noreplace
        if !flags.is_empty() {
            #[cfg(target_os = "linux")]
            {
                if flags != RenameFlags::RENAME_NOREPLACE {
                    return Err(Errno::EOPNOTSUPP);
                }
                if target.is_some() {
                    return Err(Errno::EEXIST);
                }
            }

            // mac
            #[cfg(not(target_os = "linux"))]
            return Err(Errno::EOPNOTSUPP);
        }

        let (base, branch, oldpath) = self.repo_path(parent.0, name).await?;
        let (newbase, newbranch, newpath) = self.repo_path(newparent.0, newname).await?;

        if base != newbase || branch != newbranch {
            return Err(Errno::EXDEV);
        }

        if parent == newparent && name == newname {
            return Ok(());
        }

        let directory = matches!(node, Node::Dir(_));
        if let Node::Dir(dir) = &node {
            if !matches!(dir.kind, DirKind::Standard) {
                return Err(Errno::EACCES);
            }
            if newpath.starts_with(&format!("{}/", oldpath)) {
                return Err(Errno::EINVAL);
            }
        }

        match (&node, &target) {
            (Node::File(_), Some(Node::Dir(_))) => return Err(Errno::EISDIR),
            (Node::Dir(_), Some(Node::File(_))) => return Err(Errno::ENOTDIR),
            (_, Some(Node::Dir(dir))) if !dir.files.is_empty() => return Err(Errno::ENOTEMPTY),
            _ => {}
        }

        let persisted = {
            let map = self.nodes.read().await;
            let mut pending = vec![ino];
            let mut persisted = false;
            while let Some(ino) = pending.pop() {
                match map.get(&ino).ok_or(Errno::ENOENT)? {
                    Node::File(file) => persisted |= file.sha.is_some(),
                    Node::Dir(dir) => pending.extend(dir.files.values().copied()),
                }
            }
            persisted
        };

        if !persisted {
            if let Node::File(file) = &mut node {
                let sha = match &target {
                    Some(Node::File(file)) => file.sha.as_deref(),
                    _ => None,
                };
                let content = file.content(&self.client).await?;
                let response = self.push(newparent.0, newname, sha, content).await?;
                file.sha = Some(response.sha);
                file.url = response.git_url;
                file.dirty = false;
            }
        } else {
            let mut reference = base.join("git/ref/heads/").unwrap();
            reference
                .path_segments_mut()
                .unwrap()
                .pop_if_empty()
                .extend(branch.split('/'));

            let request = self.client.get(reference);
            let response = request.send().await.map_err(|_| Errno::EIO)?;
            let response = response.error_for_status().map_err(|_| Errno::EIO)?;

            let head: GithubReference = response.json().await.map_err(|_| Errno::EIO)?;
            let head = head.object.sha;
            let url = base.join(&format!("git/commits/{}", head)).unwrap();
            let request = self.client.get(url);
            let response = request.send().await.map_err(|_| Errno::EIO)?;
            let response = response.error_for_status().map_err(|_| Errno::EIO)?;
            let commit: GithubCommit = response.json().await.map_err(|_| Errno::EIO)?;

            let mut url = base
                .join(&format!("git/trees/{}", commit.tree.sha))
                .unwrap();
            url.set_query(Some("recursive=1"));
            let request = self.client.get(url);
            let response = request.send().await.map_err(|_| Errno::EIO)?;
            let response = response.error_for_status().map_err(|_| Errno::EIO)?;

            let tree: GithubTreeResponse = response.json().await.map_err(|_| Errno::EIO)?;
            if tree.truncated {
                return Err(Errno::EFBIG);
            }

            let source = tree
                .tree
                .iter()
                .find(|entry| entry.path == oldpath)
                .ok_or(Errno::EAGAIN)?;

            let destination = tree.tree.iter().find(|entry| entry.path == newpath);
            let expected = match &target {
                Some(Node::File(file)) => file.sha.as_deref(),
                _ => None,
            };

            if destination.map(|entry| entry.sha.as_str()) != expected {
                return Err(Errno::EAGAIN);
            }

            let mut sha = source.sha.clone();
            if let Node::File(file) = &node {
                if file.sha.as_deref() != Some(source.sha.as_str()) {
                    return Err(Errno::EAGAIN);
                }
                if file.dirty {
                    let content = file.content(&self.client).await?;
                    let blob: GithubObject = self.client.post(base.join("git/blobs").unwrap())
                        .json(&serde_json::json!({"content": STANDARD.encode(content), "encoding": "base64"}))
                        .send().await.map_err(|_| Errno::EIO)?.error_for_status().map_err(|_| Errno::EIO)?
                        .json().await.map_err(|_| Errno::EIO)?;
                    sha = blob.sha;
                }
            }
            let kind = if directory { "tree" } else { "blob" };
            let changes = serde_json::json!([
                {"path": oldpath, "mode": source.mode, "type": kind, "sha": null},
                {"path": newpath, "mode": source.mode, "type": kind, "sha": sha},
            ]);
            let url = base.join("git/trees").unwrap();
            let body = serde_json::json!({"base_tree": commit.tree.sha, "tree": changes});
            let request = self.client.post(url).json(&body);
            let response = request.send().await.map_err(|_| Errno::EIO)?;
            let response = response.error_for_status().map_err(|_| Errno::EIO)?;
            let tree: GithubObject = response.json().await.map_err(|_| Errno::EIO)?;

            let message = if target.is_some() && !directory {
                // olds gone
                format!("update: {}", newpath)
            } else {
                format!("move: {} -> {}", oldpath, newpath)
            };

            let url = base.join("git/commits").unwrap();
            let body = serde_json::json!({"message": message, "tree": tree.sha, "parents": [head]});
            let request = self.client.post(url).json(&body);
            let response = request.send().await.map_err(|_| Errno::EIO)?;
            let response = response.error_for_status().map_err(|_| Errno::EIO)?;

            let commit: GithubObject = response.json().await.map_err(|_| Errno::EIO)?;
            let mut reference = base.join("git/refs/heads/").unwrap();

            reference
                .path_segments_mut()
                .unwrap()
                .pop_if_empty()
                .extend(branch.split('/'));

            let body = serde_json::json!({"sha": commit.sha, "force": false});
            let request = self.client.patch(reference).json(&body);
            let response = request.send().await.map_err(|_| Errno::EIO)?;

            if matches!(
                response.status(),
                reqwest::StatusCode::CONFLICT | reqwest::StatusCode::UNPROCESSABLE_ENTITY
            ) {
                return Err(Errno::EAGAIN);
            }

            response.error_for_status().map_err(|_| Errno::EIO)?;

            if let Node::File(file) = &mut node {
                file.url = base.join(&format!("git/blobs/{}", sha)).unwrap().into();
                file.sha = Some(sha);
                file.dirty = false;
            }
        }

        match &mut node {
            Node::File(file) => {
                file.parent = newparent.0;
                file.name = newname.into();
            }
            Node::Dir(dir) => {
                dir.parent = newparent.0;
                dir.name = newname.into();
            }
        }

        let mut map = self.nodes.write().await;

        let Some(Node::Dir(dir)) = map.get_mut(&parent.0) else {
            return Err(Errno::ENOTDIR);
        };

        dir.files.remove(name);

        let Some(Node::Dir(dir)) = map.get_mut(&newparent.0) else {
            return Err(Errno::ENOTDIR);
        };

        dir.files.insert(newname.into(), ino);
        map.insert(ino, node);
        Ok(())
    }

    async fn unlink(&self, parent: INodeNo, name: &OsStr) -> Result<(), Errno> {
        self.hydrate(parent).await?;
        let _guard = self.operations.lock().await;
        let name = name.to_str().ok_or(Errno::EINVAL)?;

        let ino = {
            let map = self.nodes.read().await;
            let Some(Node::Dir(dir)) = map.get(&parent.0) else {
                return Err(Errno::ENOTDIR);
            };
            *dir.files.get(name).ok_or(Errno::ENOENT)?
        };

        let file = self.file(INodeNo(ino)).await?;

        if let Some(sha) = file.sha {
            let (url, branch) = self.contents_url(parent.0, name).await?;
            let body = serde_json::json!({
                "message": format!("remove: {}", name),
                "sha": sha,
                "branch": branch,
            });
            let request = self.client.delete(url).json(&body);
            let response = request.send().await.map_err(|_| Errno::EIO)?;
            response.error_for_status().map_err(|_| Errno::EIO)?;
        }

        let mut map = self.nodes.write().await;
        let Some(Node::Dir(dir)) = map.get_mut(&parent.0) else {
            return Err(Errno::ENOTDIR);
        };
        dir.files.remove(name);
        // keep the inode for open handles
        // save() checks whether it is still linked
        Ok(())
    }

    async fn rmdir(&self, parent: INodeNo, name: &OsStr) -> Result<(), Errno> {
        self.hydrate(parent).await?;

        let name = name.to_str().ok_or(Errno::EINVAL)?;
        let ino = {
            let map = self.nodes.read().await;
            let Some(Node::Dir(dir)) = map.get(&parent.0) else {
                return Err(Errno::ENOTDIR);
            };
            let ino = *dir.files.get(name).ok_or(Errno::ENOENT)?;
            let Some(Node::Dir(dir)) = map.get(&ino) else {
                return Err(Errno::ENOTDIR);
            };
            if matches!(dir.kind, DirKind::Root | DirKind::User) {
                return Err(Errno::EACCES);
            }
            ino
        };

        self.hydrate(INodeNo(ino)).await?;
        let _guard = self.operations.lock().await;
        let dir = {
            let map = self.nodes.read().await;
            let Some(Node::Dir(dir)) = map.get(&ino) else {
                return Err(Errno::ENOENT);
            };
            if !dir.files.is_empty() {
                return Err(Errno::ENOTEMPTY);
            }
            dir.clone()
        };

        if let DirKind::Repo { owner, .. } = dir.kind {
            let mut url = Url::parse("https://api.github.com/repos/").unwrap();
            url.path_segments_mut()
                .unwrap()
                .pop_if_empty()
                .extend([&owner, &dir.name]);
            let request = self.client.delete(url);
            let response = request.send().await.map_err(|_| Errno::EIO)?;
            response.error_for_status().map_err(|_| Errno::EIO)?;
        }

        let mut map = self.nodes.write().await;
        let Some(Node::Dir(dir)) = map.get_mut(&parent.0) else {
            return Err(Errno::ENOTDIR);
        };
        dir.files.remove(name);
        map.remove(&ino);
        Ok(())
    }

    async fn flush(&self, ino: INodeNo) -> Result<(), Errno> {
        let _guard = self.operations.lock().await;
        let file = self.file(ino).await?;

        if file.dirty {
            let content = file.content(&self.client).await?.to_vec();
            self.save(ino, file, content).await?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct GithubWriteResponse {
    content: GithubFileResponse,
}

#[derive(Deserialize)]
struct GithubFileResponse {
    sha: String,
    git_url: String,
}

#[derive(Deserialize)]
struct GithubObject {
    sha: String,
}

#[derive(Deserialize)]
struct GithubReference {
    object: GithubObject,
}

#[derive(Deserialize)]
struct GithubCommit {
    tree: GithubObject,
}

pub struct TokioAdapter {
    inner: Arc<GithubFS>,
    runtime: tokio::runtime::Runtime,
}

impl TokioAdapter {
    pub fn new() -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let inner = Arc::new(runtime.block_on(GithubFS::new()));

        Self { inner, runtime }
    }
}

impl Filesystem for TokioAdapter {
    fn lookup(&self, _context: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_os_string();
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            match inner.lookup(parent, &name).await {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(error) => reply.error(error),
            }
        });
    }

    fn getattr(
        &self,
        _context: &Request,
        ino: INodeNo,
        _file_handle: Option<FileHandle>,
        reply: ReplyAttr,
    ) {
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            match inner.getattr(ino).await {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(error) => reply.error(error),
            }
        });
    }

    fn read(
        &self,
        _context: &Request,
        ino: INodeNo,
        _file_handle: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            let mut out_data = Vec::new();
            match inner.read(ino, offset, size, &mut out_data).await {
                Ok(()) => reply.data(&out_data),
                Err(error) => reply.error(error),
            }
        });
    }

    fn readdir(
        &self,
        _context: &Request,
        ino: INodeNo,
        _file_handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let inner = self.inner.clone();
        self.runtime.spawn(async move {
            match inner.readdir(ino).await {
                Ok(entries) => {
                    for (i, (child_ino, kind, name)) in
                        entries.into_iter().enumerate().skip(offset as usize)
                    {
                        if reply.add(INodeNo(child_ino), (i + 1) as u64, kind, name) {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(error) => reply.error(error),
            }
        });
    }

    fn open(&self, _context: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::FOPEN_DIRECT_IO);
    }

    fn mkdir(
        &self,
        _context: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name = name.to_os_string();
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            match inner.mkdir(parent, &name).await {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(error) => reply.error(error),
            }
        });
    }

    fn create(
        &self,
        _context: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name = name.to_os_string();
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            match inner.create(parent, &name).await {
                Ok(attr) => reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(0),
                    FopenFlags::FOPEN_DIRECT_IO,
                ),
                Err(error) => reply.error(error),
            }
        });
    }

    fn write(
        &self,
        _context: &Request,
        ino: INodeNo,
        _file_handle: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let data = data.to_vec();
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            match inner.write(ino, offset, &data).await {
                Ok(size) => reply.written(size),
                Err(error) => reply.error(error),
            }
        });
    }

    fn setattr(
        &self,
        _context: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _file_handle: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            let result = async {
                let attr = inner.getattr(ino).await?;
                if mode.is_some_and(|mode| mode & 0o777 != u32::from(attr.perm))
                    || uid.is_some_and(|uid| uid != attr.uid)
                    || gid.is_some_and(|gid| gid != attr.gid)
                {
                    return Err(Errno::EOPNOTSUPP);
                }
                match size {
                    Some(size) => inner.truncate(ino, size).await,
                    None => Ok(attr),
                }
            }
            .await;

            match result {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(error) => reply.error(error),
            }
        });
    }

    fn rename(
        &self,
        _context: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let name = name.to_os_string();
        let newname = newname.to_os_string();
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            match inner
                .rename(parent, &name, newparent, &newname, flags)
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(error),
            }
        });
    }

    fn unlink(&self, _context: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = name.to_os_string();
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            match inner.unlink(parent, &name).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(error),
            }
        });
    }

    fn rmdir(&self, _context: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = name.to_os_string();
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            match inner.rmdir(parent, &name).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(error),
            }
        });
    }

    fn flush(
        &self,
        _context: &Request,
        _ino: INodeNo,
        _file_handle: FileHandle,
        _lock: LockOwner,
        reply: ReplyEmpty,
    ) {
        // duplicated descriptors can flush before the replacement text is written
        reply.ok();
    }

    fn release(
        &self,
        _context: &Request,
        ino: INodeNo,
        _file_handle: FileHandle,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            match inner.flush(ino).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(error),
            }
        });
    }

    fn fsync(
        &self,
        _context: &Request,
        ino: INodeNo,
        _file_handle: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            match inner.flush(ino).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(error),
            }
        });
    }
}
