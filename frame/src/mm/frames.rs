// SPDX-License-Identifier: GPL-2.0-only

//! The physical frame allocator.
//!
//! It keeps one bit per 4 KiB frame of physical memory (a "bitmap"): 1 = in use or not RAM,
//! 0 = free. 256 MiB of RAM need 65536 bits = 8 KiB of bitmap; 64 GiB need 2 MiB. The bitmap
//! itself is stored in the first free region that is large enough.
//!
//! Frames are handed out as [`Frame`] values that *own* the frame: a `Frame` cannot be copied,
//! and dropping it gives the frame back. So safe code cannot free a frame twice or keep using
//! it after freeing it — the usual memory-corrupting bugs of a C kernel.
//!
//! Only `Usable` memory is managed for now. "Bootloader reclaimable" memory still holds Limine's
//! page tables and our boot stack; it can join once we run on our own page tables.

use core::fmt;

use spin::Once;

use super::{FRAME_SIZE, PhysAddr, direct_map};
use crate::boot::{BootInfo, MemoryKind};
use crate::sync::IrqMutex;

/// The allocator, created once by [`init`]. The lock makes it safe to use from several CPUs.
static ALLOCATOR: Once<IrqMutex<Bitmap>> = Once::new();

/// One owned 4 KiB frame of physical memory. Dropping it frees the frame.
pub struct Frame {
    address: PhysAddr,
}

impl Frame {
    /// Physical start address (always a multiple of [`FRAME_SIZE`]).
    pub fn address(&self) -> PhysAddr {
        self.address
    }

    /// Fills the frame with zeros.
    pub fn zero(&mut self) {
        for index in 0..FRAME_SIZE as usize / 8 {
            self.write_u64(index, 0);
        }
    }

    /// Reads the `index`-th 64-bit word of the frame. Panics if `index` is out of the frame.
    pub fn read_u64(&self, index: usize) -> u64 {
        let pointer = self.word(index);
        // SAFETY: `word` checked the bounds; the frame is RAM reachable through the direct map
        // and owned by `self`, so nothing else writes to it at the same time.
        unsafe { pointer.read_volatile() }
    }

    /// Writes the `index`-th 64-bit word of the frame. Panics if `index` is out of the frame.
    pub fn write_u64(&mut self, index: usize, value: u64) {
        let pointer = self.word(index);
        // SAFETY: as in `read_u64`; `&mut self` makes this the only access.
        unsafe { pointer.write_volatile(value) }
    }

    /// Copies `bytes` into the frame at `offset`. Panics if they do not fit.
    pub fn write_bytes(&mut self, offset: usize, bytes: &[u8]) {
        assert!(
            offset
                .checked_add(bytes.len())
                .is_some_and(|end| end <= FRAME_SIZE as usize),
            "{} bytes at offset {offset} do not fit into a frame",
            bytes.len()
        );
        let pointer = direct_map(self.address).wrapping_add(offset);
        // SAFETY: the range is inside the frame (checked above), which is RAM reachable through
        // the direct map and owned by `self`; `&mut self` makes this the only access, and
        // `bytes` cannot overlap it (safe code has no other reference into the frame).
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };
    }

    /// Copies bytes from the frame at `offset` into `buffer`. Panics if they do not fit.
    pub fn read_bytes(&self, offset: usize, buffer: &mut [u8]) {
        assert!(
            offset
                .checked_add(buffer.len())
                .is_some_and(|end| end <= FRAME_SIZE as usize),
            "{} bytes at offset {offset} do not fit into a frame",
            buffer.len()
        );
        let pointer = direct_map(self.address).wrapping_add(offset);
        // SAFETY: as in `write_bytes`; the frame is only read.
        unsafe { core::ptr::copy_nonoverlapping(pointer, buffer.as_mut_ptr(), buffer.len()) };
    }

    /// Takes ownership of the frame at `address` again (the reverse of `into_address`).
    ///
    /// # Safety
    ///
    /// `address` must come from `into_address`, and the frame must not be owned by anything
    /// else (no other `Frame`, no page table entry that is still used).
    pub(crate) unsafe fn from_address(address: PhysAddr) -> Self {
        Self { address }
    }

    /// Gives up ownership without freeing the frame, e.g. because it became a page table or
    /// stack page that lives as long as the kernel. Returns its address.
    pub(crate) fn into_address(self) -> PhysAddr {
        let address = self.address;
        core::mem::forget(self);
        address
    }

    fn word(&self, index: usize) -> *mut u64 {
        assert!(
            index < FRAME_SIZE as usize / 8,
            "word {index} is outside the frame"
        );
        direct_map(self.address).cast::<u64>().wrapping_add(index)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        allocator().lock().free(self.address);
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Frame({:#x})", self.address.0)
    }
}

/// Allocates one frame, or returns `None` when physical memory is exhausted.
/// The contents are whatever was there before; call [`Frame::zero`] if that matters.
pub fn allocate() -> Option<Frame> {
    allocator()
        .lock()
        .allocate()
        .map(|address| Frame { address })
}

/// Counts of frames, for the log and for tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Frames the allocator manages (usable RAM minus the bitmap itself).
    pub total: u64,
    pub free: u64,
}

pub fn stats() -> Stats {
    let bitmap = allocator().lock();
    Stats {
        total: bitmap.total,
        free: bitmap.free,
    }
}

/// Builds the allocator from the memory map. Call once, early; later calls do nothing.
pub fn init(boot: &BootInfo) -> Stats {
    ALLOCATOR.call_once(|| IrqMutex::new(Bitmap::new(boot)));
    stats()
}

fn allocator() -> &'static IrqMutex<Bitmap> {
    ALLOCATOR
        .get()
        .expect("frames::init is called before the first allocation")
}

struct Bitmap {
    /// One bit per frame, starting at physical address 0. Lives in RAM taken from the memory
    /// map, reached through the direct map.
    words: &'static mut [u64],
    /// Where the next search starts ("next fit"): saves rescanning the full words at the start.
    next_word: usize,
    total: u64,
    free: u64,
}

impl Bitmap {
    fn new(boot: &BootInfo) -> Self {
        let usable = || {
            boot.memory_regions()
                .filter(|region| region.kind == MemoryKind::Usable)
        };
        // Frames from 0 up to the end of the highest usable region.
        let frame_count = usable().map(|region| region.end()).max().unwrap_or(0) / FRAME_SIZE;
        let word_count = frame_count.div_ceil(64) as usize;
        let bitmap_bytes = (word_count * 8) as u64;
        let bitmap_frames = bitmap_bytes.div_ceil(FRAME_SIZE);

        // The bitmap goes at the start of the first usable region that can hold it.
        let home = usable()
            .find(|region| align_up(region.start) + bitmap_frames * FRAME_SIZE <= region.end())
            .expect("no usable memory region is large enough for the frame bitmap");
        let bitmap_start = PhysAddr(align_up(home.start));

        // SAFETY: the bitmap's frames are usable RAM according to the memory map, nothing else
        // uses them (they are marked in use below, before any allocation), and Limine's direct
        // map covers all usable memory. `Once` in `init` makes this the only reference.
        let words = unsafe {
            core::slice::from_raw_parts_mut(direct_map(bitmap_start).cast::<u64>(), word_count)
        };
        // Everything starts "in use"; only usable RAM is then marked free.
        words.fill(u64::MAX);
        let mut bitmap = Self {
            words,
            next_word: 0,
            total: 0,
            free: 0,
        };

        for region in usable() {
            let first = align_up(region.start) / FRAME_SIZE;
            let end = region.end() / FRAME_SIZE;
            for frame in first..end {
                bitmap.set(frame, false);
                bitmap.total += 1;
            }
        }
        // Take back the bitmap's own frames, and frame 0: a physical address 0 is too easily
        // confused with "no address" to ever hand out.
        let bitmap_first = bitmap_start.0 / FRAME_SIZE;
        for frame in (bitmap_first..bitmap_first + bitmap_frames).chain([0]) {
            if !bitmap.is_set(frame) {
                bitmap.set(frame, true);
                bitmap.total -= 1;
            }
        }
        bitmap.free = bitmap.total;
        bitmap
    }

    fn allocate(&mut self) -> Option<PhysAddr> {
        let words = self.words.len();
        for step in 0..words {
            let index = (self.next_word + step) % words;
            let word = self.words[index];
            if word != u64::MAX {
                // The lowest 0 bit is the first free frame in this word.
                let bit = (!word).trailing_zeros() as u64;
                let frame = index as u64 * 64 + bit;
                self.set(frame, true);
                self.free -= 1;
                self.next_word = index;
                return Some(PhysAddr(frame * FRAME_SIZE));
            }
        }
        None
    }

    fn free(&mut self, address: PhysAddr) {
        let frame = address.0 / FRAME_SIZE;
        // `Frame` ownership makes a double free impossible from safe code; if it happens
        // anyway, the bitmap is corrupt and continuing would hand out a frame twice.
        assert!(self.is_set(frame), "double free of frame {:#x}", address.0);
        self.set(frame, false);
        self.free += 1;
    }

    fn is_set(&self, frame: u64) -> bool {
        self.words[(frame / 64) as usize] & (1 << (frame % 64)) != 0
    }

    fn set(&mut self, frame: u64, in_use: bool) {
        let word = &mut self.words[(frame / 64) as usize];
        if in_use {
            *word |= 1 << (frame % 64);
        } else {
            *word &= !(1 << (frame % 64));
        }
    }
}

fn align_up(address: u64) -> u64 {
    address.div_ceil(FRAME_SIZE) * FRAME_SIZE
}
