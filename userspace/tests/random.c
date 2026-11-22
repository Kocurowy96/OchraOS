// SPDX-License-Identifier: MIT OR Apache-2.0
//
// getrandom(2): full reads, flags, invalid flags (EINVAL), a bad buffer (EFAULT). The bytes
// themselves are random, so only their properties are printed.

#include <string.h>
#include <sys/random.h>

#include "common.h"

int main(void) {
    unsigned char first[64] = {0};
    unsigned char second[64] = {0};
    report("getrandom 64 bytes", getrandom(first, sizeof first, 0));
    report("getrandom GRND_NONBLOCK", getrandom(second, sizeof second, GRND_NONBLOCK));
    check("two reads differ", memcmp(first, second, sizeof first) != 0);
    report("getrandom 0 bytes", getrandom(first, 0, 0));
    report("getrandom with unknown flags", getrandom(first, 8, 0x100));
    report("getrandom into a bad pointer", getrandom((void *)8, 8, 0));
    return 0;
}
