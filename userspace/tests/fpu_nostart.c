// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What a new program finds in its registers, before any C library code runs: its own
// `_start` saves them first. Like Linux, the kernel must hand over a clean state: general
// registers zero, x87 control word 0x037F, MXCSR 0x1F80, x87 and XMM registers zero. Anything
// else could leak another process's data.
//
// No C library here (built with -nostdlib), so output and exit are raw syscalls.

#include <stdint.h>

// What `fxsave` writes: 512 bytes, 16-byte aligned.
static uint8_t state[512] __attribute__((aligned(16)));
// RAX, RBX, RCX, RDX, RSI, RDI, RBP, R8 to R15, in this order.
static uint64_t registers[15];

static long raw_syscall3(long number, long a, long b, long c) {
    long result;
    __asm__ volatile("syscall"
                     : "=a"(result)
                     : "a"(number), "D"(a), "S"(b), "d"(c)
                     : "rcx", "r11", "memory");
    return result;
}

static void print(const char *text) {
    long length = 0;
    while (text[length] != 0) {
        length++;
    }
    raw_syscall3(1, 1, (long)text, length);
}

static void print_check(const char *what, int ok) {
    print(what);
    print(ok ? ": yes\n" : ": no\n");
}

static void print_hex(const char *what, unsigned value) {
    char text[5];
    for (int i = 0; i < 4; i++) {
        text[i] = "0123456789abcdef"[(value >> (12 - 4 * i)) & 0xf];
    }
    text[4] = 0;
    print(what);
    print(": ");
    print(text);
    print("\n");
}

static int all_zero(const uint8_t *bytes, int count) {
    uint8_t any = 0;
    for (int i = 0; i < count; i++) {
        any |= bytes[i];
    }
    return any == 0;
}

__attribute__((used)) static int check(void) {
    print_hex("FCW", state[0] | state[1] << 8);
    print_hex("FSW", state[2] | state[3] << 8);
    print_hex("FTW (abridged)", state[4]);
    print_hex("MXCSR low half", state[24] | state[25] << 8);
    print_check("x87 registers zero", all_zero(state + 32, 128));
    print_check("XMM registers zero", all_zero(state + 160, 256));
    print_check("general registers zero", all_zero((const uint8_t *)registers, sizeof registers));
    return 0;
}

__asm__(".globl _start\n"
        "_start:\n"
        "    fxsave64 state(%rip)\n"
        "    mov %rax, registers+0(%rip)\n"
        "    mov %rbx, registers+8(%rip)\n"
        "    mov %rcx, registers+16(%rip)\n"
        "    mov %rdx, registers+24(%rip)\n"
        "    mov %rsi, registers+32(%rip)\n"
        "    mov %rdi, registers+40(%rip)\n"
        "    mov %rbp, registers+48(%rip)\n"
        "    mov %r8, registers+56(%rip)\n"
        "    mov %r9, registers+64(%rip)\n"
        "    mov %r10, registers+72(%rip)\n"
        "    mov %r11, registers+80(%rip)\n"
        "    mov %r12, registers+88(%rip)\n"
        "    mov %r13, registers+96(%rip)\n"
        "    mov %r14, registers+104(%rip)\n"
        "    mov %r15, registers+112(%rip)\n"
        "    and $-16, %rsp\n"
        "    call check\n"
        "    mov %eax, %edi\n"
        "    mov $60, %eax\n"
        "    syscall\n");
