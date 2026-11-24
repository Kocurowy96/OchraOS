// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Job control: stop and continue signals, what wait4 reports about them (WUNTRACED,
// WCONTINUED), SIGCHLD for a stopped child (and none with SA_NOCLDSTOP), ignored stop
// signals, SIGKILL for a stopped process, and orphaned process groups: a stopped group
// that becomes orphaned gets SIGHUP and SIGCONT, and an orphaned group is not stopped by
// SIGTSTP.

#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include "common.h"

static volatile int child_signals;
static volatile int last_code;
static volatile int last_status;

static void on_child(int signal, siginfo_t *info, void *context) {
    (void)signal;
    (void)context;
    child_signals++;
    last_code = info->si_code;
    last_status = info->si_status;
}

static void watch_children(int flags) {
    struct sigaction action = {0};
    action.sa_sigaction = on_child;
    // SA_RESTART: a SIGCHLD must not make the next waitpid fail with EINTR.
    action.sa_flags = SA_SIGINFO | SA_RESTART | flags;
    sigemptyset(&action.sa_mask);
    sigaction(SIGCHLD, &action, NULL);
    child_signals = 0;
}

static pid_t start_waiting_child(void) {
    pid_t child = fork();
    if (child == 0) {
        for (;;) {
            pause();
        }
    }
    return child;
}

// Waits for `child` with `options` and says what wait4 reported.
static void wait_for(const char *what, pid_t child, int options) {
    int status = 0;
    pid_t result = waitpid(child, &status, options);
    if (result <= 0) {
        report(what, result);
    } else if (WIFSTOPPED(status)) {
        printf("%s: stopped by %d\n", what, WSTOPSIG(status));
    } else if (WIFCONTINUED(status)) {
        printf("%s: continued\n", what);
    } else if (WIFSIGNALED(status)) {
        printf("%s: killed by %d\n", what, WTERMSIG(status));
    } else {
        printf("%s: exited with %d\n", what, WEXITSTATUS(status));
    }
    fflush(stdout);
}

static void stop_and_continue(void) {
    watch_children(0);
    pid_t child = start_waiting_child();
    kill(child, SIGSTOP);
    wait_for("after SIGSTOP", child, WUNTRACED);
    printf("SIGCHLD: %d, code %d, status %d\n", child_signals, last_code, last_status);
    wait_for("reported only once", child, WUNTRACED | WNOHANG);
    wait_for("not without WUNTRACED", child, WNOHANG);

    kill(child, SIGCONT);
    wait_for("after SIGCONT", child, WCONTINUED);
    // Linux sends this SIGCHLD when the continued child runs again, maybe after waitpid
    // has returned: give it time.
    usleep(100000);
    printf("SIGCHLD: %d, code %d, status %d\n", child_signals, last_code, last_status);

    kill(child, SIGTSTP);
    wait_for("after SIGTSTP", child, WUNTRACED);
    kill(child, SIGKILL);
    wait_for("SIGKILL while stopped", child, 0);
    fflush(stdout);
}

static void no_stop_signal(void) {
    watch_children(SA_NOCLDSTOP);
    pid_t child = start_waiting_child();
    int status = 0;
    kill(child, SIGSTOP);
    wait_for("SA_NOCLDSTOP: stopped", child, WUNTRACED);
    kill(child, SIGCONT);
    wait_for("SA_NOCLDSTOP: continued", child, WCONTINUED);
    printf("SA_NOCLDSTOP: SIGCHLD %d times\n", child_signals);
    kill(child, SIGKILL);
    waitpid(child, &status, 0);
    printf("SA_NOCLDSTOP: SIGCHLD %d times after the exit\n", child_signals);
    fflush(stdout);
    signal(SIGCHLD, SIG_DFL);
}

static void ignored_stop(void) {
    pid_t child = fork();
    if (child == 0) {
        signal(SIGTSTP, SIG_IGN);
        for (;;) {
            pause();
        }
    }
    // Wait until the child ignores SIGTSTP: it shows in /proc… which the test does not
    // rely on, so give it time instead, then check it was not stopped.
    usleep(200000);
    kill(child, SIGTSTP);
    usleep(100000);
    int status = 0;
    wait_for("ignored SIGTSTP", child, WUNTRACED | WNOHANG);
    kill(child, SIGKILL);
    waitpid(child, &status, 0);
}

static int pipe_out;

static void note(int signal) {
    char letter = signal == SIGHUP ? 'H' : signal == SIGCONT ? 'C' : '?';
    write(pipe_out, &letter, 1);
}

// A stopped process group whose last outside parent exits becomes orphaned: it gets SIGHUP
// and then SIGCONT, so it does not stay stopped forever (POSIX, Linux's
// kill_orphaned_pgrp).
static void orphaned_stopped_group(void) {
    int letters[2];
    pipe(letters);
    pid_t middle = fork();
    if (middle == 0) {
        close(letters[0]);
        pid_t stopper = fork();
        if (stopper == 0) {
            pipe_out = letters[1];
            setpgid(0, 0);
            signal(SIGHUP, note);
            signal(SIGCONT, note);
            raise(SIGSTOP);
            write(letters[1], "!", 1);
            _exit(0);
        }
        int status = 0;
        waitpid(stopper, &status, WUNTRACED);
        // Leaving makes the stopped group orphaned.
        _exit(WIFSTOPPED(status) ? 0 : 1);
    }
    close(letters[1]);
    wait_for("the parent of the stopped group", middle, 0);
    char seen[16];
    long length = 0;
    ssize_t count;
    while ((count = read(letters[0], seen + length, sizeof seen - 1 - length)) > 0) {
        length += count;
    }
    seen[length] = 0;
    printf("the orphaned group saw: %s\n", seen);
    fflush(stdout);
    close(letters[0]);
}

// In a group that is orphaned already, SIGTSTP does not stop (only SIGSTOP would).
static void orphaned_group_not_stopped(void) {
    int letters[2];
    pipe(letters);
    pid_t middle = fork();
    if (middle == 0) {
        close(letters[0]);
        // Taken before the fork: the parent may be gone before the child asks.
        pid_t parent = getpid();
        pid_t child = fork();
        if (child == 0) {
            setpgid(0, 0);
            // Wait for the parent to leave.
            while (getppid() == parent) {
                usleep(10000);
            }
            raise(SIGTSTP);
            write(letters[1], "running", 7);
            _exit(0);
        }
        _exit(0);
    }
    close(letters[1]);
    waitpid(middle, NULL, 0);
    char seen[16] = {0};
    long length = 0;
    ssize_t count;
    while ((count = read(letters[0], seen + length, sizeof seen - 1 - length)) > 0) {
        length += count;
    }
    printf("after SIGTSTP in an orphaned group: %s\n", seen);
    fflush(stdout);
    close(letters[0]);
}

int main(void) {
    // Everything runs in a child that leads a new group, with this process (in another
    // group of the same session) as its parent: so the group is not orphaned, on the host
    // (where this program is in its caller's group) as in OchraOS (where the kernel starts
    // it as a session leader without a parent, whose own group is orphaned).
    pid_t runner = fork();
    if (runner == 0) {
        setpgid(0, 0);
        stop_and_continue();
        no_stop_signal();
        ignored_stop();
        orphaned_stopped_group();
        orphaned_group_not_stopped();
        fflush(stdout);
        _exit(0);
    }
    int status = 0;
    waitpid(runner, &status, 0);
    return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}
