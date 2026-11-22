// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Memory syscalls: brk(2), mmap(2), munmap(2), mprotect(2), and malloc on top of them.
// Addresses differ between systems (ASLR), so only results and distances are printed.

#define _GNU_SOURCE
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#include "common.h"

#define PAGE 4096

int main(void) {
    // brk: musl's sbrk() refuses to grow, so call the syscall directly.
    uintptr_t start = (uintptr_t)syscall(SYS_brk, 0);
    uintptr_t grown = (uintptr_t)syscall(SYS_brk, start + 2 * PAGE);
    report("brk grows by", (long)(grown - start));
    memset((void *)start, 0x5a, 2 * PAGE);
    check("brk memory writable", ((unsigned char *)start)[2 * PAGE - 1] == 0x5a);
    uintptr_t shrunk = (uintptr_t)syscall(SYS_brk, start + PAGE);
    report("brk shrinks to", (long)(shrunk - start));
    uintptr_t regrown = (uintptr_t)syscall(SYS_brk, start + 2 * PAGE);
    check("brk regrown page is zero", ((unsigned char *)start)[PAGE] == 0 && regrown == start + 2 * PAGE);

    unsigned char *area = mmap(NULL, 3 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("mmap anonymous", area != MAP_FAILED);
    check("mmap memory is zero", area[0] == 0 && area[3 * PAGE - 1] == 0);
    memset(area, 0xa5, 3 * PAGE);
    check("mmap memory writable", area[2 * PAGE] == 0xa5);
    check("mmap page aligned", ((uintptr_t)area % PAGE) == 0);
    report("mprotect read-only", mprotect(area, PAGE, PROT_READ));
    check("read-only page still readable", area[0] == 0xa5);
    report("munmap the middle page", munmap(area + PAGE, PAGE));
    report("munmap all (with a hole)", munmap(area, 3 * PAGE));

    report("mmap of 0 bytes", (long)mmap(NULL, 0, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    report("munmap unaligned", munmap((void *)(PAGE + 1), PAGE));
    report("munmap of 0 bytes", munmap(area, 0));
    // musl rounds the address down, so this asks for page 1, which is never mapped.
    report("mprotect an unmapped page", mprotect((void *)(PAGE + 1), PAGE, PROT_READ));
    report("mprotect with unknown bits", mprotect(area, PAGE, 0x100));

    // A guard page (PROT_NONE): reserved, then made usable; its contents start as zero.
    unsigned char *guard = mmap(NULL, PAGE, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("mmap PROT_NONE", guard != MAP_FAILED);
    report("mprotect it read-write", mprotect(guard, PAGE, PROT_READ | PROT_WRITE));
    check("then it is zero", guard[100] == 0);
    guard[100] = 7;
    report("mprotect PROT_NONE and back", mprotect(guard, PAGE, PROT_NONE) | mprotect(guard, PAGE, PROT_READ));
    check("contents kept", guard[100] == 7);
    report("munmap it", munmap(guard, PAGE));

    void *wanted = (void *)0x10000000;
    void *fixed = mmap(wanted, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    check("mmap MAP_FIXED at the given address", fixed == wanted);
    void *again = mmap(wanted, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    report("MAP_FIXED_NOREPLACE over it", again == MAP_FAILED ? -1 : 0);
    report("munmap MAP_FIXED", munmap(fixed, PAGE));
    report("mmap MAP_FIXED below 64 KiB", (long)mmap((void *)PAGE, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0));
    report("mmap of a file (bad fd)", (long)mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE, 99, 0));

    // Stack Clash protection: a hint just below the stack lies in the guard gap below it
    // (Linux's stack_guard_gap, 1 MiB), so mmap must put the mapping somewhere else.
    uintptr_t stack = (uintptr_t)__builtin_frame_address(0) & ~(uintptr_t)(PAGE - 1);
    void *below_stack = (void *)(stack - 256 * 1024);
    void *placed = mmap(below_stack, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("hint in the stack guard gap not used", placed != below_stack);
    check("mapping not within 1 MiB under the stack",
          (uintptr_t)placed + PAGE <= stack - 1024 * 1024 - 256 * 1024 || (uintptr_t)placed > stack);
    munmap(placed, PAGE);

    char *small = malloc(100);
    char *large = malloc(1 << 20);
    check("malloc", small != NULL && large != NULL);
    memset(large, 1, 1 << 20);
    free(small);
    free(large);
    check("free", 1);
    return 0;
}
