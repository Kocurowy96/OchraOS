// SPDX-License-Identifier: MIT OR Apache-2.0
//
// /proc and /sys: what a process sees about itself (self, stat, status, cmdline, exe, cwd,
// fd, maps), about a stopped and an exited child, and about the system (version, uptime,
// meminfo, cpuinfo, filesystems, mounts, the CPU lists). Only facts that hold on every Linux
// are printed: no numbers that differ between machines or runs.

#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include "common.h"

// Reads a whole file (at most `size - 1` bytes, NUL-terminated); returns its length or -1.
static long slurp(const char *path, char *buffer, long size) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    long length = 0;
    ssize_t count;
    while (length < size - 1 && (count = read(fd, buffer + length, size - 1 - length)) > 0) {
        length += count;
    }
    buffer[length] = 0;
    close(fd);
    return length;
}

static char text[65536];

// The fields of a `stat` line; the name (in parentheses) is field 2. Returns the count.
static int stat_fields(const char *line, char fields[64][64]) {
    const char *open_paren = strchr(line, '(');
    const char *close_paren = strrchr(line, ')');
    if (!open_paren || !close_paren) {
        return 0;
    }
    snprintf(fields[0], 64, "%.*s", (int)(open_paren - line - 1), line);
    snprintf(fields[1], 64, "%.*s", (int)(close_paren - open_paren - 1), open_paren + 1);
    int count = 2;
    const char *cursor = close_paren + 1;
    while (count < 64 && *cursor) {
        while (*cursor == ' ') {
            cursor++;
        }
        if (!*cursor || *cursor == '\n') {
            break;
        }
        int length = (int)strcspn(cursor, " \n");
        snprintf(fields[count++], 64, "%.*s", length, cursor);
        cursor += length;
    }
    return count;
}

static void self_files(void) {
    char link[256];
    char expected[32];
    long length = readlink("/proc/self", link, sizeof link - 1);
    link[length < 0 ? 0 : length] = 0;
    snprintf(expected, sizeof expected, "%d", getpid());
    check("/proc/self is the own pid", strcmp(link, expected) == 0);

    static char fields[64][64];
    slurp("/proc/self/stat", text, sizeof text);
    int count = stat_fields(text, fields);
    printf("stat fields: %d\n", count);
    check("stat pid", atoi(fields[0]) == getpid());
    length = readlink("/proc/self/exe", link, sizeof link - 1);
    link[length < 0 ? 0 : length] = 0;
    const char *base = strrchr(link, '/') ? strrchr(link, '/') + 1 : link;
    check("stat name is the program's file name", strncmp(fields[1], base, 15) == 0);
    printf("stat state: %s\n", fields[2]);
    check("stat ppid, pgrp, session", atoi(fields[3]) == getppid() &&
                                          atoi(fields[4]) == getpgrp() &&
                                          atoi(fields[5]) == getsid(0));
    printf("stat kstkesp %s kstkeip %s wchan %s\n", fields[28], fields[29], fields[34]);
    printf("stat exit_signal %s threads %s\n", fields[37], fields[19]);

    slurp("/proc/self/status", text, sizeof text);
    snprintf(expected, sizeof expected, "\nPid:\t%d\n", getpid());
    check("status Pid", strstr(text, expected) != NULL);
    check("status State running", strstr(text, "\nState:\tR (running)\n") != NULL);
    check("status starts with Name", strncmp(text, "Name:\t", 6) == 0);

    length = slurp("/proc/self/cmdline", text, sizeof text);
    printf("cmdline: %ld bytes, \"%s\"\n", length, text);
    length = slurp("/proc/self/environ", text, sizeof text);
    printf("environ: %ld bytes\n", length);

    chdir("/tmp");
    length = readlink("/proc/self/cwd", link, sizeof link - 1);
    link[length < 0 ? 0 : length] = 0;
    printf("cwd: %s\n", link);
    chdir("/");

    slurp("/proc/self/maps", text, sizeof text);
    check("maps shows the stack", strstr(text, "[stack]") != NULL);

    struct stat status;
    stat("/proc/self/stat", &status);
    printf("stat size %ld, regular %d\n", (long)status.st_size, S_ISREG(status.st_mode));
    lstat("/proc/self/exe", &status);
    check("exe is a symbolic link", S_ISLNK(status.st_mode));
}

static void descriptors(void) {
    int ends[2];
    pipe(ends);
    char path[64];
    char link[256];
    snprintf(path, sizeof path, "/proc/self/fd/%d", ends[0]);
    long length = readlink(path, link, sizeof link - 1);
    link[length < 0 ? 0 : length] = 0;
    check("a pipe's link", strncmp(link, "pipe:[", 6) == 0);
    struct stat status;
    lstat(path, &status);
    printf("read end link permissions %o\n", status.st_mode & 0777);
    snprintf(path, sizeof path, "/proc/self/fd/%d", ends[1]);
    lstat(path, &status);
    printf("write end link permissions %o\n", status.st_mode & 0777);

    int file = open("/proc/version", O_RDONLY);
    snprintf(path, sizeof path, "/proc/self/fd/%d", file);
    length = readlink(path, link, sizeof link - 1);
    link[length < 0 ? 0 : length] = 0;
    printf("an open file's link: %s\n", link);

    DIR *directory = opendir("/proc/self/fd");
    int seen_pipe = 0;
    int seen_file = 0;
    struct dirent *entry;
    while ((entry = readdir(directory))) {
        seen_pipe |= atoi(entry->d_name) == ends[0] && entry->d_type == DT_LNK;
        seen_file |= atoi(entry->d_name) == file;
    }
    closedir(directory);
    check("fd lists the pipe and the file", seen_pipe && seen_file);
    snprintf(path, sizeof path, "/proc/self/fd/%d", 900);
    report("a descriptor that is not open", readlink(path, link, sizeof link));
    close(file);
    close(ends[0]);
    close(ends[1]);
}

// A child's state letter in /proc/<pid>/stat.
static char state_of(pid_t child) {
    char path[64];
    static char fields[64][64];
    snprintf(path, sizeof path, "/proc/%d/stat", child);
    if (slurp(path, text, sizeof text) < 0 || stat_fields(text, fields) < 3) {
        return '?';
    }
    return fields[2][0];
}

static void children(void) {
    pid_t child = fork();
    if (child == 0) {
        for (;;) {
            pause();
        }
    }
    kill(child, SIGSTOP);
    waitpid(child, NULL, WUNTRACED);
    printf("stopped child: %c\n", state_of(child));
    kill(child, SIGKILL);
    // The child becomes a zombie; wait without reaping it.
    char state = '?';
    for (int i = 0; i < 500 && state != 'Z'; i++) {
        state = state_of(child);
        if (state != 'Z') {
            usleep(10000);
        }
    }
    printf("killed child: %c\n", state);
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/cmdline", child);
    printf("zombie cmdline: %ld bytes\n", slurp(path, text, sizeof text));
    char link[64];
    snprintf(path, sizeof path, "/proc/%d/exe", child);
    report("zombie exe", readlink(path, link, sizeof link));
    snprintf(path, sizeof path, "/proc/%d", child);
    struct stat status;
    check("its directory is there", stat(path, &status) == 0 && S_ISDIR(status.st_mode));
    waitpid(child, NULL, 0);
    report("gone after wait", stat(path, &status));

    DIR *directory = opendir("/proc");
    int seen_self = 0;
    struct dirent *entry;
    while ((entry = readdir(directory))) {
        seen_self |= atoi(entry->d_name) == getpid();
    }
    closedir(directory);
    check("/proc lists this process", seen_self);
}

static void system_files(void) {
    slurp("/proc/version", text, sizeof text);
    check("version", strncmp(text, "Linux version ", 14) == 0);
    slurp("/proc/uptime", text, sizeof text);
    double up = 0;
    double idle = -1;
    check("uptime", sscanf(text, "%lf %lf", &up, &idle) == 2 && up > 0 && idle >= 0);
    slurp("/proc/meminfo", text, sizeof text);
    check("meminfo", strncmp(text, "MemTotal:", 9) == 0 && strstr(text, "\nMemFree:") &&
                         strstr(text, " kB\n"));
    slurp("/proc/cpuinfo", text, sizeof text);
    check("cpuinfo", strncmp(text, "processor\t: 0\n", 14) == 0);
    slurp("/proc/filesystems", text, sizeof text);
    check("filesystems", strstr(text, "nodev\tproc\n") && strstr(text, "nodev\ttmpfs\n") &&
                             strstr(text, "nodev\tsysfs\n") &&
                             strstr(text, "nodev\tdevpts\n"));
    slurp("/proc/mounts", text, sizeof text);
    check("mounts", strstr(text, " /proc proc ") && strstr(text, " /sys sysfs ") &&
                        strstr(text, " /dev/pts devpts "));

    const char *lists[] = {"online", "possible", "present"};
    for (int i = 0; i < 3; i++) {
        char path[64];
        snprintf(path, sizeof path, "/sys/devices/system/cpu/%s", lists[i]);
        slurp(path, text, sizeof text);
        int first = -1;
        int last = -1;
        int parts = sscanf(text, "%d-%d", &first, &last);
        printf("cpu %s starts at %d: %s\n", lists[i], first,
               parts >= 1 && text[strlen(text) - 1] == '\n' ? "yes" : "no");
    }
}

int main(void) {
    self_files();
    descriptors();
    children();
    system_files();
    return 0;
}
