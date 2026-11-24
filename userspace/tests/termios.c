// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Terminals through a pseudo-terminal (ptmx, devpts): the slave starts locked, the default
// settings, canonical input with editing and echo (ERASE, WERASE, KILL, ECHOCTL, EOF,
// ICRNL), raw input (VMIN, VTIME), Ctrl-C as a signal for the foreground group, job control
// for background groups (SIGTTIN, SIGTTOU with TOSTOP and tcsetpgrp), the window size,
// FIONREAD, and both hangups (the master sees the slave closed, the slave sees the master
// closed).

#define _GNU_SOURCE
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

#include "common.h"

static char slave_path[64];

// Prints `length` bytes, control characters as \xNN.
static void show(const char *what, const char *bytes, long length) {
    printf("%s: %ld \"", what, length);
    for (long i = 0; i < length; i++) {
        unsigned char byte = (unsigned char)bytes[i];
        if (byte < 0x20 || byte >= 0x7f || byte == '"' || byte == '\\') {
            printf("\\x%02x", byte);
        } else {
            putchar(byte);
        }
    }
    printf("\"\n");
    fflush(stdout);
}

// Reads what the master has until nothing more comes for a while (Linux processes input
// and echo asynchronously).
static void show_master(const char *what, int master) {
    char buffer[512];
    long length = 0;
    struct pollfd entry = {.fd = master, .events = POLLIN};
    while (length < (long)sizeof buffer && poll(&entry, 1, 100) == 1 &&
           (entry.revents & POLLIN)) {
        ssize_t count = read(master, buffer + length, sizeof buffer - length);
        if (count <= 0) {
            break;
        }
        length += count;
    }
    show(what, buffer, length);
}

// Types `keys` on the terminal, then reads one canonical line on the slave.
static void line(const char *what, int master, int slave, const char *keys) {
    char buffer[256];
    write(master, keys, strlen(keys));
    ssize_t count = read(slave, buffer, sizeof buffer);
    show(what, buffer, count);
    char echo_name[64];
    snprintf(echo_name, sizeof echo_name, "%s echo", what);
    show_master(echo_name, master);
}

static void set_raw(int slave, int minimum, int time) {
    struct termios settings;
    tcgetattr(slave, &settings);
    settings.c_lflag &= ~(ICANON | ECHO | ISIG);
    settings.c_cc[VMIN] = minimum;
    settings.c_cc[VTIME] = time;
    tcsetattr(slave, TCSANOW, &settings);
}

static void set_canonical(int slave, tcflag_t extra) {
    struct termios settings;
    tcgetattr(slave, &settings);
    settings.c_lflag |= ICANON | ECHO | ISIG | extra;
    tcsetattr(slave, TCSANOW, &settings);
}

static void defaults(int master, int slave) {
    struct termios settings;
    report("tcgetattr slave", tcgetattr(slave, &settings));
    printf("slave flags: iflag %o oflag %o cflag %o lflag %o\n", settings.c_iflag,
           settings.c_oflag, settings.c_cflag, settings.c_lflag);
    printf("slave characters:");
    int characters[] = {VINTR, VQUIT,  VERASE, VKILL,    VEOF,    VTIME,  VMIN,
                        VSTART, VSTOP, VSUSP, VREPRINT, VWERASE, VLNEXT};
    for (unsigned i = 0; i < sizeof characters / sizeof characters[0]; i++) {
        printf(" %d", settings.c_cc[characters[i]]);
    }
    printf("\n");
    report("tcgetattr master", tcgetattr(master, &settings));
    printf("master flags: iflag %o oflag %o cflag %o lflag %o\n", settings.c_iflag,
           settings.c_oflag, settings.c_cflag, settings.c_lflag);
    fflush(stdout);
}

static void canonical(int master, int slave) {
    line("plain", master, slave, "hello\n");
    line("erase", master, slave, "ab\x7f" "c\n");
    line("word erase", master, slave, "foo bar\x17" "baz\n");
    line("kill", master, slave, "abc\x15" "def\n");
    line("control echo", master, slave, "x\x01y\n");
    line("carriage return", master, slave, "hi\r");
    line("literal next", master, slave, "a\x16\x7f" "b\n");
    line("end of line", master, slave, "abc\x04");
    line("end of file", master, slave, "\x04");

    // Two lines typed ahead come one per read.
    write(master, "one\ntwo\n", 8);
    char buffer[64];
    show("first of two", buffer, read(slave, buffer, sizeof buffer));
    int waiting = -1;
    report("FIONREAD", ioctl(slave, FIONREAD, &waiting));
    printf("FIONREAD bytes: %d\n", waiting);
    show("second of two", buffer, read(slave, buffer, sizeof buffer));
    show_master("two lines echo", master);
}

static void raw(int master, int slave) {
    char buffer[64];
    set_raw(slave, 3, 0);
    write(master, "xyz", 3);
    show("raw VMIN 3", buffer, read(slave, buffer, sizeof buffer));
    show_master("raw echo", master);

    set_raw(slave, 0, 0);
    report("raw VMIN 0 VTIME 0, nothing typed", read(slave, buffer, sizeof buffer));
    set_raw(slave, 0, 2);
    report("raw VMIN 0 VTIME 2, nothing typed", read(slave, buffer, sizeof buffer));

    // Switching back to canonical mode: what was typed raw becomes a line of its own.
    set_raw(slave, 1, 0);
    write(master, "ab", 2);
    struct pollfd entry = {.fd = slave, .events = POLLIN};
    report("poll raw input", poll(&entry, 1, 1000));
    set_canonical(slave, 0);
    write(master, "c\n", 2);
    show("raw then canonical", buffer, read(slave, buffer, sizeof buffer));
    show("the next line", buffer, read(slave, buffer, sizeof buffer));
    show_master("raw then canonical echo", master);
}

// Waits for `child` and says how it ended.
static void ended(const char *what, pid_t child) {
    int status = 0;
    waitpid(child, &status, WUNTRACED);
    if (WIFSIGNALED(status)) {
        printf("%s: killed by signal %d\n", what, WTERMSIG(status));
    } else if (WIFSTOPPED(status)) {
        printf("%s: stopped by signal %d\n", what, WSTOPSIG(status));
        kill(child, SIGKILL);
        waitpid(child, &status, 0);
    } else {
        printf("%s: exited with %d\n", what, WEXITSTATUS(status));
    }
    fflush(stdout);
}

// A child in a new session with the slave as its controlling terminal. It writes a byte
// to `ready` once set up, then runs `body` and exits with its result.
static pid_t session_child(int ready, int (*body)(int terminal)) {
    pid_t child = fork();
    if (child == 0) {
        setsid();
        int terminal = open(slave_path, O_RDWR);
        if (terminal < 0) {
            _exit(100);
        }
        write(ready, "r", 1);
        _exit(body(terminal));
    }
    return child;
}

static int read_forever(int terminal) {
    char buffer[16];
    while (read(terminal, buffer, sizeof buffer) >= 0) {
    }
    return 1;
}

static int report_ownership(int terminal) {
    // Reported through the exit status: bit 0 foreground, bit 1 session.
    int foreground = tcgetpgrp(terminal) == getpid();
    int session = tcgetsid(terminal) == getpid();
    return foreground | session << 1;
}

// Runs `body` in a background group of the session (a child of the session leader).
static int (*background_body)(int terminal);
static int in_background(int terminal) {
    pid_t child = fork();
    if (child == 0) {
        setpgid(0, 0);
        _exit(background_body(terminal));
    }
    ended("  background", child);
    return 0;
}

static int background_read(int terminal) {
    char byte;
    return (int)read(terminal, &byte, 1);
}

static int background_read_ignoring(int terminal) {
    signal(SIGTTIN, SIG_IGN);
    char byte;
    ssize_t result = read(terminal, &byte, 1);
    return result == -1 ? errno : 0;
}

static int background_write(int terminal) {
    return (int)write(terminal, "bg", 2);
}

static int background_write_tostop(int terminal) {
    struct termios settings;
    tcgetattr(terminal, &settings);
    settings.c_lflag |= TOSTOP;
    // Changing the settings from the background with SIGTTOU ignored goes through.
    signal(SIGTTOU, SIG_IGN);
    tcsetattr(terminal, TCSANOW, &settings);
    signal(SIGTTOU, SIG_DFL);
    return (int)write(terminal, "bg", 2);
}

static int background_take_foreground(int terminal) {
    signal(SIGTTOU, SIG_IGN);
    if (tcsetpgrp(terminal, getpgrp()) != 0) {
        return 100 + errno;
    }
    return tcgetpgrp(terminal) == getpgrp() ? 7 : 8;
}

static int background_take_foreground_default(int terminal) {
    return tcsetpgrp(terminal, getpgrp());
}

static void job_control(int master) {
    int ready[2];
    pipe(ready);
    char byte;

    pid_t child = session_child(ready[1], report_ownership);
    read(ready[0], &byte, 1);
    ended("controlling terminal of a new session (3: foreground and session)", child);

    child = session_child(ready[1], read_forever);
    read(ready[0], &byte, 1);
    write(master, "\x03", 1);
    ended("Ctrl-C", child);
    show_master("Ctrl-C echo", master);

    struct {
        const char *what;
        int (*body)(int terminal);
    } cases[] = {
        {"background read", background_read},
        {"background read, SIGTTIN ignored (5: EIO)", background_read_ignoring},
        {"background write without TOSTOP", background_write},
        {"background write with TOSTOP", background_write_tostop},
        {"background tcsetpgrp, SIGTTOU ignored (7: done)", background_take_foreground},
        {"background tcsetpgrp", background_take_foreground_default},
    };
    for (unsigned i = 0; i < sizeof cases / sizeof cases[0]; i++) {
        printf("%s\n", cases[i].what);
        fflush(stdout);
        background_body = cases[i].body;
        child = session_child(ready[1], in_background);
        read(ready[0], &byte, 1);
        ended("  session leader", child);
        show_master("  output", master);
    }
    close(ready[0]);
    close(ready[1]);
}

static void window(int master, int slave) {
    struct winsize size = {.ws_row = 24, .ws_col = 80};
    report("TIOCSWINSZ on the master", ioctl(master, TIOCSWINSZ, &size));
    struct winsize read_back = {0};
    report("TIOCGWINSZ on the slave", ioctl(slave, TIOCGWINSZ, &read_back));
    printf("window: %d rows, %d columns\n", read_back.ws_row, read_back.ws_col);
    report("TIOCGPGRP without a controlling terminal", ioctl(slave, TIOCGPGRP, &(pid_t){0}));
}

int main(void) {
    int master = posix_openpt(O_RDWR | O_NOCTTY);
    check("posix_openpt", master >= 0);
    unsigned number = 0;
    report("TIOCGPTN", ioctl(master, TIOCGPTN, &number));
    snprintf(slave_path, sizeof slave_path, "/dev/pts/%u", number);
    int locked = -1;
    report("TIOCGPTLCK", ioctl(master, TIOCGPTLCK, &locked));
    printf("locked at first: %d\n", locked);
    report("open the locked slave", open(slave_path, O_RDWR | O_NOCTTY));
    report("unlockpt", unlockpt(master));
    int slave = open(slave_path, O_RDWR | O_NOCTTY);
    check("open the slave", slave >= 0);
    check("isatty slave", isatty(slave));
    check("isatty master", isatty(master));
    struct stat status;
    fstat(slave, &status);
    check("slave is a character device of major 136", S_ISCHR(status.st_mode) &&
                                                          major(status.st_rdev) == 136);

    defaults(master, slave);
    canonical(master, slave);
    raw(master, slave);
    window(master, slave);
    job_control(master);

    // The slave closed: the master reads EIO and polls as hung up.
    close(slave);
    char buffer[16];
    report("master read after the slave closed", read(master, buffer, sizeof buffer));
    struct pollfd entry = {.fd = master, .events = POLLIN};
    poll(&entry, 1, 0);
    check("master POLLHUP after the slave closed", entry.revents & POLLHUP);

    // The master closed: the slave is hung up.
    slave = open(slave_path, O_RDWR | O_NOCTTY);
    check("slave opened again", slave >= 0);
    close(master);
    report("slave read after the master closed", read(slave, buffer, sizeof buffer));
    report("slave write after the master closed", write(slave, "x", 1));
    report("slave tcgetattr after the master closed",
           tcgetattr(slave, &(struct termios){0}));
    report("open the slave after the master closed", open(slave_path, O_RDWR | O_NOCTTY));
    close(slave);
    return 0;
}
