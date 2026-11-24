// SPDX-License-Identifier: GPL-2.0-only

//! A process's memory: which parts of its address space are in use ("areas"), the program
//! break (`brk`), anonymous `mmap`, `munmap`, `mprotect`, and the stack that grows on demand.
//!
//! Areas are only reservations: the permissions of each page live in the page tables
//! (`UserSpace`), which are the truth about what is mapped. Memory is allocated eagerly when
//! an area is created (zeroed frames), except the stack, which grows one page at a time when
//! the program touches the page below it (like Linux's grows-down stack area). Copy-on-write
//! and lazy allocation come later.

use alloc::collections::BTreeMap;

use ochra_frame::mm::FRAME_SIZE;
use ochra_frame::mm::frames;
use ochra_frame::mm::user::{USER_END, UserAccess, UserSpace};

use crate::errno::{EACCES, EBADF, EEXIST, EINVAL, ENOMEM, EPERM, Errno};

/// Nothing is mapped below this address, so a NULL pointer (plus a small offset) always
/// faults. Linux's default `vm.mmap_min_addr`.
pub const MIN_ADDRESS: u64 = 0x1_0000;

/// How far the stack may grow: Linux's default `RLIMIT_STACK` (8 MiB).
pub const STACK_LIMIT: u64 = 8 * 1024 * 1024;

/// Free space kept below the stack's room to grow, where `mmap` puts nothing: Linux's
/// `stack_guard_gap` (256 pages). Without it, a mapping could sit right under the stack, and
/// a large stack frame could jump over the last stack page straight into it ("Stack Clash").
pub const STACK_GUARD_GAP: u64 = 256 * FRAME_SIZE;

pub fn page_down(address: u64) -> u64 {
    address & !(FRAME_SIZE - 1)
}

/// Rounds up to a page boundary; `None` on overflow.
pub fn page_up(address: u64) -> Option<u64> {
    Some(address.checked_add(FRAME_SIZE - 1)? & !(FRAME_SIZE - 1))
}

/// `mmap`/`mprotect` protection bits.
mod prot {
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const EXEC: u64 = 4;
}

/// `mmap` flags (`include/uapi/linux/mman.h`, `asm-generic/mman-common.h`, `asm/mman.h`).
mod map {
    pub const SHARED: u64 = 0x01;
    pub const PRIVATE: u64 = 0x02;
    pub const SHARED_VALIDATE: u64 = 0x03;
    pub const TYPE: u64 = 0x0f;
    pub const FIXED: u64 = 0x10;
    pub const ANONYMOUS: u64 = 0x20;
    pub const FIXED_NOREPLACE: u64 = 0x10_0000;
}

/// Turns `PROT_*` bits into page permissions: `EINVAL` for unknown bits, `EACCES` for
/// writable and executable at once. User memory is W^X too, for now (docs/decisions.md);
/// Linux refuses the same way when SELinux forbids `execmem`. JIT compilers will need a way
/// around it later.
fn access(protection: u64) -> Result<UserAccess, Errno> {
    if protection & !(prot::READ | prot::WRITE | prot::EXEC) != 0 {
        return Err(EINVAL);
    }
    let writable = protection & prot::WRITE != 0;
    let executable = protection & prot::EXEC != 0;
    if writable && executable {
        return Err(EACCES);
    }
    Ok(UserAccess {
        readable: protection & prot::READ != 0 || writable || executable,
        writable,
        executable,
    })
}

/// The address ranges in use, as `start -> end` (page-aligned, never overlapping).
#[derive(Clone, Default)]
struct Areas(BTreeMap<u64, u64>);

impl Areas {
    fn is_free(&self, start: u64, end: u64) -> bool {
        // Only the last area starting before `end` can reach into the range.
        self.0
            .range(..end)
            .next_back()
            .is_none_or(|(_, &area_end)| area_end <= start)
    }

    /// Whether every byte of the range is in some area.
    fn covers(&self, start: u64, end: u64) -> bool {
        let mut cursor = start;
        while cursor < end {
            match self.0.range(..=cursor).next_back() {
                Some((_, &area_end)) if area_end > cursor => cursor = area_end,
                _ => return false,
            }
        }
        true
    }

    fn insert(&mut self, start: u64, end: u64) {
        debug_assert!(self.is_free(start, end));
        self.0.insert(start, end);
    }

    /// Removes the range, splitting areas that stick out of it.
    fn remove(&mut self, start: u64, end: u64) {
        let overlapping: alloc::vec::Vec<(u64, u64)> = self
            .0
            .range(..end)
            .rev()
            .take_while(|(_, area_end)| **area_end > start)
            .map(|(&area_start, &area_end)| (area_start, area_end))
            .collect();
        for (area_start, area_end) in overlapping {
            self.0.remove(&area_start);
            if area_start < start {
                self.0.insert(area_start, start);
            }
            if area_end > end {
                self.0.insert(end, area_end);
            }
        }
    }

    /// The highest free range of `length` bytes that ends at or below `top`.
    fn find_free(&self, length: u64, top: u64) -> Option<u64> {
        let mut top = top;
        for (&area_start, &area_end) in self.0.range(..top).rev() {
            if area_end <= top && top - area_end >= length {
                return Some(top - length);
            }
            top = top.min(area_start);
        }
        (top.checked_sub(length)? >= MIN_ADDRESS).then(|| top - length)
    }
}

/// A run of pages with the same access, for `/proc/<pid>/maps`.
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub access: UserAccess,
    /// `[heap]`, `[stack]`, or empty.
    pub name: &'static str,
}

/// `Clone` for `fork`: the child gets the same layout (its pages are copied separately).
#[derive(Clone)]
pub struct Memory {
    areas: Areas,
    /// The program break: `brk_start..brk` is the heap that `brk` grows and shrinks; its
    /// pages are `brk_start..page_up(brk)`.
    brk_start: u64,
    brk: u64,
    /// `mmap` without an address looks for free space below this (top down, as Linux).
    mmap_base: u64,
    /// The range the stack may grow into, and the lowest page of it mapped so far.
    stack_limit: u64,
    stack_bottom: u64,
}

impl Memory {
    pub fn new(mmap_base: u64) -> Self {
        Self {
            areas: Areas::default(),
            brk_start: 0,
            brk: 0,
            mmap_base,
            stack_limit: 0,
            stack_bottom: 0,
        }
    }

    /// The memory in use as runs of mapped pages with equal access (`/proc/<pid>/maps`).
    /// Linux lists the areas as they were made; this reads the page tables, so neighbouring
    /// areas with the same access show as one, and pages not mapped yet (the stack's room
    /// to grow, the guard gap) do not show.
    pub fn regions(&self, space: &UserSpace) -> alloc::vec::Vec<Region> {
        let heap = self.brk_start..page_up(self.brk).unwrap_or(self.brk);
        let stack = self.stack_limit..self.areas.0.values().copied().max().unwrap_or(0);
        let mut regions: alloc::vec::Vec<Region> = alloc::vec::Vec::new();
        for (&start, &end) in &self.areas.0 {
            for page in (start..end).step_by(FRAME_SIZE as usize) {
                let Some((_, access)) = space.translate(page) else {
                    continue;
                };
                let name = if heap.contains(&page) {
                    "[heap]"
                } else if self.stack_limit != 0 && stack.contains(&page) {
                    "[stack]"
                } else {
                    ""
                };
                match regions.last_mut() {
                    Some(last)
                        if last.end == page && last.access == access && last.name == name =>
                    {
                        last.end = page + FRAME_SIZE;
                    }
                    _ => regions.push(Region {
                        start: page,
                        end: page + FRAME_SIZE,
                        access,
                        name,
                    }),
                }
            }
        }
        regions
    }

    /// Reserves `start..end` (page-aligned) and fills it with zeroed pages.
    pub fn map_zeroed(
        &mut self,
        space: &UserSpace,
        start: u64,
        end: u64,
        access: UserAccess,
    ) -> Result<(), Errno> {
        if start < MIN_ADDRESS || end > USER_END || start >= end || !self.areas.is_free(start, end)
        {
            return Err(ENOMEM);
        }
        populate(space, start, end, access)?;
        self.areas.insert(start, end);
        Ok(())
    }

    /// Sets up the stack: `bottom..top` is mapped now, and it may grow down to `top -
    /// STACK_LIMIT` when used. Below that, the guard gap is reserved too.
    pub fn map_stack(&mut self, space: &UserSpace, bottom: u64, top: u64) -> Result<(), Errno> {
        let limit = top - STACK_LIMIT;
        let reserved = limit - STACK_GUARD_GAP;
        if !self.areas.is_free(reserved, top) {
            return Err(ENOMEM);
        }
        populate(space, bottom, top, UserAccess::READ_WRITE)?;
        self.areas.insert(reserved, top);
        self.stack_limit = limit;
        self.stack_bottom = bottom;
        Ok(())
    }

    /// Called for a page fault at `address` on a page that is not mapped: if it is in the
    /// stack's room to grow, maps zeroed pages down to it and returns `true` (the program
    /// then retries the access).
    pub fn grow_stack(&mut self, space: &UserSpace, address: u64) -> bool {
        if address < self.stack_limit || address >= self.stack_bottom {
            return false;
        }
        let page = page_down(address);
        if populate(space, page, self.stack_bottom, UserAccess::READ_WRITE).is_err() {
            return false;
        }
        self.stack_bottom = page;
        true
    }

    /// The heap starts at `start` (page-aligned, just past the program, plus a random gap).
    pub fn set_brk_start(&mut self, start: u64) {
        self.brk_start = start;
        self.brk = start;
    }

    /// `brk(address)`: moves the program break and returns the new one, or the old one if
    /// it cannot move (that is how Linux reports failure: the C library compares). Address 0
    /// (or any address below the start) only asks for the current break.
    pub fn brk(&mut self, space: &UserSpace, address: u64) -> u64 {
        if address < self.brk_start || address >= USER_END {
            return self.brk;
        }
        let (Some(old_end), Some(new_end)) = (page_up(self.brk), page_up(address)) else {
            return self.brk;
        };
        if new_end > old_end {
            if !self.areas.is_free(old_end, new_end)
                || populate(space, old_end, new_end, UserAccess::READ_WRITE).is_err()
            {
                return self.brk;
            }
        } else if new_end < old_end {
            space.unmap(new_end, (old_end - new_end) / FRAME_SIZE);
        }
        // The heap is one area, `brk_start..new_end`, replaced as a whole.
        self.areas
            .remove(self.brk_start, old_end.max(self.brk_start));
        if new_end > self.brk_start {
            self.areas.insert(self.brk_start, new_end);
        }
        self.brk = address;
        address
    }

    /// `mmap(address, length, protection, flags, fd, offset)`, anonymous memory only for now
    /// (files come with the VFS).
    pub fn mmap(
        &mut self,
        space: &UserSpace,
        address: u64,
        length: u64,
        protection: u64,
        flags: u64,
        offset: u64,
    ) -> Result<u64, Errno> {
        let access = access(protection)?;
        if length == 0 || !offset.is_multiple_of(FRAME_SIZE) {
            return Err(EINVAL);
        }
        match flags & map::TYPE {
            map::PRIVATE => {}
            // Shared memory only differs from private once `fork` exists (stage 2, P3).
            map::SHARED | map::SHARED_VALIDATE => {
                log::debug!("mmap: MAP_SHARED is not supported yet");
                return Err(EINVAL);
            }
            _ => return Err(EINVAL),
        }
        if flags & map::ANONYMOUS == 0 {
            // No file descriptors refer to files yet.
            return Err(EBADF);
        }
        let length = page_up(length).ok_or(ENOMEM)?;
        let fixed = flags & (map::FIXED | map::FIXED_NOREPLACE) != 0;
        let start = if fixed {
            if !address.is_multiple_of(FRAME_SIZE) {
                return Err(EINVAL);
            }
            let end = address.checked_add(length).ok_or(ENOMEM)?;
            if end > USER_END {
                return Err(ENOMEM);
            }
            if address < MIN_ADDRESS {
                return Err(EPERM);
            }
            if !self.areas.is_free(address, end) {
                if flags & map::FIXED_NOREPLACE != 0 {
                    return Err(EEXIST);
                }
                self.unmap(space, address, end);
            }
            address
        } else {
            // The address is only a hint: used if it is free, otherwise anything goes.
            let hint = page_down(address);
            let hint_fits = hint >= MIN_ADDRESS
                && hint
                    .checked_add(length)
                    .is_some_and(|end| end <= USER_END && self.areas.is_free(hint, end));
            if hint_fits {
                hint
            } else {
                self.areas.find_free(length, self.mmap_base).ok_or(ENOMEM)?
            }
        };
        populate(space, start, start + length, access)?;
        self.areas.insert(start, start + length);
        Ok(start)
    }

    /// `munmap(address, length)`. Holes in the range are fine.
    pub fn munmap(&mut self, space: &UserSpace, address: u64, length: u64) -> Result<(), Errno> {
        let end = check_range(address, length)?;
        self.unmap(space, address, end);
        Ok(())
    }

    /// `mprotect(address, length, protection)`. Fails with `ENOMEM` if part of the range is
    /// not mapped, as Linux.
    pub fn mprotect(
        &mut self,
        space: &UserSpace,
        address: u64,
        length: u64,
        protection: u64,
    ) -> Result<(), Errno> {
        let access = access(protection)?;
        if length == 0 && address.is_multiple_of(FRAME_SIZE) {
            return Ok(());
        }
        let end = check_range(address, length)?;
        if !self.areas.covers(address, end) {
            return Err(ENOMEM);
        }
        space.protect(address, (end - address) / FRAME_SIZE, access);
        Ok(())
    }

    fn unmap(&mut self, space: &UserSpace, start: u64, end: u64) {
        // The frames come back as `Frame`s and are freed when dropped.
        drop(space.unmap(start, (end - start) / FRAME_SIZE));
        self.areas.remove(start, end);
    }
}

/// Checks an `munmap`/`mprotect` range; returns its page-aligned end.
fn check_range(address: u64, length: u64) -> Result<u64, Errno> {
    if !address.is_multiple_of(FRAME_SIZE) || length == 0 {
        return Err(EINVAL);
    }
    let end = page_up(address.checked_add(length).ok_or(EINVAL)?).ok_or(EINVAL)?;
    if end > USER_END {
        return Err(EINVAL);
    }
    Ok(end)
}

/// Maps zeroed pages over `start..end`; on failure, takes back what it mapped.
fn populate(space: &UserSpace, start: u64, end: u64, access: UserAccess) -> Result<(), Errno> {
    let mut page = start;
    while page < end {
        let mapped = frames::allocate().ok_or(ENOMEM).and_then(|mut frame| {
            frame.zero();
            space.map(page, frame, access).map_err(|_| ENOMEM)
        });
        if mapped.is_err() {
            drop(space.unmap(start, (page - start) / FRAME_SIZE));
            return Err(ENOMEM);
        }
        page += FRAME_SIZE;
    }
    Ok(())
}
