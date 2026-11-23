# VFS design (stage 3)

The virtual filesystem (VFS) is the layer between file syscalls and the filesystems
(tmpfs, devtmpfs, ext2/ext4, EROFS, later `/proc` and `/sys`). It follows Linux's model,
simplified, because the syscalls it serves follow Linux.

```text
 syscalls (open, read, getdents64, rename, mmap…)
     │
 path walk ── dentry cache ── mount table
     │
 Inode (one trait, one implementation per filesystem) ── page cache (regular files)
     │
 filesystem: tmpfs │ devtmpfs │ ext2/ext4 ── block cache ── NVMe / AHCI
```

## Objects

| Object | What it is | Lives |
| --- | --- | --- |
| `SuperBlock` | one mounted filesystem: its root inode, device number (`st_dev`), flags (read-only), `sync`, `statfs` | as long as it is mounted or a file on it is open |
| `Inode` | a file, directory, link or device node: its metadata and operations | while referenced (dentry, open file, page cache) |
| `Dentry` | a name in a directory pointing to an inode; children cached by name | cache; dropped when unused (no eviction under pressure in stage 3) |
| `Mount` | a superblock attached at a dentry of another mount | the mount table |
| `VfsFile` | one `open`: dentry + mount + position + flags; implements the existing `File` trait | shared by `dup`/`fork` through `OpenFile` |

Pipes and the console stay `File`s outside the VFS; a device node (`/dev/null`) opens the
device's `File`, as in stage 2.

### `Inode`

A trait object (`Arc<dyn Inode>`) per filesystem, with:

- metadata under a lock: `mode` (type and permissions), `uid`, `gid`, `size`, `nlink`,
  `atime`/`mtime`/`ctime`, `rdev`, inode number;
- directory operations: `lookup`, `create`, `mkdir`, `mknod`, `symlink`, `link`, `unlink`,
  `rmdir`, `rename`, `readdir`;
- `readlink` for links;
- for regular files: `read_page(index, &mut Frame)` and `write_page(index, &Frame)` (the
  page cache calls these; tmpfs never needs them), `truncate`;
- `setattr` (`chmod`, `chown`, `utimensat`).

The VFS checks permissions and the arguments before calling a filesystem, so filesystems
only store and fetch.

## Page cache

Every regular file's data goes through its inode's page cache: a map from page index to a
cached page (one 4 KiB frame, "up to date" and "dirty" flags).

- `read` copies from cached pages, reading missing ones from the filesystem first.
- `write` fills or changes cached pages, marks them dirty and updates `size` and `mtime`.
- Writeback (`fsync`, `sync`, unmount, and a periodic flush) writes dirty pages back through
  `write_page`. tmpfs has nowhere to write back: its pages are the file.
- Pages are reference-counted (`Arc`), so in stage 4 `mmap` of a file maps the same frames
  into processes (read-only or copy-on-write for `MAP_PRIVATE`, shared for `MAP_SHARED`), and
  `ld.so` loads libraries without copying. `ochra-frame` will get the API to map a shared
  frame that the address space does not own.
- No eviction in stage 3 (memory is plentiful in tests); a least-recently-used shrinker comes
  with memory pressure handling.

Disks get a separate block cache (P3) for metadata (superblock, inode tables, directories,
bitmaps); file contents never pass through it twice.

## Path walk

Lookup goes component by component from the root or the current directory (or a directory
descriptor for the `…at` calls), as the stage 2 initramfs code does now:

- `.` and `..` (`..` at a mount's root goes to the parent mount; at `/` it stays);
- crossing into a mount when a dentry is a mount point;
- symbolic links followed in the middle, and at the end unless `O_NOFOLLOW`/`AT_SYMLINK_NOFOLLOW`
  (at most 40, `ELOOP`);
- search (execute) permission checked on every directory passed.

The current directory and the root (`chroot` later) are a mount + dentry per process.

## Permissions

Stored and checked from P1, although every process runs as root for now:

- each process has credentials: real, effective and saved user and group IDs, and
  supplementary groups (all 0 for now), plus its `umask`;
- new inodes get the effective user ID, the group of the effective group ID (setgid
  directories later), and `mode & !umask`;
- `may_access(inode, credentials, read/write/execute)` follows Linux: owner, group, then
  other bits; root may read and write anything and search any directory, but may execute a
  file only if some execute bit is set;
- `may_access` is the only place that decides about access: every syscall asks it, nothing
  checks mode bits itself. It has a hook for later policy (Ochra Guard), called after the
  mode bits allow the access and able only to refuse it, as Linux security modules (LSM) do;
- `may_delete(directory, victim, credentials)` adds the sticky bit rule (restricted
  deletion): in a directory with mode bit `S_ISVTX` (as `/tmp`, mode 1777) only the owner
  of the entry, the owner of the directory or root may remove or rename it;
- `chmod` only for the owner or root; `chown` only for root (for now);
- `execve` requires execute permission and a regular file on a mount without `noexec`. It
  ignores the setuid and setgid bits until there are real users (`docs/security.md`).

## Locking

Filesystem operations may sleep (disk I/O from P3), so they must not run under `IrqMutex`
(which keeps interrupts off). A sleeping lock, `sched::Mutex<T>` built on `WaitQueue`, guards
each inode's metadata and directory contents and each page cache. Short structures (the
mount table, reference counts) keep spin locks.

Rules that prevent deadlocks:

- a parent directory is locked before its child;
- `rename` takes one global rename lock first. Under it, it checks that a directory is not
  moved into its own subtree (`EINVAL`), then locks the two directories: if one is an
  ancestor of the other, the ancestor first; only for unrelated directories does the address
  decide the order (as Linux's `lock_rename`);
- the page cache lock is never held while calling into the filesystem for another inode.

## Root filesystem and boot

- At boot the kernel mounts a tmpfs at `/` and unpacks the initramfs cpio into it (as
  Linux's rootfs), so the root is writable from the start;
- `/tmp` is a tmpfs with mode 1777 (sticky, writable by all);
- a devtmpfs is mounted at `/dev` with `null`, `zero`, `full`, `random`, `urandom`,
  `console` and `tty`;
- in P4 the kernel mounts the disk given by `root=` (ext4) and starts `/sbin/init` or the
  `init=` program from there; with an initramfs, `/init` can still mount it itself
  (`switch_root`).

## Directory reading

`getdents64` returns entries with an offset cookie (`d_off`) that later calls continue from.
tmpfs gives every entry a stable cookie at creation (an increasing counter), so entries
created or removed during a listing neither repeat nor hide others; `.` and `..` are 1 and
2. ext2/ext4 use the byte offset within the directory, as Linux does (hashed `dir_index`
directories are read linearly, which is still correct).

## Time

Inode times come from the wall clock (RTC at boot plus the TSC); see the P1 clock work.

`atime` follows Linux's default `relatime`: a read updates it only if it is older than
`mtime` or `ctime`, or more than a day old. Reads therefore rarely cause writes.

## As built in P1

Where the code (`kernel/src/vfs/`, `kernel/src/fs/tmpfs.rs`) differs from the design above,
and what is still missing:

- **No `SuperBlock` object yet.** A `Mount` holds the root dentry of a filesystem; tmpfs keeps
  its device number and inode counter privately. The superblock (read-only flag, `sync`,
  `statfs`, mount flags such as `noexec`) comes with the first disk filesystem in P4.
- **One `create` instead of `create`/`mkdir`/`mknod`/`symlink`.** `Inode::create` takes a
  `NewNode` (regular file, directory, symbolic link or character device). `setattr` is
  `set_metadata`, which takes a closure that changes the metadata under the inode's lock.
  `write_page` and `truncate` join the trait with the first filesystem that has a backing
  store (P4); tmpfs needs neither.
- **Locks.** Each dentry has a sleeping lock (`sched::Mutex`) held while its directory's
  entries change and while a lookup fills the dentry cache; `rename` takes the global rename
  lock and then the two directory locks as described above. Inside tmpfs, metadata and
  directory maps are short spin locks, because tmpfs never sleeps; a disk filesystem will
  sleep only under the VFS's sleeping locks. A page cache has a second sleeping lock that
  keeps a write (or truncate) and its size update together, so `O_APPEND` writes do not mix.
- **Pages** are filled when they enter the cache, under the cache's lock, so there is no "up
  to date" flag; the "dirty" flag is set but not used until writeback (P4).
- **`/dev`** is a plain tmpfs in which the kernel creates the device nodes at boot (no
  separate devtmpfs). A device opened through its node is a `DeviceNode`: reads and writes go
  to the device, `fstat`/`fchmod`/`fchown` to the node. `/dev/tty` is the console for now.
- **`chown`** follows Linux rather than "root only": an owner may also change the group to one
  of its own groups. It clears set-user-ID (and set-group-ID if the group may execute), as
  Linux does.
- **Names are UTF-8** strings: a path with other bytes cannot be found (`ENOENT`) or created.
  Linux allows any byte but `/` and NUL; this is lifted when a program needs it.
- **Not yet:** FIFOs, sockets and block device nodes (`mknod` refuses them); changing
  `O_APPEND` with `fcntl(F_SETFL)`; `RENAME_EXCHANGE`; `statx`; `noexec`; file locks.
- `relatime` is implemented but not compared with the host in the tests: the result depends
  on the host's mount options (the owner's `/tmp` is `noatime`).

## Not in stage 3

File locks (`flock`, `fcntl` locks), `inotify`, extended attributes and ACLs, quotas,
`O_DIRECT`, splice, page cache eviction.
