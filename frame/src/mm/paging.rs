// SPDX-License-Identifier: GPL-2.0-only

//! The kernel's own page tables.
//!
//! x86-64 translates a virtual address in four steps. The address is split into four 9-bit
//! indexes (plus a 12-bit offset into the 4 KiB page); each index picks one of 512 entries in a
//! table, and each entry points to the next table:
//!
//! ```text
//! bits 47..39   38..30   29..21   20..12   11..0
//!      PML4  ->  PDPT  ->  PD   ->  PT   -> offset in the page
//!     (level 4) (level 3) (level 2) (level 1)
//! ```
//!
//! CR3 holds the physical address of the PML4. A level-2 entry with the "huge" bit maps a
//! whole 2 MiB page directly, which we use for the direct map of physical memory.
//!
//! Every entry also carries permissions: present, writable, user-accessible, no-execute. The
//! most restrictive combination along the walk wins, which is how W^X is enforced.
//!
//! Until now the kernel ran on Limine's page tables. [`init`] builds our own:
//!
//! - the kernel image, segment by segment: code r-x, read-only data r--, data rw-;
//! - the direct map, at the same address Limine chose, so existing pointers stay valid;
//! - kernel stacks with an unmapped guard page below each, so an overflow faults instead of
//!   silently overwriting other memory.
//!
//! Then it switches CR3. Nothing else (no user pages, no device memory) is mapped yet.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::{Mutex, Once};

use super::frames;
use super::{FRAME_SIZE, PhysAddr, direct_map};
use crate::arch::gdt;
use crate::boot::{BootInfo, MemoryKind};

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
/// In a level-2 (or level-3) entry: maps a large page instead of pointing to a table.
const HUGE: u64 = 1 << 7;
/// Kept in the TLB across CR3 switches; for mappings shared by every address space.
const GLOBAL: u64 = 1 << 8;
const NO_EXECUTE: u64 = 1 << 63;
/// Bits 12..51 of an entry: the physical address of the next table or of the page.
const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const HUGE_PAGE_SIZE: u64 = 2 * 1024 * 1024;

/// Permissions of a mapping.
#[derive(Clone, Copy, Debug)]
pub struct Access {
    pub writable: bool,
    pub executable: bool,
    pub user: bool,
}

impl Access {
    /// Kernel code: read and execute.
    pub const CODE: Self = Self {
        writable: false,
        executable: true,
        user: false,
    };
    /// Kernel read-only data.
    pub const READ_ONLY: Self = Self {
        writable: false,
        executable: false,
        user: false,
    };
    /// Kernel data: read and write, never execute (W^X).
    pub const DATA: Self = Self {
        writable: true,
        executable: false,
        user: false,
    };

    fn entry_bits(self) -> u64 {
        let mut bits = PRESENT;
        if self.writable {
            bits |= WRITABLE;
        }
        if !self.executable {
            bits |= NO_EXECUTE;
        }
        if self.user {
            bits |= USER;
        } else {
            bits |= GLOBAL;
        }
        bits
    }
}

/// One set of page tables, identified by its PML4.
struct AddressSpace {
    root: PhysAddr,
    /// Page table frames allocated so far, for the log.
    tables: u64,
}

impl AddressSpace {
    fn new() -> Self {
        Self {
            root: new_table(),
            tables: 1,
        }
    }

    /// Maps the 4 KiB page at `virt` to the frame at `phys`.
    fn map_page(&mut self, virt: u64, phys: PhysAddr, access: Access) {
        let entry = self.walk(virt, 1, access.user);
        // SAFETY: `walk` returns a pointer to an entry inside a live page table frame.
        unsafe { set_entry(entry, phys.0 | access.entry_bits(), virt) };
    }

    /// Maps the 2 MiB page at `virt` (2 MiB aligned) to `phys` (2 MiB aligned).
    fn map_huge_page(&mut self, virt: u64, phys: PhysAddr, access: Access) {
        let entry = self.walk(virt, 2, access.user);
        // SAFETY: `walk` returns a pointer to an entry inside a live page table frame.
        unsafe { set_entry(entry, phys.0 | access.entry_bits() | HUGE, virt) };
    }

    /// Walks from the PML4 down to the table at `level` (1 = PT, 2 = PD), creating missing
    /// tables, and returns a pointer to the entry for `virt` there.
    fn walk(&mut self, virt: u64, level: u32, user: bool) -> *mut u64 {
        let mut table = self.root;
        for current in (level + 1..=4).rev() {
            let entry = entry_pointer(table, virt, current);
            // SAFETY: `entry` points into a live page table frame of this address space, and
            // the address space is only changed under the `KERNEL_SPACE` lock.
            let value = unsafe { entry.read() };
            if value & PRESENT == 0 {
                let next = new_table();
                self.tables += 1;
                // Intermediate entries allow everything; the final entry restricts access.
                let mut bits = PRESENT | WRITABLE;
                if user {
                    bits |= USER;
                }
                // SAFETY: as above.
                unsafe { entry.write(next.0 | bits) };
                table = next;
            } else {
                assert!(
                    value & HUGE == 0,
                    "mapping {virt:#x} inside an existing huge page"
                );
                if user && value & USER == 0 {
                    // SAFETY: as above; adding USER to an intermediate entry only allows
                    // what the final entries allow.
                    unsafe { entry.write(value | USER) };
                }
                table = PhysAddr(value & ADDRESS_MASK);
            }
        }
        entry_pointer(table, virt, level)
    }
}

/// Writes a final (page-mapping) entry, refusing to silently replace an existing mapping.
///
/// # Safety
///
/// `entry` must point to an entry inside a live page table frame.
unsafe fn set_entry(entry: *mut u64, value: u64, virt: u64) {
    // SAFETY: guaranteed by the caller.
    let old = unsafe { entry.read() };
    assert!(old & PRESENT == 0, "{virt:#x} is already mapped");
    // SAFETY: guaranteed by the caller.
    unsafe { entry.write(value) };
}

/// Allocates and zeroes a frame for a page table. Page tables live as long as the kernel.
fn new_table() -> PhysAddr {
    let mut frame = frames::allocate().expect("out of memory for page tables");
    frame.zero();
    frame.into_address()
}

/// Pointer to the entry for `virt` in the table at physical address `table`, at `level`.
fn entry_pointer(table: PhysAddr, virt: u64, level: u32) -> *mut u64 {
    let index = (virt >> (12 + 9 * (level - 1))) & 0x1FF;
    direct_map(table).cast::<u64>().wrapping_add(index as usize)
}

/// The kernel's address space, after [`init`].
static KERNEL_SPACE: Once<Mutex<AddressSpace>> = Once::new();

/// Where kernel stacks go: PML4 slot 509, far from the kernel image (slot 511) and the direct
/// map (Limine puts it in the lower slots of the higher half). The heap uses slot 508.
const STACKS_BASE: u64 = 0xFFFF_FE80_0000_0000;
/// Lowest address of the kernel's own regions (heap and stacks); the direct map must end below.
pub(crate) const KERNEL_REGIONS_BASE: u64 = 0xFFFF_FE00_0000_0000;
/// Virtual space per stack. Only the top part is mapped; everything below stays unmapped, so
/// each stack has at least one guard page under it.
const STACK_SLOT_SIZE: u64 = 64 * 1024;
/// Largest stack that still leaves a guard page in its slot.
const MAX_STACK_PAGES: u64 = STACK_SLOT_SIZE / FRAME_SIZE - 1;
static NEXT_STACK_SLOT: AtomicU64 = AtomicU64::new(0);

/// A kernel stack with an unmapped guard page below it.
///
/// For now stacks live as long as the kernel (the double fault stack; task stacks come with
/// the scheduler, together with freeing).
pub struct KernelStack {
    bottom: u64,
    top: u64,
}

impl KernelStack {
    /// Maps `pages` fresh frames as a new stack.
    pub fn new(pages: u64) -> Self {
        assert!(
            (1..=MAX_STACK_PAGES).contains(&pages),
            "a kernel stack has 1..={MAX_STACK_PAGES} pages"
        );
        let slot = NEXT_STACK_SLOT.fetch_add(1, Ordering::Relaxed);
        let slot_start = STACKS_BASE + slot * STACK_SLOT_SIZE;
        let top = slot_start + STACK_SLOT_SIZE;
        let bottom = top - pages * FRAME_SIZE;

        map_new_pages(bottom, pages).expect("out of memory for a kernel stack");
        Self { bottom, top }
    }

    /// Address just past the highest byte: the initial stack pointer.
    pub fn top(&self) -> u64 {
        self.top
    }

    /// Lowest mapped address; the page below it is the guard page.
    pub fn bottom(&self) -> u64 {
        self.bottom
    }
}

/// Stack of the double fault handler (IST 1), set up by [`init`].
static DOUBLE_FAULT_STACK: Once<KernelStack> = Once::new();

/// What [`init`] did, for the log.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub kernel_pages: u64,
    pub direct_map_bytes: u64,
    pub huge_pages: u64,
    pub page_tables: u64,
}

/// Builds the kernel's page tables and switches to them. Call once, after `frames::init`.
pub fn init(boot: &BootInfo) -> Report {
    let mut report = Report {
        kernel_pages: 0,
        direct_map_bytes: 0,
        huge_pages: 0,
        page_tables: 0,
    };
    let mut space = AddressSpace::new();

    // The kernel image, segment by segment, at the (random) address Limine chose. The
    // physical address of every page is looked up in Limine's page tables, so we do not
    // depend on how Limine laid the image out in physical memory.
    for (start, end, access) in kernel_segments() {
        let mut page = start;
        while page < end {
            let phys = translate_current(page).expect("kernel page not mapped by Limine");
            space.map_page(page, phys, access);
            report.kernel_pages += 1;
            page += FRAME_SIZE;
        }
    }

    // The direct map: every region of RAM the kernel may have to reach, at Limine's offset.
    let offset = boot
        .direct_map_offset()
        .expect("checked in kmain before the kernel starts");
    let mapped_kinds = [
        MemoryKind::Usable,
        MemoryKind::BootloaderReclaimable,
        MemoryKind::KernelAndModules,
        MemoryKind::AcpiReclaimable,
        MemoryKind::AcpiNvs,
    ];
    for region in boot
        .memory_regions()
        .filter(|region| mapped_kinds.contains(&region.kind))
    {
        let mut phys = region.start;
        while phys < region.end() {
            // A 2 MiB page where the region allows it, 4 KiB pages at the unaligned edges.
            if phys % HUGE_PAGE_SIZE == 0 && phys + HUGE_PAGE_SIZE <= region.end() {
                space.map_huge_page(offset + phys, PhysAddr(phys), Access::DATA);
                report.huge_pages += 1;
                phys += HUGE_PAGE_SIZE;
            } else {
                space.map_page(offset + phys, PhysAddr(phys), Access::DATA);
                phys += FRAME_SIZE;
            }
        }
        report.direct_map_bytes += region.length;
        assert!(
            offset + region.end() <= KERNEL_REGIONS_BASE,
            "the direct map reaches into the heap and stack regions"
        );
    }

    let root = space.root;
    report.page_tables = space.tables;
    KERNEL_SPACE.call_once(|| Mutex::new(space));

    // SAFETY: the new tables map everything the kernel uses from now on: its code, data and
    // stack-less statics (the image), the current stack, Limine's answers, the frame bitmap
    // and all page tables (the direct map of usable and bootloader memory), with the same
    // virtual addresses as before. GDT, IDT and TSS are statics in the image.
    unsafe { write_cr3(root) };

    // Move the double fault handler to a stack with a guard page.
    let stack = DOUBLE_FAULT_STACK.call_once(|| KernelStack::new(4));
    // SAFETY: the stack is mapped and writable, lives for ever (in a static `Once`), and only
    // the double fault handler uses it.
    unsafe { gdt::set_interrupt_stack(gdt::DOUBLE_FAULT_IST, stack.top()) };

    report.page_tables = kernel_space().lock().tables;
    report
}

/// Maps `pages` fresh, zeroed frames as kernel data at `start`. Returns `None` when physical
/// memory runs out; the pages mapped before that stay mapped.
pub(crate) fn map_new_pages(start: u64, pages: u64) -> Option<()> {
    let mut space = kernel_space().lock();
    for page in 0..pages {
        let mut frame = frames::allocate()?;
        frame.zero();
        space.map_page(
            start + page * FRAME_SIZE,
            frame.into_address(),
            Access::DATA,
        );
    }
    Some(())
}

fn kernel_space() -> &'static Mutex<AddressSpace> {
    KERNEL_SPACE
        .get()
        .expect("paging::init runs before kernel stacks are created")
}

/// Start, end and permissions of the kernel image segments (from `linker.ld`).
fn kernel_segments() -> [(u64, u64, Access); 3] {
    unsafe extern "C" {
        // SAFETY: these symbols are defined by `linker.ld`; only their addresses are used,
        // never their (meaningless) contents.
        safe static __kernel_text_start: u8;
        safe static __kernel_text_end: u8;
        safe static __kernel_rodata_start: u8;
        safe static __kernel_rodata_end: u8;
        safe static __kernel_data_start: u8;
        safe static __kernel_data_end: u8;
    }
    let address = |symbol: &u8| symbol as *const u8 as u64;
    let page_end = |symbol: &u8| address(symbol).div_ceil(FRAME_SIZE) * FRAME_SIZE;
    [
        (
            address(&__kernel_text_start),
            page_end(&__kernel_text_end),
            Access::CODE,
        ),
        (
            address(&__kernel_rodata_start),
            page_end(&__kernel_rodata_end),
            Access::READ_ONLY,
        ),
        (
            address(&__kernel_data_start),
            page_end(&__kernel_data_end),
            Access::DATA,
        ),
    ]
}

/// Translates `virt` with the page tables currently in CR3. Handles 1 GiB and 2 MiB pages.
fn translate_current(virt: u64) -> Option<PhysAddr> {
    let mut table = PhysAddr(read_cr3() & ADDRESS_MASK);
    for level in (1..=4).rev() {
        // SAFETY: CR3 and every present non-huge entry point to page tables, which are RAM
        // covered by the direct map; we only read.
        let entry = unsafe { entry_pointer(table, virt, level).read() };
        if entry & PRESENT == 0 {
            return None;
        }
        let address = entry & ADDRESS_MASK;
        if level == 1 || entry & HUGE != 0 {
            let page_size = 1u64 << (12 + 9 * (level - 1));
            return Some(PhysAddr(address + (virt & (page_size - 1))));
        }
        table = PhysAddr(address);
    }
    None
}

fn read_cr3() -> u64 {
    let value;
    // SAFETY: reading CR3 has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// # Safety
///
/// `root` must be a PML4 that maps everything the kernel touches from now on.
unsafe fn write_cr3(root: PhysAddr) {
    // SAFETY: guaranteed by the caller. Writing CR3 also flushes the TLB (except global pages).
    unsafe { asm!("mov cr3, {}", in(reg) root.0, options(nostack, preserves_flags)) };
}

/// Maps one user-accessible page with code that just returns, at a fixed low address, for
/// the SMEP/SMAP self-tests. Returns its address.
#[cfg(feature = "selftest")]
pub(crate) fn map_user_test_page() -> u64 {
    const ADDRESS: u64 = 0x1000_0000;
    let mut frame = frames::allocate().expect("out of memory");
    frame.zero();
    // 0xC3 = `ret`, so that a successful call would come straight back.
    frame.write_u64(0, 0xC3);
    let access = Access {
        writable: false,
        executable: true,
        user: true,
    };
    kernel_space()
        .lock()
        .map_page(ADDRESS, frame.into_address(), access);
    ADDRESS
}

/// Lowest address of the double fault stack (the guard page is just below it).
#[cfg(feature = "selftest")]
pub(crate) fn double_fault_stack_bottom() -> u64 {
    DOUBLE_FAULT_STACK
        .get()
        .expect("paging::init has run")
        .bottom()
}
