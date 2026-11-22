// SPDX-License-Identifier: MIT OR Apache-2.0
//
// File descriptor numbers: dup(2), dup2(2), dup3(2), fcntl(2) F_DUPFD, F_DUPFD_CLOEXEC,
// F_GETFD and F_SETFD, and the lowest free number rule. (The descriptor limit is not tested:
// on the host it depends on the user's RLIMIT_NOFILE.)

#define _GNU_SOURCE
#include <fcntl.h>
#include <unistd.h>

#include "common.h"

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int fds[2];
    pipe(fds);
    report("pipe ends", fds[0] * 10 + fds[1]);
    report("dup takes the lowest free number", dup(fds[0]));
    report("dup of a bad fd", dup(99));
    report("dup2 onto itself", dup2(fds[0], fds[0]));
    report("dup2 of a bad fd onto itself", dup2(99, 99));
    report("dup2 to 20", dup2(fds[0], 20));
    report("dup2 over an open fd", dup2(fds[1], 20));
    report("dup3 onto itself", dup3(fds[0], fds[0], 0));
    report("dup3 with unknown flags", dup3(fds[0], 21, 1));
    report("dup3 with O_CLOEXEC", dup3(fds[0], 21, O_CLOEXEC));
    report("F_GETFD after dup3 O_CLOEXEC", fcntl(21, F_GETFD));
    report("F_GETFD after dup2", fcntl(20, F_GETFD));
    report("F_SETFD", fcntl(20, F_SETFD, FD_CLOEXEC));
    report("F_GETFD now", fcntl(20, F_GETFD));
    report("F_DUPFD from 10", fcntl(fds[0], F_DUPFD, 10));
    report("F_DUPFD_CLOEXEC from 10", fcntl(fds[0], F_DUPFD_CLOEXEC, 10));
    report("F_GETFD of that one", fcntl(11, F_GETFD));
    report("F_GETFD of a bad fd", fcntl(99, F_GETFD));
    report("close 10", close(10));
    report("dup takes 10 again", fcntl(fds[0], F_DUPFD, 10));
    report("close a negative fd", close(-1));
    return 0;
}
