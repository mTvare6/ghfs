mod fs;

use fuser::{MountOption, SessionACL};

use fs::TokioAdapter;

fn main() {
    env_logger::init();
    let mountpoint = "/tmp/github";

    std::fs::create_dir_all(mountpoint).unwrap();

    let mut config = fuser::Config::default();
    config.mount_options.push(MountOption::RW);
    config.mount_options.push(MountOption::DefaultPermissions);
    config
        .mount_options
        .push(MountOption::FSName("ghfs".to_string()));
    config.mount_options.push(MountOption::AutoUnmount);
    config.acl = SessionACL::RootAndOwner;

    fuser::mount2(TokioAdapter::new(), mountpoint, &config).unwrap();
}
