# ghfs

A read-only FUSE filesystem that exposes GitHub as a directory tree. Users, repositories, and files are fetched lazily via the GitHub API and presented using FUSE as a frontend.

### Features
- Mounts at `/tmp/github`
- Uses Github API for user/repo discovery with option for using `GITHUB_TOKEN`
- File contents lazily hydrated
- Lock free asyncfs using tokio for I/O

### Libraries
- **Linux:** `sudo apt install libfuse3-dev` (or equivalent)
- **macOS:** `brew install --cask macfuse`

In Linux, enable `user_allow_other` in `/etc/fuse.conf`. By default, FUSE mounts are isolated to the mounting user to prevent non-root users from halting root processes and causes a DoS. Enabling this allows other users, and the root user to see the filesystem.

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

It auto unmounts cleanly when program is killed.

### Caveats
- Read-only mount
- Cache is not invalidated during runtime (in-memory only)
