// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The first program the kernel starts (/init for now): proves that a static musl binary runs.

#include <stdio.h>

int main(void) {
    printf("Hello from musl\n");
    return 0;
}
