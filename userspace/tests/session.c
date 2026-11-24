// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Sessions and process groups: setsid(2), setpgid(2), getpgid(2), getsid(2) and their
// errors (`man 2 setsid`, `man 2 setpgid`). Everything happens in children, whose IDs are
// group IDs of nobody: the program itself may lead its group (in OchraOS the kernel starts
// it as a session leader, on the host it is not).

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include "common.h"

// Runs `body` in a child and waits for it, so its lines come before the next ones.
static void in_child(void (*body)(void)) {
    pid_t child = fork();
    if (child == 0) {
        body();
        fflush(stdout);
        _exit(0);
    }
    waitpid(child, NULL, 0);
}

static void group_leader(void) {
    check("a child is in its parent's session", getsid(0) == getsid(getppid()));
    check("a child is in its parent's group", getpgrp() == getpgid(getppid()));
    report("setpgid(0, 0)", setpgid(0, 0));
    check("now it leads its group", getpgrp() == getpid());
    check("getpgid(0) is getpgrp()", getpgid(0) == getpgrp());
    report("setsid as a group leader", setsid());
}

static void new_session(void) {
    pid_t parent_group = getpgrp();
    check("setsid returns the new session", setsid() == getpid());
    check("it leads its session", getsid(0) == getpid());
    check("and its group", getpgrp() == getpid());
    report("setsid again", setsid());
    report("setpgid of a session leader", setpgid(0, 0));
    report("setpgid into a group of another session", setpgid(0, parent_group));

    pid_t child = fork();
    if (child == 0) {
        report("  child: setpgid(0, 0)", setpgid(0, 0));
        check("  child: leads a new group in the same session",
              getpgrp() == getpid() && getsid(0) == getppid());
        report("  child: back into the leader's group", setpgid(0, getppid()));
        check("  child: in the leader's group", getpgrp() == getppid());
        report("  child: setpgid of its parent", setpgid(getppid(), getppid()));
        report("  child: into a group that does not exist", setpgid(0, 999999));
        fflush(stdout);
        _exit(0);
    }
    waitpid(child, NULL, 0);
}

static void errors(void) {
    report("getpgid of nobody", getpgid(999999));
    report("getsid of nobody", getsid(999999));
    report("setpgid of nobody", setpgid(999999, 0));
    report("setpgid with a negative group", setpgid(0, -1));
    report("setpgid with a negative pid", setpgid(-1, 0));
    check("getsid(0) is getsid(getpid())", getsid(0) == getsid(getpid()));
    check("getpgid(getpid()) is getpgrp()", getpgid(getpid()) == getpgrp());
}

// The parent may move its child to another group until the child runs `execve`.
static void after_exec(const char *self) {
    int go[2];
    int started[2];
    pipe(go);
    pipe(started);
    pid_t child = fork();
    if (child == 0) {
        char byte;
        read(go[0], &byte, 1);
        char fd[16];
        snprintf(fd, sizeof fd, "%d", started[1]);
        execl(self, self, "started", fd, (char *)NULL);
        perror("execl");
        _exit(127);
    }
    // Only the child writes: a failed execve then ends the read below instead of hanging.
    close(started[1]);
    report("setpgid of a child before execve", setpgid(child, child));
    write(go[1], "g", 1);
    char byte;
    read(started[0], &byte, 1);
    report("setpgid of a child after execve", setpgid(child, child));
    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "started") == 0) {
        // Started again by `after_exec`: say so, then wait to be killed.
        write(atoi(argv[2]), "s", 1);
        for (;;) {
            pause();
        }
    }
    in_child(group_leader);
    in_child(new_session);
    in_child(errors);
    after_exec("/proc/self/exe");
    return 0;
}
