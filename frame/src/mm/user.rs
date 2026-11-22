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
//!
//! A page can also be mapped with no access at all (`PROT_NONE` in Linux, used for guard
//! pages): its entry keeps the frame but is not present, so any access faults. Software bit 9
//! ([`OWNED`]) marks every level-1 entry that holds a frame, present or not.

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

/// Marks a level-1 entry that owns a frame (the CPU ignores bit 9).
const OWNED: u64 = 1 << 9;

/// Permissions of a user page, as in `mmap`'s `PROT_*` flags. The CPU cannot make a page
/// writable or executable without making it readable, so those imply `readable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserAccess {
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

impl UserAccess {
    /// No access: any use of the page faults.
    pub const NONE: Self = Self {
        readable: false,
        writable: false,
        executable: false,
    };
    pub const READ: Self = Self {
        readable: true,
        writable: false,
        executable: false,
    };
    pub const READ_WRITE: Self = Self {
        readable: true,
        writable: true,
        executable: false,
    };
    pub const READ_EXECUTE: Self = Self {
        readable: true,
        writable: false,
        executable: true,
    };

    fn is_none(self) -> bool {
        !self.readable && !self.writable && !self.executable
    }
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
        if unsafe { entry.read() } & OWNED != 0 {
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
                if value & OWNED != 0 {
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
                if value & OWNED != 0 {
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
        (value & OWNED != 0).then(|| {
            let present = value & PRESENT != 0;
            let access = UserAccess {
                readable: present,
                writable: present && value & WRITABLE != 0,
                executable: present && value & NO_EXECUTE == 0,
            };
            (PhysAddr(value & ADDRESS_MASK), access)
        })
    }

    /// Writes `bytes` to user address `virt` through the direct map, whatever the pages'
    /// permissions (the loader fills read-only code pages this way). Fails if a page in the
    /// range is not mapped.
    pub fn write_bytes(&self, virt: u64, bytes: &[u8]) -> Result<(), MapError> {
        let mut done = 0;
        while done < bytes.len() {
            let address = virt + done as u64;
            let page = address & !(FRAME_SIZE - 1);
            let offset = (address - page) as usize;
            let length = (FRAME_SIZE as usize - offset).min(bytes.len() - done);
            let _guard = self.lock.lock();
            let entry = self.leaf(page).ok_or(MapError::BadAddress)?;
            // SAFETY: `leaf` returned an entry inside a live table; we hold the lock.
            let value = unsafe { entry.read() };
            if value & OWNED == 0 {
                return Err(MapError::BadAddress);
            }
            let target = direct_map(PhysAddr(value & ADDRESS_MASK));
            // SAFETY: the frame is owned by this space and stays mapped while we hold the
            // lock; `offset + length` is within the frame. User code may be writing to the
            // same frame at the same time, which only makes the bytes it sees unpredictable.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes[done..].as_ptr(), target.add(offset), length)
            };
            done += length;
        }
        Ok(())
    }

    /// A copy of this address space with copies of all its pages (for `fork`), or `None` if
    /// memory runs out. The copy is eager: every page is copied now (copy-on-write later).
    pub fn duplicate(&self) -> Option<Self> {
        let copy = Self::new()?;
        let _guard = self.lock.lock();
        let mut ok = true;
        // SAFETY: `root` is this space's live PML4, and we hold its lock, so its tables do
        // not change while we read them.
        unsafe {
            for_each_page(self.root, 4, 0, &mut |virt, value| {
                if !ok {
                    return;
                }
                ok = copy.copy_page(virt, value);
            })
        };
        ok.then_some(copy)
    }

    /// Maps a copy of the frame in the level-1 entry `value` at `virt`, with the same bits.
    fn copy_page(&self, virt: u64, value: u64) -> bool {
        let Some(mut frame) = super::frames::allocate() else {
            return false;
        };
        let source = direct_map(PhysAddr(value & ADDRESS_MASK));
        // SAFETY: the source is a frame owned by the space being copied (whose lock the
        // caller holds), read through the direct map; the new frame is ours.
        let bytes = unsafe { core::slice::from_raw_parts(source, FRAME_SIZE as usize) };
        frame.write_bytes(0, bytes);
        let _guard = self.lock.lock();
        let mut created = 0;
        // SAFETY: `root` is this (new) space's live PML4, changed only under `lock`.
        let entry =
            unsafe { paging::walk(self.root, virt, 1, Create::Yes { user: true }, &mut created) };
        let Some(entry) = entry else {
            return false;
        };
        // SAFETY: `walk` returned an entry of this space; the frame's ownership moves into it.
        unsafe { entry.write(frame.into_address().0 | (value & !ADDRESS_MASK)) };
        true
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
        let owns = if level > 1 { PRESENT } else { OWNED };
        if value & owns == 0 {
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

/// Calls `visit(virtual address, entry)` for every level-1 entry that owns a frame, in the
/// user half of the table at `level` covering addresses from `base`.
///
/// # Safety
///
/// The table must be a live page table that does not change during the walk.
unsafe fn for_each_page(table: PhysAddr, level: u32, base: u64, visit: &mut dyn FnMut(u64, u64)) {
    let entries = direct_map(table).cast::<u64>();
    // Only the lower (user) half of the PML4.
    let count = if level == 4 { 256 } else { 512 };
    let span = FRAME_SIZE << (9 * (level - 1));
    for index in 0..count {
        // SAFETY: `index < 512`, inside the table frame, reached through the direct map.
        let value = unsafe { entries.add(index).read() };
        let virt = base + index as u64 * span;
        if level == 1 {
            if value & OWNED != 0 {
                visit(virt, value);
            }
        } else if value & PRESENT != 0 {
            // SAFETY: a present entry above level 1 points to a lower table of this space.
            unsafe { for_each_page(PhysAddr(value & ADDRESS_MASK), level - 1, virt, visit) };
        }
    }
}

fn is_user_page(virt: u64) -> bool {
    virt.is_multiple_of(FRAME_SIZE) && virt < USER_END
}

fn entry_bits(access: UserAccess) -> u64 {
    if access.is_none() {
        return OWNED;
    }
    Access {
        writable: access.writable,
        executable: access.executable,
        user: true,
    }
    .entry_bits()
        | USER
        | OWNED
}
