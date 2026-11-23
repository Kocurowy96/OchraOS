// SPDX-License-Identifier: MIT OR Apache-2.0
//
// uname(2): the fields that are the same on every x86-64 Linux system (the system name and
// the machine); the release and version differ between systems, so only their shape is
// checked (a release starting with a number, every field NUL-terminated).

#define _GNU_SOURCE
#include <ctype.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <unistd.h>

#include "common.h"

int main(void) {
    struct utsname name;
    memset(&name, 0xFF, sizeof name);
    report("uname", uname(&name));
    printf("sysname: %s\n", name.sysname);
    printf("machine: %s\n", name.machine);
    check("release starts with a number", isdigit((unsigned char)name.release[0]));
    check("every field ends within its 65 bytes",
          memchr(name.sysname, 0, sizeof name.sysname) && memchr(name.nodename, 0, sizeof name.nodename) &&
              memchr(name.release, 0, sizeof name.release) && memchr(name.version, 0, sizeof name.version) &&
              memchr(name.machine, 0, sizeof name.machine) &&
              memchr(name.domainname, 0, sizeof name.domainname));
    report("uname with a bad pointer", syscall(SYS_uname, 8));
    return 0;
}
