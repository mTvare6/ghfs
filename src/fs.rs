use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug)]
struct Cache;

impl Cache {
    fn new() -> Self {
        Self {}
    }
}

#[derive(Debug)]
struct File {
    size: u64,
    url: String,
    cache: Cache,
}

#[derive(Debug)]
struct Dir {
    files: HashMap<String, u64>,
    name: String,
}

#[derive(Debug)]
enum Node {
    File(File),
    Dir(Dir),
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

    fn new() -> Self {
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

    fn add_repo(&mut self, response: GithubTreeResponse, author: String, repo: String) {
        let author = self.new_dir(1, author);
        let repo = self.new_dir(author, repo);
        for node in response.tree {
            match node.node_type {
                GithubNodeType::Blob => {
                    let size = node.size.unwrap();
                    let file_path = node.path;
                    let (parent, file_name) = self.get_parent_and_node(repo, file_path).unwrap();
                    let url = node.url;
                    let cache = Cache::new();
                    let file = File { size, url, cache };
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


