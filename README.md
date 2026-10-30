# ghfs

A temporary FUSE filesystem that represents GitHub as a directory tree at `/tmp/ghfs/`. Users and repositories are fetched via the GitHub API.

### Libraries
- **Linux:** `sudo apt install libfuse3-dev` (or equivalent)
- **macOS:** `brew install --cask macfuse`

In Linux, enable `user_allow_other` in `/etc/fuse.conf`. By default, FUSE mounts are isolated to the mounting user to prevent non-root users from halting root processes and causes a DoS.
Enabling this allows other users, and the root user to see the filesystem.

### Building & Running

```sh
cargo build --release
```

Unauthenticated GitHub API requests are limited to *60 per hour*. GitHub PATs increases the limit to *5k per hour*.

```sh
GITHUB_TOKEN="ghp_xxx" cargo run --release
```

It is recommended to run this as a startup program with the token in env.

### Usage

```sh
$ cd /tmp/github
$ cd torvalds
$ cd linux
$ tree
$ cat fs/fuse/file.c
```
