// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The stack grows on demand: a large local array (touched from its far end first), deep
// recursion, and a syscall that writes into stack memory the program has not touched yet.

#include <string.h>
#include <sys/random.h>

#include "common.h"

#define ARRAY_SIZE (2 * 1024 * 1024)

// noinline, and the result used, so the compiler keeps every frame.
__attribute__((noinline)) static long recurse(long depth) {
    volatile char frame[512];
    frame[0] = (char)depth;
    if (depth == 0) {
        return frame[0];
    }
    return recurse(depth - 1) + frame[0];
}

__attribute__((noinline)) static int big_array(void) {
    volatile unsigned char array[ARRAY_SIZE];
    array[0] = 1;
    array[ARRAY_SIZE - 1] = 2;
    return array[0] + array[ARRAY_SIZE - 1];
}

__attribute__((noinline)) static long syscall_into_fresh_stack(void) {
    unsigned char buffer[1024 * 1024];
    long result = getrandom(buffer, sizeof buffer, 0);
    unsigned char sum = 0;
    for (size_t i = 0; i < sizeof buffer; i++) {
        sum |= buffer[i];
    }
    return sum != 0 ? result : -2;
}

int main(void) {
    report("recursion 4000 deep", recurse(4000) & 0xff);
    report("2 MiB array on the stack", big_array());
    report("getrandom into 1 MiB of fresh stack", syscall_into_fresh_stack());
    return 0;
}
