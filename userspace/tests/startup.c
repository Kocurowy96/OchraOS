// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What a program finds at start: arguments, environment, the auxiliary vector (auxv), and
// thread-local storage (set up by musl through arch_prctl(2) and set_tid_address(2)). `cargo xtask test` runs it on
// the host with the same argv[0] and an empty environment.

#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <sys/auxv.h>
#include <sys/syscall.h>
#include <unistd.h>

#include "common.h"

static __thread int counter = 41;

int main(int argc, char **argv, char **envp) {
    report("argc", argc);
    printf("argv[0]: %s\n", argv[0]);
    long environment = 0;
    while (envp[environment] != NULL) {
        environment++;
    }
    report("environment variables", environment);

    report("AT_PAGESZ", (long)getauxval(AT_PAGESZ));
    report("AT_SECURE", (long)getauxval(AT_SECURE));
    check("AT_PHDR set", getauxval(AT_PHDR) != 0);
    report("AT_PHENT", (long)getauxval(AT_PHENT));
    check("AT_PHNUM set", getauxval(AT_PHNUM) != 0);
    check("AT_ENTRY set", getauxval(AT_ENTRY) != 0);
    check("AT_RANDOM set", getauxval(AT_RANDOM) != 0);
    check("AT_EXECFN set", getauxval(AT_EXECFN) != 0);

    check("stack aligned to 16 bytes in main", ((uintptr_t)__builtin_frame_address(0) % 16) == 0);
    counter++;
    report("thread-local counter", counter);

    // musl's thread pointer is the FS base, and points to the thread's own descriptor.
    unsigned long fs = 0;
    report("ARCH_GET_FS", syscall(SYS_arch_prctl, 0x1003, &fs));
    check("FS base is the thread pointer", fs == (unsigned long)pthread_self());
    report("ARCH_SET_FS to a kernel address", syscall(SYS_arch_prctl, 0x1002, 0xffff800000000000UL));
    report("ARCH_GET_FS into a bad pointer", syscall(SYS_arch_prctl, 0x1003, 8));
    report("unknown arch_prctl code", syscall(SYS_arch_prctl, 0x1999, 0));
    counter++;
    report("TLS still works", counter);
    check("set_tid_address gives a thread ID", syscall(SYS_set_tid_address, NULL) > 0);
    return 3;
}
