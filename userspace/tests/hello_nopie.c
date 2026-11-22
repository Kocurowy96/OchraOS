// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The same as /bin/hello, but built as a classic static executable at a fixed address (not
// position independent), so the kernel's loader is tested with that kind of ELF file too.

#include <stdio.h>

int main(void) {
    printf("Hello from a fixed address\n");
    return 0;
}
