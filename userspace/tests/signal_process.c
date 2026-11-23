// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Signals between processes: kill(2) ends a child, SIGCHLD tells the parent (with the
// child's status), SIGCHLD ignored leaves no zombie, a write to a pipe nobody reads raises
// SIGPIPE, a signal interrupts a blocking read(2) (EINTR) or restarts it (SA_RESTART), and
// rt_sigsuspend(2) and pause(2) wait for a handler.

#define _GNU_SOURCE
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include "common.h"

static volatile int handled;
static volatile int child_code;
static volatile int child_status;
static volatile pid_t child_pid;

static void on_signal(int signal) {
    (void)signal;
    handled++;
}

static void on_child(int signal, siginfo_t *info, void *context) {
    (void)signal;
    (void)context;
    child_code = info->si_code;
    child_status = info->si_status;
    child_pid = info->si_pid;
}

static void install(int signal, void (*handler)(int), int flags) {
    struct sigaction action = {0};
    action.sa_handler = handler;
    action.sa_flags = flags;
    sigemptyset(&action.sa_mask);
    sigaction(signal, &action, NULL);
}

// A child that sends SIGUSR1 to its parent again and again (giving the CPU away in between)
// until the parent writes to `stop`, so one of them surely comes while the parent is blocked.
static pid_t start_nagging(int stop_reader) {
    pid_t parent = getpid();
    pid_t child = fork();
    if (child == 0) {
        fcntl(stop_reader, F_SETFL, O_NONBLOCK);
        char byte;
        while (read(stop_reader, &byte, 1) != 1) {
            kill(parent, SIGUSR1);
            for (int i = 0; i < 20; i++) {
                sched_yield();
            }
        }
        _exit(0);
    }
    return child;
}

int main(void) {
    // kill ends a child that has no handler.
    pid_t child = fork();
    if (child == 0) {
        for (;;) {
            pause();
        }
    }
    report("kill the child", kill(child, SIGTERM));
    int status = 0;
    waitpid(child, &status, 0);
    check("child killed by SIGTERM", WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM);

    // SIGCHLD with the child's ID and exit status; sigsuspend waits for it.
    struct sigaction action = {0};
    action.sa_sigaction = on_child;
    action.sa_flags = SA_SIGINFO;
    sigemptyset(&action.sa_mask);
    sigaction(SIGCHLD, &action, NULL);
    sigset_t chld;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    sigset_t old;
    sigprocmask(SIG_BLOCK, &chld, &old);
    child = fork();
    if (child == 0) {
        _exit(3);
    }
    report("sigsuspend", sigsuspend(&old));
    check("SIGCHLD from the child", child_pid == child);
    check("si_code is CLD_EXITED", child_code == CLD_EXITED);
    report("si_status", child_status);
    sigset_t now;
    sigprocmask(SIG_BLOCK, NULL, &now);
    check("sigsuspend restored the mask", sigismember(&now, SIGCHLD));
    sigprocmask(SIG_SETMASK, &old, NULL);
    waitpid(child, &status, 0);
    report("exit status", WEXITSTATUS(status));

    // SIGCHLD ignored: no zombie, wait4 finds no child.
    signal(SIGCHLD, SIG_IGN);
    child = fork();
    if (child == 0) {
        _exit(0);
    }
    report("wait4 with SIGCHLD ignored", waitpid(child, &status, 0));
    signal(SIGCHLD, SIG_DFL);

    // A write to a pipe without readers: SIGPIPE ends the writer; ignored, EPIPE.
    int ends[2];
    pipe(ends);
    close(ends[0]);
    child = fork();
    if (child == 0) {
        write(ends[1], "x", 1);
        _exit(0);
    }
    waitpid(child, &status, 0);
    check("writer killed by SIGPIPE", WIFSIGNALED(status) && WTERMSIG(status) == SIGPIPE);
    signal(SIGPIPE, SIG_IGN);
    report("write with SIGPIPE ignored", write(ends[1], "x", 1));
    close(ends[1]);

    // A handler without SA_RESTART: the blocking read fails with EINTR.
    install(SIGUSR1, on_signal, 0);
    int data[2];
    int stop[2];
    pipe(data);
    pipe(stop);
    child = start_nagging(stop[0]);
    char byte;
    report("read interrupted", read(data[0], &byte, 1));
    write(stop[1], "s", 1);
    waitpid(child, &status, 0);
    check("handler ran", handled > 0);

    // With SA_RESTART the read goes on, and gets the data written after the signals.
    install(SIGUSR1, on_signal, SA_RESTART);
    handled = 0;
    child = fork();
    if (child == 0) {
        pid_t parent = getppid();
        for (int i = 0; i < 20; i++) {
            kill(parent, SIGUSR1);
            for (int j = 0; j < 20; j++) {
                sched_yield();
            }
        }
        write(data[1], "d", 1);
        _exit(0);
    }
    report("read restarted", read(data[0], &byte, 1));
    waitpid(child, &status, 0);
    check("handler ran", handled > 0);

    // wait4 is interrupted too, without SA_RESTART.
    install(SIGUSR1, on_signal, 0);
    int done[2];
    pipe(done);
    pid_t waited = fork();
    if (waited == 0) {
        // Lives until the parent writes to `done`.
        close(done[1]);
        read(done[0], &byte, 1);
        _exit(0);
    }
    pipe(stop);
    child = start_nagging(stop[0]);
    report("wait4 interrupted", waitpid(waited, &status, 0));
    write(stop[1], "s", 1);
    waitpid(child, &status, 0);
    close(done[1]);
    report("wait4 afterwards", waitpid(waited, &status, 0) == waited);

    // pause returns after a handler.
    pipe(stop);
    child = start_nagging(stop[0]);
    report("pause", pause());
    write(stop[1], "s", 1);
    waitpid(child, &status, 0);
    return 0;
}
