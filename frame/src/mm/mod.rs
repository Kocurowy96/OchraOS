// SPDX-License-Identifier: GPL-2.0-only

//! Memory management: physical frames, page tables (kernel and user), the kernel heap.
//!
//! Physical memory is split into 4 KiB "frames". Before we have our own page tables we reach
//! physical memory through Limine's direct map (HHDM): physical address `p` is visible at
//! virtual address `p + offset`.

pub mod frames;
pub mod heap;
pub mod paging;
pub mod tlb;
pub mod user;

use spin::Once;

/// Size of a frame (and of a normal page).
pub const FRAME_SIZE: u64 = 4096;

/// A physical memory address. A separate type from virtual addresses (plain pointers), so the
/// two cannot be mixed up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhysAddr(pub u64);

/// Virtual address of the direct map of physical memory, set once at boot.
static DIRECT_MAP_OFFSET: Once<u64> = Once::new();

/// Remembers where the direct map is. Called once by `boot::kmain`.
pub(crate) fn set_direct_map_offset(offset: u64) {
    DIRECT_MAP_OFFSET.call_once(|| offset);
}

/// The virtual address through which the kernel can reach physical address `address`.
///
/// Only the pointer is computed here; dereferencing it is up to the (unsafe) caller.
pub(crate) fn direct_map(address: PhysAddr) -> *mut u8 {
    let offset = *DIRECT_MAP_OFFSET
        .get()
        .expect("the direct map offset is set in kmain before anything else runs");
    (offset + address.0) as *mut u8
}
