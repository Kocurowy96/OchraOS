// SPDX-License-Identifier: GPL-2.0-only

//! Names and mounts: [`Dentry`], [`Mount`], [`Path`], the path walk, and the directory
//! operations (create, remove, link, rename).
//!
//! A dentry is a name in a directory with the inode it names. Dentries are cached in their
//! parent while something uses them (an open file, a current directory, a mount point), so
//! the same name always gives the same dentry then; mounts are attached to dentries.
//!
//! Every change to a directory's entries happens under that directory dentry's sleeping
//! lock, and so does filling the cache from the filesystem: a lookup cannot cache a name that
//! is being removed. `rename` also takes one global lock first (`docs/vfs.md`).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use ochra_frame::sync::IrqMutex;

use super::{Access, Credentials, InodeRef, Metadata, NewNode, Timestamp, may_access, may_delete};
use crate::errno::{
    EBUSY, EEXIST, EINVAL, EISDIR, ELOOP, ENAMETOOLONG, ENOENT, ENOTDIR, EPERM, EXDEV, Errno,
};
use crate::sched::{Mutex, MutexGuard};

/// Longest name of one entry (Linux's `NAME_MAX`).
const NAME_MAX: usize = 255;
/// Longest path, with its NUL (Linux's `PATH_MAX`).
pub const PATH_MAX: usize = 4096;
/// Most symbolic links followed in one walk (Linux's `MAXSYMLINKS`).
const MAX_LINKS: usize = 40;

/// A name in a directory and the inode it names.
pub struct Dentry {
    name: IrqMutex<String>,
    /// `None` for the root of a filesystem.
    parent: IrqMutex<Option<Arc<Dentry>>>,
    pub inode: InodeRef,
    /// Children in use, by name.
    children: IrqMutex<BTreeMap<String, Weak<Dentry>>>,
    /// Held while this directory's entries change, and while a lookup fills the cache.
    lock: Mutex<()>,
    /// The name was removed (the inode may live on while open).
    removed: AtomicBool,
}

impl Dentry {
    fn new(name: &str, parent: Option<Arc<Dentry>>, inode: InodeRef) -> Arc<Self> {
        Arc::new(Self {
            name: IrqMutex::new(String::from(name)),
            parent: IrqMutex::new(parent),
            inode,
            children: IrqMutex::new(BTreeMap::new()),
            lock: Mutex::new(()),
            removed: AtomicBool::new(false),
        })
    }

    pub fn metadata(&self) -> Metadata {
        self.inode.metadata()
    }

    fn cached(&self, name: &str) -> Option<Arc<Dentry>> {
        if !self.inode.caches_lookups() {
            return None;
        }
        self.children.lock().get(name).and_then(Weak::upgrade)
    }

    /// The child `name`, from the cache or the filesystem. The caller holds `self.lock`.
    fn lookup_locked(self: &Arc<Self>, name: &str) -> Result<Arc<Dentry>, Errno> {
        if let Some(child) = self.cached(name) {
            return Ok(child);
        }
        let inode = self.inode.lookup(name)?;
        let child = Dentry::new(name, Some(self.clone()), inode);
        let mut children = self.children.lock();
        // Forget entries nobody uses any more while we are here.
        children.retain(|_, child| child.strong_count() > 0);
        children.insert(String::from(name), Arc::downgrade(&child));
        Ok(child)
    }

    fn lookup(self: &Arc<Self>, name: &str) -> Result<Arc<Dentry>, Errno> {
        if let Some(child) = self.cached(name) {
            return Ok(child);
        }
        let _lock = self.lock.lock();
        self.lookup_locked(name)
    }

    /// The name was removed: forget it in the parent and mark it.
    fn forget(&self, name: &str, child: &Arc<Dentry>) {
        child.removed.store(true, Ordering::Relaxed);
        let mut children = self.children.lock();
        if children
            .get(name)
            .is_some_and(|cached| cached.as_ptr() == Arc::as_ptr(child))
        {
            children.remove(name);
        }
    }

    fn parent(&self) -> Option<Arc<Dentry>> {
        self.parent.lock().clone()
    }

    /// Whether `self` is `other` or lies inside it (following parents).
    fn is_within(self: &Arc<Self>, other: &Arc<Dentry>) -> bool {
        let mut current = Some(self.clone());
        while let Some(dentry) = current {
            if Arc::ptr_eq(&dentry, other) {
                return true;
            }
            current = dentry.parent();
        }
        false
    }
}

/// A filesystem attached somewhere in the tree.
pub struct Mount {
    /// The filesystem's root directory.
    pub root: Arc<Dentry>,
    /// The filesystem type (`tmpfs`, `proc`…), and what was mounted (Linux's "source"), for
    /// `/proc/mounts`.
    pub kind: &'static str,
    pub source: &'static str,
    /// Where it is attached: the parent mount and the dentry there (`None` for `/`).
    mountpoint: Option<(Arc<Mount>, Arc<Dentry>)>,
}

/// Every mount but the root one.
static MOUNTS: IrqMutex<Vec<Arc<Mount>>> = IrqMutex::new(Vec::new());
/// `/`.
static ROOT: IrqMutex<Option<Path>> = IrqMutex::new(None);
/// Taken first by every `rename` (`docs/vfs.md`).
static RENAME: Mutex<()> = Mutex::new(());

/// A place in the tree: a dentry in a mount.
#[derive(Clone)]
pub struct Path {
    pub mount: Arc<Mount>,
    pub dentry: Arc<Dentry>,
}

/// Makes a filesystem with root directory `inode` the root of the tree.
pub(super) fn set_root(inode: InodeRef, kind: &'static str) {
    let mount = Arc::new(Mount {
        root: Dentry::new("/", None, inode),
        kind,
        source: kind,
        mountpoint: None,
    });
    *ROOT.lock() = Some(Path {
        dentry: mount.root.clone(),
        mount,
    });
}

/// `/`.
pub fn root() -> Path {
    ROOT.lock().clone().expect("the root is mounted at boot")
}

/// Mounts the filesystem of type `kind` with root directory `inode` on the directory `at`.
pub fn mount(at: &Path, inode: InodeRef, kind: &'static str) -> Result<(), Errno> {
    if !at.metadata().is_directory() {
        return Err(ENOTDIR);
    }
    let mount = Arc::new(Mount {
        root: Dentry::new("/", None, inode),
        kind,
        source: kind,
        mountpoint: Some((at.mount.clone(), at.dentry.clone())),
    });
    MOUNTS.lock().push(mount);
    Ok(())
}

/// Every mount, `/` first, in mount order: the filesystem type, the source and where it is
/// (`/proc/mounts`).
pub fn mounts() -> Vec<(&'static str, &'static str, String)> {
    let root = root();
    let mut all = alloc::vec![(root.mount.kind, root.mount.source, String::from("/"))];
    let mounts = MOUNTS.lock().clone();
    for mount in mounts {
        let at = Path {
            dentry: mount.root.clone(),
            mount: mount.clone(),
        };
        if let Ok(name) = at.absolute_name() {
            all.push((mount.kind, mount.source, name));
        }
    }
    all
}

/// The mount attached at `path`, if any.
fn mounted_on(path: &Path) -> Option<Arc<Mount>> {
    MOUNTS
        .lock()
        .iter()
        .find(|mount| {
            mount.mountpoint.as_ref().is_some_and(|(parent, dentry)| {
                Arc::ptr_eq(parent, &path.mount) && Arc::ptr_eq(dentry, &path.dentry)
            })
        })
        .cloned()
}

/// Whether the last component of a walk follows a symbolic link.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Walk {
    Follow,
    NoFollow,
}

/// A path split for a directory operation: the parent directory and the last name.
pub struct Parent {
    pub directory: Path,
    /// The last component; `.` or `..` when the path ends with one (or is `/`).
    pub name: String,
    /// The path ended with `/`: the entry must be (or become) a directory.
    pub trailing_slash: bool,
}

/// The components of `path`, last first (so `pop` gives the next one).
fn components(path: &str) -> Result<Vec<String>, Errno> {
    let mut parts = Vec::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        if part.len() > NAME_MAX {
            return Err(ENAMETOOLONG);
        }
        parts.push(String::from(part));
    }
    parts.reverse();
    Ok(parts)
}

impl Path {
    pub fn inode(&self) -> &InodeRef {
        &self.dentry.inode
    }

    pub fn metadata(&self) -> Metadata {
        self.dentry.metadata()
    }

    /// The child `name` of this directory, entering a filesystem mounted there.
    fn child(&self, name: &str) -> Result<Path, Errno> {
        let mut path = Path {
            mount: self.mount.clone(),
            dentry: self.dentry.lookup(name)?,
        };
        while let Some(mount) = mounted_on(&path) {
            path = Path {
                dentry: mount.root.clone(),
                mount,
            };
        }
        Ok(path)
    }

    /// `..`: the parent directory, leaving a mounted filesystem at its root; `/` stays.
    pub fn parent(&self) -> Path {
        let mut path = self.clone();
        loop {
            if Arc::ptr_eq(&path.dentry, &path.mount.root) {
                match &path.mount.mountpoint {
                    Some((mount, dentry)) => {
                        path = Path {
                            mount: mount.clone(),
                            dentry: dentry.clone(),
                        };
                        continue;
                    }
                    None => return path,
                }
            }
            let parent = path
                .dentry
                .parent()
                .expect("only a filesystem root has no parent");
            return Path {
                mount: path.mount,
                dentry: parent,
            };
        }
    }

    /// Looks `path` up from here (from `/` if it is absolute), following symbolic links on
    /// the way and, if `last` says so, at the end.
    pub fn walk(&self, path: &str, last: Walk, credentials: &Credentials) -> Result<Path, Errno> {
        if path.is_empty() {
            return Err(ENOENT);
        }
        if path.len() >= PATH_MAX {
            return Err(ENAMETOOLONG);
        }
        let trailing_slash = path.ends_with('/');
        let start = if path.starts_with('/') {
            root()
        } else {
            self.clone()
        };
        let follow_last = last == Walk::Follow || trailing_slash;
        let found = walk_components(start, components(path)?, follow_last, credentials)?;
        if trailing_slash && !found.metadata().is_directory() {
            return Err(ENOTDIR);
        }
        Ok(found)
    }

    /// Looks up the directory that holds the last component of `path`, and that name.
    pub fn walk_parent(&self, path: &str, credentials: &Credentials) -> Result<Parent, Errno> {
        if path.is_empty() {
            return Err(ENOENT);
        }
        if path.len() >= PATH_MAX {
            return Err(ENAMETOOLONG);
        }
        let start = if path.starts_with('/') {
            root()
        } else {
            self.clone()
        };
        let mut parts = components(path)?;
        let Some(name) = (!parts.is_empty()).then(|| parts.remove(0)) else {
            // `/` itself.
            return Ok(Parent {
                directory: start,
                name: String::from("."),
                trailing_slash: true,
            });
        };
        let directory = walk_components(start, parts, true, credentials)?;
        if !directory.metadata().is_directory() {
            return Err(ENOTDIR);
        }
        Ok(Parent {
            directory,
            name,
            trailing_slash: path.ends_with('/'),
        })
    }

    /// The absolute path of this place (`getcwd`); `ENOENT` once it has been removed.
    pub fn absolute_name(&self) -> Result<String, Errno> {
        let mut names = Vec::new();
        let mut path = self.clone();
        loop {
            if path.dentry.removed.load(Ordering::Relaxed) {
                return Err(ENOENT);
            }
            let parent = path.parent();
            if Arc::ptr_eq(&parent.dentry, &path.dentry) {
                break;
            }
            // At a mount's root, the name is that of the mount point.
            let mut named = path.clone();
            while Arc::ptr_eq(&named.dentry, &named.mount.root) {
                match &named.mount.mountpoint {
                    Some((mount, dentry)) => {
                        named = Path {
                            mount: mount.clone(),
                            dentry: dentry.clone(),
                        }
                    }
                    None => break,
                }
            }
            names.push(named.dentry.name.lock().clone());
            path = parent;
        }
        if names.is_empty() {
            return Ok(String::from("/"));
        }
        names.reverse();
        Ok(names.iter().fold(String::new(), |mut all, name| {
            all.push('/');
            all.push_str(name);
            all
        }))
    }

    /// Whether this directory may get or lose entries: a directory, writable and searchable.
    fn check_writable_directory(&self, credentials: &Credentials) -> Result<Metadata, Errno> {
        // A removed directory takes no new entries (it may still be someone's current one).
        if self.dentry.removed.load(Ordering::Relaxed) {
            return Err(ENOENT);
        }
        let metadata = self.metadata();
        if !metadata.is_directory() {
            return Err(ENOTDIR);
        }
        may_access(&metadata, credentials, Access::WRITE.union(Access::EXECUTE))?;
        Ok(metadata)
    }

    /// Whether `name` is in use as a mount point here.
    fn is_mount_point(&self, dentry: &Arc<Dentry>) -> bool {
        mounted_on(&Path {
            mount: self.mount.clone(),
            dentry: dentry.clone(),
        })
        .is_some()
    }

    /// Creates `name` in this directory with `permissions` (the umask already applied).
    pub fn create(
        &self,
        name: &str,
        node: NewNode,
        permissions: u32,
        credentials: &Credentials,
    ) -> Result<Path, Errno> {
        self.check_writable_directory(credentials)?;
        if name == "." || name == ".." {
            return Err(EEXIST);
        }
        let _lock = self.dentry.lock.lock();
        match self.dentry.lookup_locked(name) {
            Ok(_) => return Err(EEXIST),
            Err(ENOENT) => {}
            Err(error) => return Err(error),
        }
        let owner = (credentials.effective_user, credentials.effective_group);
        let inode = self.inode().create(name, node, permissions, owner)?;
        touch_directory(self.inode());
        let child = Dentry::new(name, Some(self.dentry.clone()), inode);
        self.dentry
            .children
            .lock()
            .insert(String::from(name), Arc::downgrade(&child));
        Ok(Path {
            mount: self.mount.clone(),
            dentry: child,
        })
    }

    /// Adds `name` in this directory for the file at `target` (a hard link).
    pub fn link(&self, name: &str, target: &Path, credentials: &Credentials) -> Result<(), Errno> {
        self.check_writable_directory(credentials)?;
        if !Arc::ptr_eq(&self.mount, &target.mount) {
            return Err(EXDEV);
        }
        let metadata = target.metadata();
        if metadata.is_directory() {
            return Err(EPERM);
        }
        if name == "." || name == ".." {
            return Err(EEXIST);
        }
        let _lock = self.dentry.lock.lock();
        match self.dentry.lookup_locked(name) {
            Ok(_) => return Err(EEXIST),
            Err(ENOENT) => {}
            Err(error) => return Err(error),
        }
        self.inode().link(name, target.inode())?;
        touch_directory(self.inode());
        touch_changed(target.inode());
        Ok(())
    }

    /// Removes `name` (not a directory) from this directory.
    pub fn unlink(&self, name: &str, credentials: &Credentials) -> Result<(), Errno> {
        let directory = self.check_writable_directory(credentials)?;
        if name == "." || name == ".." {
            return Err(EISDIR);
        }
        let _lock = self.dentry.lock.lock();
        let victim = self.dentry.lookup_locked(name)?;
        let metadata = victim.metadata();
        if metadata.is_directory() {
            return Err(EISDIR);
        }
        may_delete(&directory, &metadata, credentials)?;
        if self.is_mount_point(&victim) {
            return Err(EBUSY);
        }
        self.inode().unlink(name)?;
        self.dentry.forget(name, &victim);
        touch_directory(self.inode());
        touch_changed(&victim.inode);
        Ok(())
    }

    /// Removes the empty directory `name` from this directory.
    pub fn rmdir(&self, name: &str, credentials: &Credentials) -> Result<(), Errno> {
        let directory = self.check_writable_directory(credentials)?;
        match name {
            "." => return Err(EINVAL),
            ".." => return Err(crate::errno::ENOTEMPTY),
            _ => {}
        }
        let _lock = self.dentry.lock.lock();
        let victim = self.dentry.lookup_locked(name)?;
        let metadata = victim.metadata();
        if !metadata.is_directory() {
            return Err(ENOTDIR);
        }
        may_delete(&directory, &metadata, credentials)?;
        if self.is_mount_point(&victim) {
            return Err(EBUSY);
        }
        // Keep the victim's own entries from changing meanwhile.
        let _victim_lock = victim.lock.lock();
        self.inode().rmdir(name)?;
        self.dentry.forget(name, &victim);
        touch_directory(self.inode());
        Ok(())
    }
}

/// Walks `parts` (last first) from `start`.
fn walk_components(
    start: Path,
    mut parts: Vec<String>,
    follow_last: bool,
    credentials: &Credentials,
) -> Result<Path, Errno> {
    let mut current = start;
    let mut links = 0;
    while let Some(name) = parts.pop() {
        let metadata = current.metadata();
        if !metadata.is_directory() {
            return Err(ENOTDIR);
        }
        may_access(&metadata, credentials, Access::EXECUTE)?;
        match name.as_str() {
            "." => continue,
            ".." => {
                current = current.parent();
                continue;
            }
            _ => {}
        }
        let next = current.child(&name)?;
        let last = parts.is_empty();
        if next.metadata().is_symlink() && (!last || follow_last) {
            links += 1;
            if links > MAX_LINKS {
                return Err(ELOOP);
            }
            let target = next.inode().readlink()?;
            if target.is_empty() {
                return Err(ENOENT);
            }
            // The target's components go on top of the rest (both are kept last first),
            // so they are walked before it. An empty list (the target is `/`) leaves us at
            // the root.
            parts.append(&mut components(&target)?);
            if target.starts_with('/') {
                current = root();
            }
            continue;
        }
        current = next;
    }
    Ok(current)
}

/// A change of a directory's entries: its modification and change times.
fn touch_directory(inode: &InodeRef) {
    let now = Timestamp::now();
    let _ = inode.set_metadata(&mut |metadata| {
        metadata.modified = now;
        metadata.changed = now;
    });
}

/// A change of an inode itself (its link count, owner…): its change time.
fn touch_changed(inode: &InodeRef) {
    let now = Timestamp::now();
    let _ = inode.set_metadata(&mut |metadata| metadata.changed = now);
}

/// Locks two directories for `rename`, in the order of `docs/vfs.md`: an ancestor before its
/// descendant; for unrelated directories, the lower address first. One lock if they are the
/// same directory.
fn lock_pair<'a>(
    first: &'a Arc<Dentry>,
    second: &'a Arc<Dentry>,
) -> (MutexGuard<'a, ()>, Option<MutexGuard<'a, ()>>) {
    if Arc::ptr_eq(first, second) {
        return (first.lock.lock(), None);
    }
    let first_goes_first = if second.is_within(first) {
        true
    } else if first.is_within(second) {
        false
    } else {
        Arc::as_ptr(first) < Arc::as_ptr(second)
    };
    if first_goes_first {
        let a = first.lock.lock();
        (a, Some(second.lock.lock()))
    } else {
        let b = second.lock.lock();
        (b, Some(first.lock.lock()))
    }
}

/// `rename`: moves `old_name` in `old_directory` to `new_name` in `new_directory`, replacing
/// what is there if allowed (unless `no_replace`).
pub fn rename(
    old_directory: &Path,
    old_name: &str,
    new_directory: &Path,
    new_name: &str,
    no_replace: bool,
    credentials: &Credentials,
) -> Result<(), Errno> {
    let old_metadata = old_directory.check_writable_directory(credentials)?;
    let new_metadata = new_directory.check_writable_directory(credentials)?;
    for name in [old_name, new_name] {
        if name == "." || name == ".." {
            return Err(EBUSY);
        }
    }
    if !Arc::ptr_eq(&old_directory.mount, &new_directory.mount) {
        return Err(EXDEV);
    }
    let _rename = RENAME.lock();
    let source = {
        let _lock = old_directory.dentry.lock.lock();
        old_directory.dentry.lookup_locked(old_name)?
    };
    let source_metadata = source.metadata();
    // Under the global lock no directory moves, so this check stays true: a directory cannot
    // go into its own subtree.
    if source_metadata.is_directory() && new_directory.dentry.is_within(&source) {
        return Err(EINVAL);
    }
    let _locks = lock_pair(&old_directory.dentry, &new_directory.dentry);
    // Look again under the directory locks: the name may have gone meanwhile.
    let source = old_directory.dentry.lookup_locked(old_name)?;
    let target = match new_directory.dentry.lookup_locked(new_name) {
        Ok(target) => Some(target),
        Err(ENOENT) => None,
        Err(error) => return Err(error),
    };
    may_delete(&old_metadata, &source_metadata, credentials)?;
    if old_directory.is_mount_point(&source) {
        return Err(EBUSY);
    }
    if let Some(target) = &target {
        if Arc::ptr_eq(&target.inode, &source.inode)
            || target.metadata().inode == source_metadata.inode
        {
            // Two names of one file: nothing to do (POSIX).
            return Ok(());
        }
        if no_replace {
            return Err(EEXIST);
        }
        let target_metadata = target.metadata();
        match (
            source_metadata.is_directory(),
            target_metadata.is_directory(),
        ) {
            (true, false) => return Err(ENOTDIR),
            (false, true) => return Err(EISDIR),
            _ => {}
        }
        may_delete(&new_metadata, &target_metadata, credentials)?;
        if new_directory.is_mount_point(target) || source.is_within(target) {
            return Err(EBUSY);
        }
    }
    // A directory that changes parent gets a new `..`: that writes the directory itself.
    if source_metadata.is_directory() && !Arc::ptr_eq(&old_directory.dentry, &new_directory.dentry)
    {
        may_access(&source_metadata, credentials, Access::WRITE)?;
    }
    old_directory
        .inode()
        .rename(old_name, new_directory.inode(), new_name)?;

    if let Some(target) = &target {
        new_directory.dentry.forget(new_name, target);
    }
    old_directory.dentry.children.lock().remove(old_name);
    *source.name.lock() = String::from(new_name);
    *source.parent.lock() = Some(new_directory.dentry.clone());
    new_directory
        .dentry
        .children
        .lock()
        .insert(String::from(new_name), Arc::downgrade(&source));
    touch_directory(old_directory.inode());
    touch_directory(new_directory.inode());
    touch_changed(&source.inode);
    Ok(())
}
