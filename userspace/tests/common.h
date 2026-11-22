// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Shared by the syscall test programs. Every test prints only what is the same on every
// correct Linux system (results and errno values, never addresses or random numbers), because
// `cargo xtask test` compares its output on OchraOS with its output on the host Linux.

#ifndef OCHRA_TESTS_COMMON_H
#define OCHRA_TESTS_COMMON_H

#include <errno.h>
#include <stdio.h>

// Prints "<what>: <result>", plus errno when the call failed (returned -1), and flushes, so
// the order of lines never depends on how stdout is buffered.
static inline void report(const char *what, long result) {
    if (result == -1) {
        printf("%s: -1 errno %d\n", what, errno);
    } else {
        printf("%s: %ld\n", what, result);
    }
    fflush(stdout);
}

static inline void check(const char *what, int ok) {
    printf("%s: %s\n", what, ok ? "yes" : "no");
    fflush(stdout);
}

#endif
