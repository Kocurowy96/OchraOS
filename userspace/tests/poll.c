// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Waiting for files: poll(2), ppoll(2), select(2), pselect(2) on pipes — readiness, hangup
// and error events, POLLNVAL, EBADF, timeouts and the time left, a signal mask for the
// wait — and a race: an event that comes while poll is still registering with many files
// must wake it (not leave it asleep until the timeout).

#define _GNU_SOURCE
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include "common.h"

static void show_events(const char *what, int result, short revents) {
    printf("%s: %d, revents %#x\n", what, result, (unsigned)(unsigned short)revents);
    fflush(stdout);
}

static int poll_one(int fd, short events, int timeout, short *revents) {
    struct pollfd entry = {.fd = fd, .events = events};
    int result = poll(&entry, 1, timeout);
    *revents = entry.revents;
    return result;
}

static void pipes(void) {
    int ends[2];
    pipe(ends);
    short revents;
    int result = poll_one(ends[0], POLLIN, 0, &revents);
    show_events("empty pipe, read end", result, revents);
    result = poll_one(ends[1], POLLOUT, 0, &revents);
    show_events("empty pipe, write end", result, revents);
    write(ends[1], "x", 1);
    result = poll_one(ends[0], POLLIN | POLLRDNORM, 0, &revents);
    show_events("pipe with data", result, revents);
    result = poll_one(ends[0], POLLOUT, 0, &revents);
    show_events("read end asked for POLLOUT", result, revents);
    close(ends[1]);
    result = poll_one(ends[0], POLLIN, 0, &revents);
    show_events("data, writer gone", result, revents);
    char byte;
    read(ends[0], &byte, 1);
    result = poll_one(ends[0], POLLIN, 0, &revents);
    show_events("no data, writer gone", result, revents);
    result = poll_one(ends[0], 0, 0, &revents);
    show_events("hangup reported without asking", result, revents);
    close(ends[0]);

    pipe(ends);
    close(ends[0]);
    result = poll_one(ends[1], POLLOUT, 0, &revents);
    show_events("write end, reader gone", result, revents);
    close(ends[1]);
}

static void special_entries(void) {
    int ends[2];
    pipe(ends);
    write(ends[1], "x", 1);
    struct pollfd entries[3] = {
        {.fd = 999, .events = POLLIN},
        {.fd = -1, .events = POLLIN},
        {.fd = ends[0], .events = POLLIN},
    };
    int result = poll(entries, 3, 0);
    printf("not open, negative, ready: %d, revents %#x %#x %#x\n", result,
           (unsigned)entries[0].revents, (unsigned)entries[1].revents,
           (unsigned)entries[2].revents);
    report("poll with no entries", poll(NULL, 0, 10));
    report("poll with too many entries", poll(entries, 1 << 21, 0));
    close(ends[0]);
    close(ends[1]);
}

static void timeouts(void) {
    int ends[2];
    pipe(ends);
    short revents;
    int result = poll_one(ends[0], POLLIN, 50, &revents);
    show_events("poll 50 ms, nothing", result, revents);

    struct timespec zero = {0, 0};
    struct pollfd entry = {.fd = ends[0], .events = POLLIN};
    report("ppoll zero timeout", ppoll(&entry, 1, &zero, NULL));

    fd_set readable;
    FD_ZERO(&readable);
    FD_SET(ends[0], &readable);
    struct timeval wait = {0, 50000};
    report("select 50 ms, nothing", select(ends[0] + 1, &readable, NULL, NULL, &wait));
    printf("time left: %ld s %ld us, set still holds the pipe: %d\n", (long)wait.tv_sec,
           (long)wait.tv_usec, FD_ISSET(ends[0], &readable));

    write(ends[1], "x", 1);
    FD_ZERO(&readable);
    FD_SET(ends[0], &readable);
    fd_set writable;
    FD_ZERO(&writable);
    FD_SET(ends[1], &writable);
    FD_SET(ends[0], &writable);
    struct timeval long_wait = {5, 0};
    report("select with data",
           select(ends[1] + 1, &readable, &writable, NULL, &long_wait));
    printf("readable %d, writable: write end %d, read end %d\n", FD_ISSET(ends[0], &readable),
           FD_ISSET(ends[1], &writable), FD_ISSET(ends[0], &writable));
    check("most of the time left", long_wait.tv_sec >= 4);

    FD_ZERO(&readable);
    FD_SET(50, &readable);
    report("select with a closed descriptor", select(51, &readable, NULL, NULL, &wait));
    report("select with a negative count", select(-1, NULL, NULL, NULL, &wait));
    struct timeval negative = {-1, 0};
    report("select with a negative time", select(0, NULL, NULL, NULL, &negative));
    struct timespec bad = {0, 1000000000};
    report("ppoll with nanoseconds out of range", ppoll(&entry, 1, &bad, NULL));
    close(ends[0]);
    close(ends[1]);
}

static volatile int handled;

static void on_signal(int signal) {
    (void)signal;
    handled++;
}

// SIGUSR1 is blocked; pselect and ppoll unblock it for the wait only.
static void signal_masks(void) {
    struct sigaction action = {0};
    action.sa_handler = on_signal;
    sigemptyset(&action.sa_mask);
    sigaction(SIGUSR1, &action, NULL);
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGUSR1);
    sigprocmask(SIG_BLOCK, &blocked, NULL);
    raise(SIGUSR1);
    printf("handled while blocked: %d\n", handled);

    int ends[2];
    pipe(ends);
    fd_set readable;
    FD_ZERO(&readable);
    FD_SET(ends[0], &readable);
    sigset_t open;
    sigemptyset(&open);
    struct timespec wait = {5, 0};
    report("pselect with the signal unblocked",
           pselect(ends[0] + 1, &readable, NULL, NULL, &wait, &open));
    printf("handled: %d\n", handled);
    sigset_t now;
    sigprocmask(SIG_BLOCK, NULL, &now);
    check("blocked again after pselect", sigismember(&now, SIGUSR1));

    raise(SIGUSR1);
    struct pollfd entry = {.fd = ends[0], .events = POLLIN};
    report("ppoll with the signal unblocked", ppoll(&entry, 1, NULL, &open));
    printf("handled: %d\n", handled);
    sigprocmask(SIG_BLOCK, NULL, &now);
    check("blocked again after ppoll", sigismember(&now, SIGUSR1));

    // Nothing pending: the mask is put back at once after a successful wait.
    write(ends[1], "x", 1);
    report("ppoll ready, with a mask", ppoll(&entry, 1, NULL, &open));
    sigprocmask(SIG_BLOCK, NULL, &now);
    check("blocked after a ready ppoll", sigismember(&now, SIGUSR1));
    sigprocmask(SIG_UNBLOCK, &blocked, NULL);
    close(ends[0]);
    close(ends[1]);
}

static double seconds(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return now.tv_sec + now.tv_nsec / 1e9;
}

// Many pipes; a child writes to the last one right away, while the parent is still
// registering with them. Each round must end long before poll's timeout.
static void registration_race(void) {
    enum { PIPES = 64, ROUNDS = 40 };
    int ends[PIPES][2];
    struct pollfd entries[PIPES];
    for (int i = 0; i < PIPES; i++) {
        pipe(ends[i]);
        entries[i] = (struct pollfd){.fd = ends[i][0], .events = POLLIN};
    }
    int woken = 0;
    int ready = 0;
    for (int round = 0; round < ROUNDS; round++) {
        pid_t child = fork();
        if (child == 0) {
            write(ends[PIPES - 1][1], "x", 1);
            _exit(0);
        }
        double start = seconds();
        int result = poll(entries, PIPES, 3000);
        if (seconds() - start < 2.0) {
            woken++;
        }
        if (result == 1 && entries[PIPES - 1].revents == POLLIN) {
            ready++;
        }
        char byte;
        read(ends[PIPES - 1][0], &byte, 1);
        waitpid(child, NULL, 0);
    }
    printf("race: %d of %d rounds ready, %d woken before the timeout\n", ready, ROUNDS,
           woken);
    for (int i = 0; i < PIPES; i++) {
        close(ends[i][0]);
        close(ends[i][1]);
    }
}

int main(void) {
    pipes();
    special_entries();
    timeouts();
    signal_masks();
    registration_race();
    return 0;
}
