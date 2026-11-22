// SPDX-License-Identifier: MIT OR Apache-2.0
//
// fork(2), wait4(2) (through waitpid), getpid(2), getppid(2): a child's exit status, a child
// killed by a fault, WNOHANG, several children, ECHILD, separate memory after fork, and an
// orphan that gets a new parent.

#define _GNU_SOURCE
#include <sched.h>
#include <sys/wait.h>
#include <unistd.h>

#include "common.h"

static int value = 1;

int main(void) {
    // Unbuffered: otherwise a child would inherit (and print again) the parent's buffer.
    setvbuf(stdout, NULL, _IONBF, 0);
    pid_t me = getpid();
    report("waitpid with no children", waitpid(-1, NULL, 0));

    pid_t child = fork();
    if (child == 0) {
        value = 2;
        check("child: getppid is the parent", getppid() == me);
        check("child: getpid is new", getpid() != me);
        _exit(7);
    }
    int status = 0;
    pid_t waited = waitpid(child, &status, 0);
    // Printed only after the child is done, so the order of lines is always the same.
    check("fork returns the child's ID", child > 0);
    check("waitpid returns that child", waited == child);
    check("WIFEXITED", WIFEXITED(status));
    report("WEXITSTATUS", WEXITSTATUS(status));
    report("parent's memory unchanged", value);

    child = fork();
    if (child == 0) {
        // Through a volatile pointer, so the compiler cannot see the bad address.
        int *volatile bad = (int *)8;
        *bad = 1;
        _exit(0);
    }
    waitpid(child, &status, 0);
    check("faulting child: WIFSIGNALED", WIFSIGNALED(status));
    report("WTERMSIG", WTERMSIG(status));

    // The child waits on a pipe, so it is certainly still running for WNOHANG.
    int fds[2];
    pipe(fds);
    child = fork();
    if (child == 0) {
        close(fds[1]);
        char byte = 0;
        read(fds[0], &byte, 1);
        _exit(byte);
    }
    close(fds[0]);
    report("WNOHANG while the child runs", waitpid(child, &status, WNOHANG));
    char byte = 5;
    write(fds[1], &byte, 1);
    close(fds[1]);
    waitpid(child, &status, 0);
    report("status sent through a pipe", WEXITSTATUS(status));

    report("waitpid on a process that is not a child", waitpid(me, &status, 0));

    for (int i = 0; i < 3; i++) {
        if (fork() == 0) {
            _exit(10 + i);
        }
    }
    int sum = 0;
    for (int i = 0; i < 3; i++) {
        waitpid(-1, &status, 0);
        sum += WEXITSTATUS(status);
    }
    report("sum of three children's statuses", sum);
    report("no children left", waitpid(-1, NULL, WNOHANG));

    // Last: an orphan. Its line is the last output, after this process has exited.
    printf("orphan test\n");
    int ready[2];
    pipe(ready);
    pid_t middle = fork();
    if (middle == 0) {
        if (fork() == 0) {
            pid_t parent = getppid();
            // Only now may the middle process exit, or the parent would already be new.
            write(ready[1], "x", 1);
            while (getppid() == parent) {
                sched_yield();
            }
            printf("grandchild: got a new parent\n");
            _exit(0);
        }
        char byte;
        read(ready[0], &byte, 1);
        _exit(0);
    }
    waitpid(middle, NULL, 0);
    return 0;
}
