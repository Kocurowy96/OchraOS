// SPDX-License-Identifier: GPL-2.0-only

//! User address spaces: one set of page tables per process.
//!
//! The lower half of the 48-bit address space (`0` to [`USER_END`]) belongs to the process;
//! the upper half is the kernel, shared by every process: a new address space copies the 256
//! kernel-half PML4 entries, so all of them point to the same kernel tables. (The kernel half
//! never gains new PML4 entries after boot, see `paging`.) Kernel pages are supervisor-only,
//! so user code cannot touch them.
//!
//! Frames mapped into a user address space belong to it: they are handed over as owned
//! [`Frame`]s and come back as `Frame`s when unmapped, or are freed when the address space is
//! dropped, together with its page tables.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use super::frames::Frame;
use super::paging::{
    self, ADDRESS_MASK, Access, Create, NO_EXECUTE, PRESENT, USER, WRITABLE, entry_pointer,
};
use super::{FRAME_SIZE, PhysAddr, direct_map, tlb};
use crate::sync::IrqMutex;

/// First address past the user half.
pub const USER_END: u64 = 0x0000_8000_0000_0000;

/// Permissions of a user page. (Every user page is readable.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserAccess {
    pub writable: bool,
    pub executable: bool,
}

/// Why a page could not be mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Not page-aligned, or not in the user half.
    BadAddress,
    AlreadyMapped,
    /// No memory left for a page table.
    OutOfMemory,
}

/// One process's address space.
pub struct UserSpace {
    root: PhysAddr,
    /// Serialises changes to the user half of the tables.
    lock: IrqMutex<()>,
    /// On how many CPUs this address space is in CR3 right now.
    active: AtomicU32,
}

impl UserSpace {
    /// Creates an empty address space (only the kernel half is mapped). `None` if memory is
    /// exhausted.
    pub fn new() -> Option<Self> {
        let mut frame = super::frames::allocate()?;
        frame.zero();
        let root = frame.into_address();
        let kernel = paging::kernel_root();
        for index in 256..512 {
            let virt = (index as u64) << 39;
            // SAFETY: both PML4s are live page table frames; the kernel's upper half no longer
            // changes at PML4 level (frozen in `paging::init`), and the new one is ours alone.
            unsafe {
                let value = entry_pointer(kernel, virt, 4).read();
                entry_pointer(root, virt, 4).write(value);
            }
        }
        Some(Self {
            root,
            lock: IrqMutex::new(()),
            active: AtomicU32::new(0),
        })
    }

    /// Maps `frame` at the page `virt`. On failure the frame is handed back.
    pub fn map(
        &self,
        virt: u64,
        frame: Frame,
        access: UserAccess,
    ) -> Result<(), (MapError, Frame)> {
        if !is_user_page(virt) {
            return Err((MapError::BadAddress, frame));
        }
        let _guard = self.lock.lock();
        let mut created = 0;
        // SAFETY: `root` is this space's live PML4, changed only under `lock`.
        let entry =
            unsafe { paging::walk(self.root, virt, 1, Create::Yes { user: true }, &mut created) };
        let Some(entry) = entry else {
            return Err((MapError::OutOfMemory, frame));
        };
        // SAFETY: `walk` returned an entry inside a live table of this space.
        if unsafe { entry.read() } & PRESENT != 0 {
            return Err((MapError::AlreadyMapped, frame));
        }
        let bits = entry_bits(access);
        // SAFETY: as above; the frame's ownership moves into the entry.
        unsafe { entry.write(frame.into_address().0 | bits) };
        // A page that was not present cannot be in any TLB: no flush needed.
        Ok(())
    }

    /// Unmaps `pages` pages from `start` and returns the frames that were mapped there (holes
    /// are skipped). Every CPU has stopped using them when this returns.
    pub fn unmap(&self, start: u64, pages: u64) -> Vec<Frame> {
        let mut frames = Vec::new();
        {
            let _guard = self.lock.lock();
            for page in 0..pages {
                let virt = start + page * FRAME_SIZE;
                let Some(entry) = self.leaf(virt) else {
                    continue;
                };
                // SAFETY: `leaf` returned an entry inside a live table of this space; we
                // hold the lock.
                let value = unsafe { entry.read() };
                if value & PRESENT != 0 {
                    // SAFETY: as above.
                    unsafe { entry.write(0) };
                    // SAFETY: the entry owned the frame (`map`) and is cleared now.
                    frames.push(unsafe { Frame::from_address(PhysAddr(value & ADDRESS_MASK)) });
                }
            }
        }
        if !frames.is_empty() {
            self.flush(start, pages);
        }
        frames
    }

    /// Changes the permissions of the mapped pages among `pages` pages from `start`. Returns
    /// how many pages were changed.
    pub fn protect(&self, start: u64, pages: u64, access: UserAccess) -> u64 {
        let mut changed = 0;
        {
            let _guard = self.lock.lock();
            for page in 0..pages {
                let virt = start + page * FRAME_SIZE;
                let Some(entry) = self.leaf(virt) else {
                    continue;
                };
                // SAFETY: `leaf` returned an entry inside a live table; we hold the lock.
                let value = unsafe { entry.read() };
                if value & PRESENT != 0 {
                    // SAFETY: as above.
                    unsafe { entry.write((value & ADDRESS_MASK) | entry_bits(access)) };
                    changed += 1;
                }
            }
        }
        if changed > 0 {
            self.flush(start, pages);
        }
        changed
    }

    /// The frame and permissions mapped at the page `virt`, if any.
    pub fn translate(&self, virt: u64) -> Option<(PhysAddr, UserAccess)> {
        let _guard = self.lock.lock();
        let entry = self.leaf(virt)?;
        // SAFETY: `leaf` returned an entry inside a live table; we hold the lock.
        let value = unsafe { entry.read() };
        (value & PRESENT != 0).then(|| {
            let access = UserAccess {
                writable: value & WRITABLE != 0,
                executable: value & NO_EXECUTE == 0,
            };
            (PhysAddr(value & ADDRESS_MASK), access)
        })
    }

    /// The physical address of the PML4, for CR3.
    pub(crate) fn root(&self) -> PhysAddr {
        self.root
    }

    /// Bookkeeping for the task switch: this space was loaded into / removed from CR3 on
    /// the current CPU.
    pub(crate) fn note_loaded(&self) {
        // SeqCst pairs with `flush`: either `flush` sees this CPU as active, or this CPU
        // loads CR3 (dropping old translations) after the page table change.
        self.active.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn note_unloaded(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }

    /// The level-1 entry for `virt`, without creating tables. Hold `lock`.
    fn leaf(&self, virt: u64) -> Option<*mut u64> {
        if !is_user_page(virt) {
            return None;
        }
        let mut unused = 0;
        // SAFETY: `root` is this space's live PML4; the caller holds `lock`.
        unsafe { paging::walk(self.root, virt, 1, Create::No, &mut unused) }
    }

    /// TLB shootdown after a change; other CPUs only if they may use this space.
    fn flush(&self, start: u64, pages: u64) {
        let here = paging::read_cr3() & ADDRESS_MASK == self.root.0;
        let active = self.active.load(Ordering::SeqCst);
        let on_others = active > u32::from(here);
        tlb::flush(self.root, start, pages, on_others);
    }
}

impl Drop for UserSpace {
    fn drop(&mut self) {
        // No CPU has it in CR3 (every running task holds a reference to its space), so no CPU
        // has its translations cached either: freeing needs no shootdown.
        assert_eq!(
            self.active.load(Ordering::SeqCst),
            0,
            "dropping an address space that is still loaded"
        );
        // SAFETY: nothing uses this space any more; its user half is freed exactly once.
        unsafe { free_table(self.root, 4, 0..256) };
        // SAFETY: the PML4 itself; its kernel half only points to shared tables, not freed.
        drop(unsafe { Frame::from_address(self.root) });
    }
}

/// Frees what the entries `range` of the table at `level` point to: the page frames (level 1)
/// or the lower tables, recursively.
///
/// # Safety
///
/// The table must be a live page table that nothing uses any more, owning what it points to.
unsafe fn free_table(table: PhysAddr, level: u32, range: core::ops::Range<usize>) {
    let entries = direct_map(table).cast::<u64>();
    for index in range {
        // SAFETY: `index < 512`, inside the table frame, reached through the direct map.
        let value = unsafe { entries.add(index).read() };
        if value & PRESENT == 0 {
            continue;
        }
        let address = PhysAddr(value & ADDRESS_MASK);
        if level > 1 {
            // SAFETY: a present entry above level 1 points to a lower table of this space.
            unsafe { free_table(address, level - 1, 0..512) };
        }
        // SAFETY: the entry owned this frame (a page or a table), and nothing uses it now.
        drop(unsafe { Frame::from_address(address) });
    }
}

fn is_user_page(virt: u64) -> bool {
    virt.is_multiple_of(FRAME_SIZE) && virt < USER_END
}

fn entry_bits(access: UserAccess) -> u64 {
    Access {
        writable: access.writable,
        executable: access.executable,
        user: true,
    }
    .entry_bits()
        | USER
}
