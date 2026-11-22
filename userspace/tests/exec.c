// SPDX-License-Identifier: MIT OR Apache-2.0
//
// execve(2): errors, then the program runs itself again (found through AT_EXECFN, which on
// the host is its real path) with other arguments and environment; close-on-exec descriptors
// are closed, others stay open, and the process ID stays the same.

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include "common.h"

extern char **environ;

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc >= 5 && strcmp(argv[1], "again") == 0) {
        printf("again: argc %d, argv[0] %s, TEST=%s\n", argc, argv[0], getenv("TEST"));
        check("again: same process ID", getpid() == atoi(argv[2]));
        report("again: close-on-exec descriptor", fcntl(atoi(argv[3]), F_GETFD));
        report("again: other descriptor still open", fcntl(atoi(argv[4]), F_GETFD));
        return 4;
    }
    const char *self = (const char *)getauxval(AT_EXECFN);
    report("execve of a missing file", execve("/no/such/file", argv, environ));
    report("execve with a bad path", syscall(SYS_execve, 8, argv, environ));
    report("execve with a bad argv", syscall(SYS_execve, self, 8, environ));

    pid_t child = fork();
    if (child == 0) {
        int closed[2];
        int kept[2];
        pipe2(closed, O_CLOEXEC);
        pipe(kept);
        char pid[16];
        char closed_fd[16];
        char kept_fd[16];
        snprintf(pid, sizeof pid, "%d", getpid());
        snprintf(closed_fd, sizeof closed_fd, "%d", closed[0]);
        snprintf(kept_fd, sizeof kept_fd, "%d", kept[0]);
        char *arguments[] = {"exec-again", "again", pid, closed_fd, kept_fd, NULL};
        char *environment[] = {"TEST=yes", NULL};
        execve(self, arguments, environment);
        _exit(99);
    }
    int status = 0;
    waitpid(child, &status, 0);
    report("exit status of the new program", WEXITSTATUS(status));
    return 0;
}
