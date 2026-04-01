mod fs;

use fuser::experimental::TokioAdapter;
use fuser::{MountOption, SessionACL};

use fs::GithubFS;

#[tokio::main]
async fn main() {
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

    fuser::mount2(TokioAdapter::new(GithubFS::new()), mountpoint, &config).unwrap();
}
