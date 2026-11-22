// SPDX-License-Identifier: GPL-2.0-only

//! The kernel heap: makes `Box`, `Vec`, `String` and the rest of the `alloc` crate work.
//!
//! Rust's `alloc` crate asks a "global allocator" for memory through two calls:
//! `alloc(layout)` (give me `size` bytes aligned to `align`) and `dealloc(pointer, layout)`.
//! This is our implementation, deliberately simple:
//!
//! - The heap is a range of virtual addresses (PML4 slot 508), mapped page by page as it
//!   grows, so it costs physical memory only for what is used.
//! - Free memory is a list of blocks sorted by address. Each free block stores its own size
//!   and the address of the next free block in its first 16 bytes.
//! - `alloc` takes the first block that fits ("first fit"), splitting off what is left.
//!   `dealloc` puts the block back in address order and merges it with free neighbours, so
//!   the heap does not crumble into tiny pieces.
//! - Every size is rounded up to 16 bytes. Blocks therefore always start on a 16-byte
//!   boundary and every left-over piece has room for the 16-byte free block header.
//!
//! First fit walks the list on every allocation; size classes ("slabs") can replace it if
//! profiling ever shows it matters.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use crate::sync::IrqMutex;

use super::FRAME_SIZE;
use super::paging::{self, KERNEL_REGIONS_BASE};

/// Start of the heap's virtual range (PML4 slot 508).
const HEAP_BASE: u64 = KERNEL_REGIONS_BASE;
/// The heap may grow up to this size (virtual space; physical memory runs out first).
const HEAP_MAX: u64 = 256 * 1024 * 1024 * 1024;
/// The heap grows by at least this much at a time, to keep page mapping work rare.
const GROW_STEP: u64 = 256 * 1024;
/// Allocation granularity and size of the free block header.
const BLOCK: usize = 16;

#[global_allocator]
static HEAP: Heap = Heap(IrqMutex::new(FreeList {
    head: ptr::null_mut(),
    end: HEAP_BASE,
    used: 0,
}));

struct Heap(IrqMutex<FreeList>);

/// Header written at the start of every free block.
#[repr(C, align(16))]
struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

struct FreeList {
    /// First free block (lowest address), or null.
    head: *mut FreeBlock,
    /// End of the mapped part of the heap.
    end: u64,
    /// Bytes handed out and not yet returned.
    used: usize,
}

// SAFETY: the raw pointers point into the heap's own mapped range, and every access goes
// through the mutex, so moving the list between CPUs is fine.
unsafe impl Send for FreeList {}

/// Heap usage, for the log and for tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Bytes of virtual memory mapped for the heap.
    pub mapped: u64,
    /// Bytes currently allocated.
    pub used: usize,
    /// Number of free blocks (1 means no fragmentation).
    pub free_blocks: usize,
}

pub fn stats() -> Stats {
    let list = HEAP.0.lock();
    let mut free_blocks = 0;
    let mut block = list.head;
    while !block.is_null() {
        free_blocks += 1;
        // SAFETY: every pointer in the list points to a valid free block header.
        block = unsafe { (*block).next };
    }
    Stats {
        mapped: list.end - HEAP_BASE,
        used: list.used,
        free_blocks,
    }
}

/// Maps the first part of the heap. Call once, after `paging::init`.
pub fn init() -> Stats {
    {
        let mut list = HEAP.0.lock();
        if list.end == HEAP_BASE {
            // SAFETY: called with the lock held, after paging is set up.
            unsafe { list.grow(GROW_STEP as usize) };
        }
    }
    stats()
}

// SAFETY: `alloc` returns blocks of at least `layout.size()` bytes aligned to `layout.align()`
// that do not overlap any other live block (they are cut out of the free list, which only
// holds unused memory), or null. `dealloc` only accepts blocks that `alloc` returned.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = block_size(layout);
        let align = layout.align().max(BLOCK);
        let mut list = self.0.lock();
        // SAFETY: the lock is held; the free list only describes unused heap memory.
        unsafe {
            if let Some(block) = list.take(size, align) {
                return block;
            }
            // Nothing fits: map more pages (enough even for the worst alignment), then retry.
            if list.grow(size + align).is_none() {
                return ptr::null_mut();
            }
            list.take(size, align).unwrap_or(ptr::null_mut())
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let mut list = self.0.lock();
        // SAFETY: the caller (the `alloc` crate) returns a block it got from `alloc` with the
        // same layout, so `pointer..pointer + block_size` is ours to put back.
        unsafe { list.insert(pointer as u64, block_size(layout)) };
        list.used -= block_size(layout);
    }
}

/// Size actually taken for `layout`: rounded up to whole 16-byte blocks.
fn block_size(layout: Layout) -> usize {
    layout.size().max(1).next_multiple_of(BLOCK)
}

impl FreeList {
    /// Cuts `size` bytes aligned to `align` out of the first free block that can hold them.
    ///
    /// # Safety
    ///
    /// The list must be consistent (only unused, mapped heap memory, sorted, not overlapping).
    unsafe fn take(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        let mut link: *mut *mut FreeBlock = &mut self.head;
        // SAFETY: for the whole loop, `link` always points either to `self.head` or to the `next`
        // field of a free block in the list; every block in the list is a valid header.
        unsafe {
            while !(*link).is_null() {
                let block = *link;
                let start = block as u64;
                let end = start + (*block).size as u64;
                let aligned = start.next_multiple_of(align as u64);
                if aligned + size as u64 <= end {
                    // Unlink the block, then return the unused space before and after the
                    // allocation to the list. Both pieces are multiples of 16 bytes.
                    *link = (*block).next;
                    if aligned > start {
                        self.insert(start, (aligned - start) as usize);
                    }
                    let rest = aligned + size as u64;
                    if rest < end {
                        self.insert(rest, (end - rest) as usize);
                    }
                    self.used += size;
                    return Some(aligned as *mut u8);
                }
                link = &mut (*block).next;
            }
        }
        None
    }

    /// Adds the free block `start..start + size` in address order, merging it with the
    /// neighbouring free blocks when they touch.
    ///
    /// # Safety
    ///
    /// The block must be unused, mapped heap memory, 16-byte aligned, a multiple of 16 bytes
    /// long, and must not overlap any block already in the list.
    unsafe fn insert(&mut self, start: u64, size: usize) {
        let mut previous: *mut FreeBlock = ptr::null_mut();
        let mut next = self.head;
        // SAFETY: list pointers are valid headers (see `take`); the new header
        // is written into memory the caller hands over.
        unsafe {
            while !next.is_null() && (next as u64) < start {
                previous = next;
                next = (*next).next;
            }

            let block = start as *mut FreeBlock;
            block.write(FreeBlock { size, next });

            // Merge with the following block if they touch.
            if !next.is_null() && start + size as u64 == next as u64 {
                (*block).size += (*next).size;
                (*block).next = (*next).next;
            }
            // Merge with the previous block if they touch; otherwise link after it.
            if previous.is_null() {
                self.head = block;
            } else if previous as u64 + (*previous).size as u64 == start {
                (*previous).size += (*block).size;
                (*previous).next = (*block).next;
            } else {
                (*previous).next = block;
            }
        }
    }

    /// Maps at least `bytes` more heap and adds it to the free list.
    ///
    /// # Safety
    ///
    /// The lock must be held (it is: this takes `&mut self` from inside the mutex).
    unsafe fn grow(&mut self, bytes: usize) -> Option<()> {
        let bytes = (bytes as u64).max(GROW_STEP).next_multiple_of(FRAME_SIZE);
        if self.end + bytes > HEAP_BASE + HEAP_MAX {
            return None;
        }
        paging::map_new_pages(self.end, bytes / FRAME_SIZE)?;
        let start = self.end;
        self.end += bytes;
        // SAFETY: the range was just mapped and is not in the list yet.
        unsafe { self.insert(start, bytes as usize) };
        Some(())
    }
}
