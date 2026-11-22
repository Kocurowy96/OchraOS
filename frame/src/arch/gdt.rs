// SPDX-License-Identifier: GPL-2.0-only

//! GDT (Global Descriptor Table) and TSS (Task State Segment).
//!
//! The GDT is a table of "segment descriptors" that the CPU consults whenever a segment
//! register (CS, SS, DS…) is loaded. In 64-bit mode segmentation is mostly switched off —
//! bases and limits are ignored — but the CPU still needs the GDT for three things:
//!
//! - the code segment in CS decides the privilege level (ring 0 kernel, ring 3 user) and
//!   that we run 64-bit code;
//! - `syscall`/`sysret` (stage 2) jump between fixed kernel and user selectors;
//! - it points to the TSS.
//!
//! The TSS once held a whole task's registers; in 64-bit mode it is just a list of stack
//! pointers the CPU switches to on an interrupt. We use one of them (IST 1) for double
//! faults: if the kernel stack overflows, the fault cannot be handled on that same broken
//! stack, so the CPU switches to a separate, known-good one.
//!
//! The layout follows Linux (`arch/x86/include/asm/segment.h`), because user programs can see
//! selector values (e.g. Wine checks CS = 0x23 / 0x33 for 32/64-bit code).
//!
//! For now there is one GDT and one TSS; with SMP (stage 1) every CPU gets its own.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU64, Ordering};

/// Kernel code segment selector (GDT index 2, ring 0). A selector is `index * 8 | ring`.
pub const KERNEL_CODE: u16 = 2 * 8;
/// Kernel data / stack segment selector (GDT index 3, ring 0).
pub const KERNEL_DATA: u16 = 3 * 8;
/// TSS selector (GDT index 8; a TSS descriptor takes two entries, 8 and 9).
const TSS_SELECTOR: u16 = 8 * 8;

/// Number of GDT entries. Index 0 must be empty; 1 (Linux: 32-bit kernel code) and
/// 4–7 (32/64-bit user code and user data, needed from stage 2) are reserved and empty for now.
const ENTRIES: usize = 10;

// A segment descriptor in bits. Only the "access" byte (bits 40–47) and the flags (bits
// 52–55) matter in 64-bit mode; base and limit are ignored, but we fill the limit like Linux.
//
// access: P (present) = 1, DPL (ring) = 0, S (code/data) = 1, then the type:
//   0x9B = present, ring 0, code, readable, accessed
//   0x93 = present, ring 0, data, writable, accessed
// ("accessed" is preset so the CPU never has to write it into the table.)
// flags: 0xA = granularity 4 KiB + L (64-bit code); 0xC = granularity 4 KiB + 32-bit operands.
const KERNEL_CODE_DESCRIPTOR: u64 = 0x00AF_9B00_0000_FFFF;
const KERNEL_DATA_DESCRIPTOR: u64 = 0x00CF_9300_0000_FFFF;

/// The GDT itself. The entries are atomics only because the CPU writes into the table:
/// `ltr` sets the "busy" bit in the TSS descriptor. With a plain array Rust would assume the
/// memory never changes (and could even put it in read-only memory).
static GDT: [AtomicU64; ENTRIES] = [const { AtomicU64::new(0) }; ENTRIES];

static TSS: Tss = Tss(UnsafeCell::new(TaskStateSegment {
    _reserved_1: 0,
    privilege_stacks: [0; 3],
    _reserved_2: 0,
    interrupt_stacks: [0; 7],
    _reserved_3: 0,
    _reserved_4: 0,
    io_map_base: size_of::<TaskStateSegment>() as u16,
}));

/// The TSS, which the CPU reads on its own (on every interrupt that switches stacks), while
/// the kernel occasionally changes a stack pointer in it.
#[repr(transparent)]
struct Tss(UnsafeCell<TaskStateSegment>);

// SAFETY: there is one TSS for the one CPU in use; its fields are only written by
// `set_interrupt_stack`, on that CPU. (With SMP every CPU gets its own TSS.)
unsafe impl Sync for Tss {}

/// Size of the boot-time interrupt stack.
const IST_STACK_SIZE: usize = 16 * 1024;

/// Stack used by the double fault handler (IST 1) until the kernel has its own page tables;
/// then it is replaced by a stack with a guard page (`mm::paging`).
static BOOT_DOUBLE_FAULT_STACK: InterruptStack =
    InterruptStack(UnsafeCell::new([0; IST_STACK_SIZE]));

/// Index of the double fault stack in the TSS, as written into an IDT entry (1-based).
pub const DOUBLE_FAULT_IST: u8 = 1;

/// Memory used only by the CPU as a stack; Rust code never reads or writes it.
#[repr(C, align(16))]
struct InterruptStack(UnsafeCell<[u8; IST_STACK_SIZE]>);

// SAFETY: Rust never accesses the contents, so no data race is possible; only the address is
// shared. `UnsafeCell` makes sure the stack lands in writable memory.
unsafe impl Sync for InterruptStack {}

impl InterruptStack {
    /// Stacks grow down, so the CPU needs the address just past the end.
    fn top(&self) -> u64 {
        self.0.get() as u64 + IST_STACK_SIZE as u64
    }
}

/// The 64-bit TSS, exactly as the CPU expects it (Intel SDM vol. 3, "64-Bit TSS").
#[repr(C, packed(4))]
struct TaskStateSegment {
    _reserved_1: u32,
    /// Stack pointers for entering ring 0–2 from a less privileged ring (used from stage 2).
    privilege_stacks: [u64; 3],
    _reserved_2: u64,
    /// Interrupt Stack Table: IST 1–7.
    interrupt_stacks: [u64; 7],
    _reserved_3: u64,
    _reserved_4: u16,
    /// Offset of the I/O permission bitmap; pointing past the end means "no bitmap".
    io_map_base: u16,
}

/// The operand of `lgdt` / `lidt`: size of the table minus one, and its address.
#[repr(C, packed)]
pub struct DescriptorTablePointer {
    pub limit: u16,
    pub base: u64,
}

/// Fills in the GDT and TSS, then loads them into this CPU.
pub fn init() {
    // SAFETY: the TSS is not loaded yet, so the CPU does not read it, and nothing else writes
    // it (`init` runs once, on the boot CPU).
    unsafe { set_interrupt_stack(DOUBLE_FAULT_IST, BOOT_DOUBLE_FAULT_STACK.top()) };
    let (tss_low, tss_high) = tss_descriptor();

    GDT[usize::from(KERNEL_CODE / 8)].store(KERNEL_CODE_DESCRIPTOR, Ordering::Relaxed);
    GDT[usize::from(KERNEL_DATA / 8)].store(KERNEL_DATA_DESCRIPTOR, Ordering::Relaxed);
    GDT[usize::from(TSS_SELECTOR / 8)].store(tss_low, Ordering::Relaxed);
    GDT[usize::from(TSS_SELECTOR / 8) + 1].store(tss_high, Ordering::Relaxed);

    let pointer = DescriptorTablePointer {
        limit: (size_of::<[AtomicU64; ENTRIES]>() - 1) as u16,
        base: GDT.as_ptr() as u64,
    };

    // SAFETY: the GDT is a static, so it stays valid forever, and it holds valid kernel code
    // and data descriptors at the selectors loaded below. Reloading CS needs a far return:
    // we push the new CS and the address of label 2, and `retfq` pops both. The data segment
    // registers get the kernel data selector; FS and GS get the null selector (their bases
    // will be set through MSRs). Interrupts are off, so nothing can run half-way through.
    unsafe {
        asm!(
            "lgdt [{pointer}]",
            "push {code}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov ss, {data:x}",
            "mov fs, {null:x}",
            "mov gs, {null:x}",
            pointer = in(reg) &pointer,
            code = in(reg) u64::from(KERNEL_CODE),
            data = in(reg) u64::from(KERNEL_DATA),
            null = in(reg) 0u64,
            tmp = out(reg) _,
        );
    }

    // SAFETY: TSS_SELECTOR points to a valid, not yet busy TSS descriptor, and the TSS it
    // describes is a static that lives forever.
    unsafe {
        asm!("ltr {0:x}", in(reg) TSS_SELECTOR, options(nostack, preserves_flags));
    }
}

/// Points IST entry `ist` (1–7, as in the IDT) at a new stack top.
///
/// # Safety
///
/// `top` must be the top of a mapped, writable stack that stays valid for as long as the IST
/// entry points to it and that nothing else uses. Must run on the CPU that owns the TSS.
pub(crate) unsafe fn set_interrupt_stack(ist: u8, top: u64) {
    let index = usize::from(ist) - 1;
    assert!(index < 7, "IST index {ist} out of range 1..=7");
    // SAFETY: `TSS.0.get()` points to the static TSS, so the field pointer is valid; the TSS is
    // packed, so its fields may be unaligned, hence a raw pointer and `write_unaligned`
    // instead of a reference. `index < 7` keeps the write inside `interrupt_stacks`. The TSS
    // is only written on this CPU, and the CPU reads the entry only when delivering an
    // exception through this IST entry; the caller vouches for the stack itself.
    unsafe {
        let entries = addr_of_mut!((*TSS.0.get()).interrupt_stacks).cast::<u64>();
        entries.add(index).write_unaligned(top);
    }
}

/// Builds the two 64-bit halves of a TSS descriptor ("system segment", 16 bytes long,
/// because a 64-bit base does not fit into a normal 8-byte descriptor).
fn tss_descriptor() -> (u64, u64) {
    let base = TSS.0.get() as u64;
    let limit = (size_of::<TaskStateSegment>() - 1) as u64;
    // 0x89 = present, ring 0, type 9 ("available 64-bit TSS").
    let access: u64 = 0x89;

    let low = (limit & 0xFFFF)
        | (base & 0xFF_FFFF) << 16
        | access << 40
        | ((limit >> 16) & 0xF) << 48
        | ((base >> 24) & 0xFF) << 56;
    let high = base >> 32;
    (low, high)
}
