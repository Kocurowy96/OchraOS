// SPDX-License-Identifier: MIT OR Apache-2.0
//
// write(2) and writev(2) to stdout: byte counts, an empty write, a bad file descriptor
// (EBADF), a bad buffer (EFAULT). And ioctl(2): stdout is not a terminal here (the kernel
// captures it, the host test runs it into a pipe).

#include <sys/ioctl.h>
#include <sys/uio.h>
#include <unistd.h>

#include "common.h"

int main(void) {
    report("write 4 bytes", write(1, "abc\n", 4));
    report("write 0 bytes", write(1, "", 0));
    report("write to fd 99", write(99, "x", 1));
    report("write from a bad pointer", write(1, (const void *)1, 4));

    struct iovec pieces[] = {{"de", 2}, {"", 0}, {"f\n", 2}};
    report("writev 3 pieces", writev(1, pieces, 3));
    report("writev with a bad piece", writev(1, (const struct iovec *)8, 1));
    report("writev of 0 pieces", writev(1, pieces, 0));
    report("writev of too many pieces", writev(1, pieces, 1025));

    struct winsize size;
    report("TIOCGWINSZ on stdout", ioctl(1, TIOCGWINSZ, &size));
    report("TIOCGWINSZ on fd 99", ioctl(99, TIOCGWINSZ, &size));
    return 0;
}
