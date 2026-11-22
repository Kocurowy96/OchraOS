// SPDX-License-Identifier: MIT OR Apache-2.0
//
// pipe(2), pipe2(2), read(2) and close(2) on pipes: data, end of file, wrong ends, O_NONBLOCK
// (EAGAIN), more data than the pipe holds between two processes (the writer blocks), and a
// child's stdout sent into a pipe with dup2.

#define _GNU_SOURCE
#include <fcntl.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include "common.h"

#define BIG (200 * 1000)

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int fds[2];
    char buffer[256];
    report("pipe", pipe(fds));
    report("write 5 bytes", write(fds[1], "hello", 5));
    report("read up to 100", read(fds[0], buffer, 100));
    report("read from the write end", read(fds[1], buffer, 1));
    report("write to the read end", write(fds[0], "x", 1));
    report("read 0 bytes", read(fds[0], buffer, 0));
    report("close the write end", close(fds[1]));
    report("read after the writer is gone", read(fds[0], buffer, 1));
    report("close the read end", close(fds[0]));
    report("close it again", close(fds[0]));

    report("pipe2 with O_NONBLOCK", pipe2(fds, O_NONBLOCK));
    report("read from an empty pipe", read(fds[0], buffer, 1));
    report("F_GETFL access mode, read end", fcntl(fds[0], F_GETFL) & O_ACCMODE);
    report("F_GETFL access mode, write end", fcntl(fds[1], F_GETFL) & O_ACCMODE);
    check("F_GETFL shows O_NONBLOCK", (fcntl(fds[0], F_GETFL) & O_NONBLOCK) != 0);
    report("F_SETFL clears it", fcntl(fds[0], F_SETFL, 0));
    check("F_GETFL no longer shows it", (fcntl(fds[0], F_GETFL) & O_NONBLOCK) == 0);
    close(fds[0]);
    close(fds[1]);
    report("pipe2 with unknown flags", pipe2(fds, 0x40));
    report("pipe into a bad pointer", syscall(SYS_pipe, 8));

    // More than the pipe holds: the writer must wait for the reader.
    pipe(fds);
    pid_t child = fork();
    if (child == 0) {
        close(fds[0]);
        static char data[BIG];
        for (int i = 0; i < BIG; i++) {
            data[i] = (char)(i * 7);
        }
        long written = write(fds[1], data, BIG);
        _exit(written == BIG ? 0 : 1);
    }
    close(fds[1]);
    long total = 0;
    unsigned long sum = 0;
    for (;;) {
        long count = read(fds[0], buffer, sizeof buffer);
        if (count <= 0) {
            break;
        }
        for (long i = 0; i < count; i++) {
            sum += (unsigned char)buffer[i] * (unsigned long)(total + i + 1);
        }
        total += count;
    }
    close(fds[0]);
    int status = 0;
    waitpid(child, &status, 0);
    report("bytes through the pipe", total);
    report("checksum", (long)sum);
    report("writer's status", WEXITSTATUS(status));

    // A child's stdout redirected into a pipe.
    pipe(fds);
    child = fork();
    if (child == 0) {
        dup2(fds[1], 1);
        close(fds[0]);
        close(fds[1]);
        printf("through the pipe\n");
        _exit(0);
    }
    close(fds[1]);
    long length = read(fds[0], buffer, sizeof buffer - 1);
    buffer[length > 0 ? length : 0] = 0;
    printf("child wrote: %s", buffer);
    close(fds[0]);
    waitpid(child, NULL, 0);
    return 0;
}
