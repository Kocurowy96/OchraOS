// SPDX-License-Identifier: GPL-2.0-only

//! The page cache: a regular file's contents in 4 KiB pages, by page number.
//!
//! Reads and writes of a file go through it: a page that is not cached is first filled by
//! the filesystem (`Inode::read_page`: from the disk, or zeros for a tmpfs hole). A changed
//! page is marked dirty until written back (disk filesystems, P4). Pages are shared through
//! `Arc`, so `mmap` (stage 4) can map the very same frames into processes.
//!
//! The file's size lives in its metadata, not here: these functions only move bytes.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use ochra_frame::mm::FRAME_SIZE;
use ochra_frame::mm::frames::{self, Frame};
use ochra_frame::sync::IrqMutex;

use super::Inode;
use crate::errno::{ENOSPC, Errno};
use crate::sched::{Mutex, MutexGuard};

const PAGE: u64 = FRAME_SIZE;

/// One cached page.
pub struct Page {
    /// Copies in and out are short, so a spin lock is enough here.
    frame: IrqMutex<Frame>,
    /// Changed since it was last written back.
    dirty: AtomicBool,
}

impl Page {
    #[expect(dead_code, reason = "for writeback to disks (P4)")]
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }
}

/// A file's cached pages.
pub struct PageCache {
    pages: Mutex<BTreeMap<u64, Arc<Page>>>,
    /// Held across a write and its size change, so appends and truncates do not interleave.
    writes: Mutex<()>,
}

impl Default for PageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PageCache {
    pub const fn new() -> Self {
        Self {
            pages: Mutex::new(BTreeMap::new()),
            writes: Mutex::new(()),
        }
    }

    /// Serialises the writers of the file (`write`, `truncate`) with their size updates.
    pub fn lock_writes(&self) -> MutexGuard<'_, ()> {
        self.writes.lock()
    }

    /// Page `index` of `inode`'s file, filled by the filesystem if it was not cached.
    fn page(&self, inode: &dyn Inode, index: u64) -> Result<Arc<Page>, Errno> {
        let mut pages = self.pages.lock();
        if let Some(page) = pages.get(&index) {
            return Ok(page.clone());
        }
        // A full tmpfs is "no space", as Linux reports for tmpfs.
        let mut frame = frames::allocate().ok_or(ENOSPC)?;
        inode.read_page(index, &mut frame)?;
        let page = Arc::new(Page {
            frame: IrqMutex::new(frame),
            dirty: AtomicBool::new(false),
        });
        pages.insert(index, page.clone());
        Ok(page)
    }

    /// Copies the file's bytes from `offset` into `buffer`, up to the file's `size`; returns
    /// how many.
    pub fn read(
        &self,
        inode: &dyn Inode,
        offset: u64,
        buffer: &mut [u8],
        size: u64,
    ) -> Result<usize, Errno> {
        let end = size.min(offset.saturating_add(buffer.len() as u64));
        let mut position = offset;
        while position < end {
            let index = position / PAGE;
            let within = (position % PAGE) as usize;
            let count = ((PAGE - within as u64).min(end - position)) as usize;
            let done = (position - offset) as usize;
            let page = self.page(inode, index)?;
            page.frame
                .lock()
                .read_bytes(within, &mut buffer[done..done + count]);
            position += count as u64;
        }
        Ok((end.saturating_sub(offset)) as usize)
    }

    /// Copies `data` into the file at `offset`; returns how many bytes went in (fewer only
    /// when memory runs out after some did). The caller updates the size.
    pub fn write(&self, inode: &dyn Inode, offset: u64, data: &[u8]) -> Result<usize, Errno> {
        let mut done = 0;
        while done < data.len() {
            let position = offset + done as u64;
            let index = position / PAGE;
            let within = (position % PAGE) as usize;
            let count = (PAGE as usize - within).min(data.len() - done);
            let page = match self.page(inode, index) {
                Ok(page) => page,
                Err(_) if done > 0 => break,
                Err(error) => return Err(error),
            };
            page.frame
                .lock()
                .write_bytes(within, &data[done..done + count]);
            page.dirty.store(true, Ordering::Relaxed);
            done += count;
        }
        Ok(done)
    }

    /// The file is now `size` bytes: pages past the end go, and the rest of the last page is
    /// zeroed (so growing the file again shows zeros, not old bytes).
    pub fn truncate(&self, size: u64) {
        let mut pages = self.pages.lock();
        let first_gone = size.div_ceil(PAGE);
        // `split_off` keeps the pages below `first_gone` in `pages`; the rest is dropped.
        drop(pages.split_off(&first_gone));
        let within = (size % PAGE) as usize;
        if within != 0
            && let Some(page) = pages.get(&(size / PAGE))
        {
            let zeros = [0; PAGE as usize];
            page.frame
                .lock()
                .write_bytes(within, &zeros[..PAGE as usize - within]);
        }
    }
}
