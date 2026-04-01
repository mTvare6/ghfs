mod fs;

use fuser::experimental::TokioAdapter;
use fuser::{MountOption, SessionACL};

use fs::{GithubFS, GithubTreeResponse};

fn main() {
    env_logger::init();
    let mountpoint = "/tmp/github";

    std::fs::create_dir_all(mountpoint).unwrap();

    let mut config = fuser::Config::default();
    config.mount_options.push(MountOption::RO);
    config
        .mount_options
        .push(MountOption::FSName("ghfs".to_string()));
    config.mount_options.push(MountOption::AutoUnmount);
    config.acl = SessionACL::RootAndOwner;

    let response = include_str!("../a.json");
    let payload: GithubTreeResponse = serde_json::from_str(&response).unwrap();

    let mut ghfs = GithubFS::new();
    ghfs.add_repo(payload, "mTvare6".into(), "hello-world.rs".into());

    fuser::mount2(TokioAdapter::new(ghfs), mountpoint, &config).unwrap();
}
