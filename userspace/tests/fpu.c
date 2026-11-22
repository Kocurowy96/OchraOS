// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The x87/SSE state belongs to each process: four children and the parent each fill all
// sixteen XMM registers with their own pattern and set their own MXCSR rounding mode, then
// give the CPU away a few thousand times (sched_yield) and check that nothing changed. A
// kernel that forgot to save and restore that state on task switches would mix them up.

#define _GNU_SOURCE
#include <sys/wait.h>
#include <unistd.h>

#include "common.h"

// Returns 0 if the registers and MXCSR kept their values across `rounds` yields.
static int hold_registers(unsigned pattern, unsigned mxcsr, long rounds) {
    unsigned before = mxcsr;
    unsigned after = 0;
    int mismatch = 0;
    __asm__ volatile(
        "ldmxcsr %[before]\n"
        "movd %[pattern], %%xmm0\n"
        "pshufd $0, %%xmm0, %%xmm0\n"
        "movdqa %%xmm0, %%xmm1\n movdqa %%xmm0, %%xmm2\n movdqa %%xmm0, %%xmm3\n"
        "movdqa %%xmm0, %%xmm4\n movdqa %%xmm0, %%xmm5\n movdqa %%xmm0, %%xmm6\n"
        "movdqa %%xmm0, %%xmm7\n movdqa %%xmm0, %%xmm8\n movdqa %%xmm0, %%xmm9\n"
        "movdqa %%xmm0, %%xmm10\n movdqa %%xmm0, %%xmm11\n movdqa %%xmm0, %%xmm12\n"
        "movdqa %%xmm0, %%xmm13\n movdqa %%xmm0, %%xmm14\n movdqa %%xmm0, %%xmm15\n"
        "1:\n"
        "mov $24, %%eax\n"  // sched_yield
        "syscall\n"
        "dec %[rounds]\n"
        "jnz 1b\n"
        "stmxcsr %[after]\n"
        // XOR every register with xmm0 and OR the results: zero if all equal xmm0.
        "pxor %%xmm0, %%xmm1\n pxor %%xmm0, %%xmm2\n pxor %%xmm0, %%xmm3\n"
        "pxor %%xmm0, %%xmm4\n pxor %%xmm0, %%xmm5\n pxor %%xmm0, %%xmm6\n"
        "pxor %%xmm0, %%xmm7\n pxor %%xmm0, %%xmm8\n pxor %%xmm0, %%xmm9\n"
        "pxor %%xmm0, %%xmm10\n pxor %%xmm0, %%xmm11\n pxor %%xmm0, %%xmm12\n"
        "pxor %%xmm0, %%xmm13\n pxor %%xmm0, %%xmm14\n pxor %%xmm0, %%xmm15\n"
        "por %%xmm2, %%xmm1\n por %%xmm3, %%xmm1\n por %%xmm4, %%xmm1\n"
        "por %%xmm5, %%xmm1\n por %%xmm6, %%xmm1\n por %%xmm7, %%xmm1\n"
        "por %%xmm8, %%xmm1\n por %%xmm9, %%xmm1\n por %%xmm10, %%xmm1\n"
        "por %%xmm11, %%xmm1\n por %%xmm12, %%xmm1\n por %%xmm13, %%xmm1\n"
        "por %%xmm14, %%xmm1\n por %%xmm15, %%xmm1\n"
        // xmm0 itself must still hold the pattern.
        "movd %[pattern], %%xmm2\n"
        "pshufd $0, %%xmm2, %%xmm2\n"
        "pxor %%xmm0, %%xmm2\n"
        "por %%xmm2, %%xmm1\n"
        "pxor %%xmm2, %%xmm2\n"
        "pcmpeqb %%xmm2, %%xmm1\n"
        "pmovmskb %%xmm1, %[mismatch]\n"
        "xor $0xffff, %[mismatch]\n"
        : [rounds] "+r"(rounds), [mismatch] "=r"(mismatch), [after] "=m"(after)
        : [pattern] "r"(pattern), [before] "m"(before)
        : "rax", "rcx", "r11", "memory", "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5",
          "xmm6", "xmm7", "xmm8", "xmm9", "xmm10", "xmm11", "xmm12", "xmm13", "xmm14",
          "xmm15");
    // Back to the default rounding mode for the C library.
    unsigned normal = 0x1f80;
    __asm__ volatile("ldmxcsr %0" : : "m"(normal));
    return mismatch != 0 || after != before;
}

#define CHILDREN 4
#define ROUNDS 3000

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    for (int i = 0; i < CHILDREN; i++) {
        if (fork() == 0) {
            // Rounding modes 0..3 live in MXCSR bits 13 and 14.
            _exit(hold_registers(0x11111111u * (unsigned)(i + 1), 0x1f80 | (unsigned)(i % 4) << 13, ROUNDS));
        }
    }
    int parent_bad = hold_registers(0xa5a5a5a5u, 0x1f80 | 3u << 13, ROUNDS);
    int children_ok = 0;
    for (int i = 0; i < CHILDREN; i++) {
        int status = 0;
        wait(&status);
        if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
            children_ok++;
        }
    }
    report("children that kept their registers", children_ok);
    check("parent kept its registers", !parent_bad);
    return 0;
}
