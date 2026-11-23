// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Clocks: clock_gettime(2), clock_getres(2), gettimeofday(2), time(2), nanosleep(2) and
// clock_nanosleep(2). The times themselves differ from run to run, so only their properties
// are printed: the monotonic clock never goes back, a sleep lasts at least as long as asked,
// the wall clock is after 2024, the clocks agree with each other.

#define _GNU_SOURCE
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#include "common.h"

static long long nanoseconds(clockid_t clock) {
    struct timespec now;
    clock_gettime(clock, &now);
    return now.tv_sec * 1000000000LL + now.tv_nsec;
}

int main(void) {
    struct timespec value;
    report("clock_gettime REALTIME", clock_gettime(CLOCK_REALTIME, &value));
    check("after 2024-01-01", value.tv_sec > 1704067200);
    check("nanoseconds in range", value.tv_nsec >= 0 && value.tv_nsec < 1000000000);
    report("clock_gettime MONOTONIC", clock_gettime(CLOCK_MONOTONIC, &value));
    report("clock_gettime BOOTTIME", clock_gettime(CLOCK_BOOTTIME, &value));
    report("clock_gettime MONOTONIC_COARSE", clock_gettime(CLOCK_MONOTONIC_COARSE, &value));
    report("clock_gettime with a bad clock", syscall(SYS_clock_gettime, 99, &value));
    report("clock_gettime to a bad pointer", syscall(SYS_clock_gettime, CLOCK_MONOTONIC, 8));

    long long previous = nanoseconds(CLOCK_MONOTONIC);
    int backwards = 0;
    for (int i = 0; i < 10000; i++) {
        long long now = nanoseconds(CLOCK_MONOTONIC);
        backwards |= now < previous;
        previous = now;
    }
    check("monotonic never goes back", !backwards);

    report("clock_getres MONOTONIC", clock_getres(CLOCK_MONOTONIC, &value));
    check("resolution at most 10 ms", value.tv_sec == 0 && value.tv_nsec <= 10000000);
    report("clock_getres with a null pointer", clock_getres(CLOCK_REALTIME, NULL));

    struct timeval tv;
    report("gettimeofday", gettimeofday(&tv, NULL));
    check("microseconds in range", tv.tv_usec >= 0 && tv.tv_usec < 1000000);
    time_t seconds = time(NULL);
    long long realtime = nanoseconds(CLOCK_REALTIME) / 1000000000LL;
    check("time and CLOCK_REALTIME agree", realtime - seconds <= 1 && realtime >= seconds);
    check("gettimeofday and time agree", seconds - tv.tv_sec <= 1);

    // Sleeps last at least as long as asked (and not absurdly longer).
    long long start = nanoseconds(CLOCK_MONOTONIC);
    struct timespec request = {0, 50000000};
    report("nanosleep 50 ms", nanosleep(&request, NULL));
    long long slept = nanoseconds(CLOCK_MONOTONIC) - start;
    check("slept at least 50 ms", slept >= 50000000);
    check("slept less than 1 s", slept < 1000000000);
    struct timespec bad = {0, 1000000000};
    report("nanosleep with 1e9 nanoseconds", nanosleep(&bad, NULL));
    struct timespec negative = {-1, 0};
    report("nanosleep with a negative time", nanosleep(&negative, NULL));

    struct timespec until;
    clock_gettime(CLOCK_MONOTONIC, &until);
    until.tv_nsec += 30000000;
    if (until.tv_nsec >= 1000000000) {
        until.tv_sec++;
        until.tv_nsec -= 1000000000;
    }
    report("clock_nanosleep absolute", clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &until, NULL));
    struct timespec after;
    clock_gettime(CLOCK_MONOTONIC, &after);
    check("woke after the deadline",
          after.tv_sec > until.tv_sec || (after.tv_sec == until.tv_sec && after.tv_nsec >= until.tv_nsec));
    struct timespec past = {0, 0};
    report("clock_nanosleep in the past", clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &past, NULL));
    return 0;
}
