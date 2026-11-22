// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The first program the kernel starts (/init). For now a small stand-in until busybox (P4):
// it runs /bin/hello, waits for it, then echoes what is typed on the console, line by line,
// and collects any orphaned processes it inherits. It must never exit: the kernel panics if
// init ends, as Linux does.

#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static void run(const char *path) {
    pid_t child = fork();
    if (child < 0) {
        printf("init: fork failed\n");
        return;
    }
    if (child == 0) {
        char *argv[] = {(char *)path, NULL};
        execve(path, argv, environ);
        _exit(127);
    }
    int status = 0;
    waitpid(child, &status, 0);
    if (WIFEXITED(status)) {
        printf("init: %s exited with status %d\n", path, WEXITSTATUS(status));
    } else {
        printf("init: %s was killed by signal %d\n", path, WTERMSIG(status));
    }
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("init: running as process %d\n", getpid());
    run("/bin/hello");
    for (;;) {
        printf("init: waiting for input\n");
        char line[256];
        ssize_t length = read(0, line, sizeof line - 1);
        // Orphans that ended meanwhile.
        while (waitpid(-1, NULL, WNOHANG) > 0) {
        }
        if (length <= 0) {
            printf("init: end of input\n");
            continue;
        }
        line[length] = 0;
        line[strcspn(line, "\n")] = 0;
        printf("init: got \"%s\"\n", line);
    }
}
