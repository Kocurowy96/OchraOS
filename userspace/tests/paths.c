// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Syscalls on paths: open(2)/openat(2), read(2) and lseek(2) and pread(2) on a file, stat(2)
// and its family, access(2)/faccessat(2), getcwd(2)/chdir(2)/fchdir(2), readlink(2), and
// /dev/null. The test reads its own program file (found through AT_EXECFN) and uses paths
// that exist on every Linux system: /, /dev, /dev/null, /bin/sh (a symbolic link).

#define _GNU_SOURCE
#include <fcntl.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#include "common.h"

int main(void) {
    const char *self = (const char *)getauxval(AT_EXECFN);

    // The program's own file: an ELF file, readable from any position.
    int fd = open(self, O_RDONLY | O_CLOEXEC);
    check("open own file", fd >= 0);
    char magic[4] = {0};
    report("read 4 bytes", read(fd, magic, 4));
    check("ELF magic", memcmp(magic, "\x7f" "ELF", 4) == 0);
    report("lseek to 1", lseek(fd, 1, SEEK_SET));
    char byte = 0;
    read(fd, &byte, 1);
    check("byte 1 is 'E'", byte == 'E');
    report("lseek SEEK_CUR +1", lseek(fd, 1, SEEK_CUR));
    off_t end = lseek(fd, 0, SEEK_END);
    struct stat by_fd;
    report("fstat", fstat(fd, &by_fd));
    check("size is the end", by_fd.st_size == end);
    check("regular file", S_ISREG(by_fd.st_mode));
    check("executable", (by_fd.st_mode & 0111) != 0);
    report("read at the end", read(fd, &byte, 1));
    report("lseek past the end", lseek(fd, end + 10, SEEK_SET) == end + 10);
    report("lseek before 0", lseek(fd, -1, SEEK_SET));
    report("lseek with a bad whence", lseek(fd, 0, 7));
    report("pread at 3", pread(fd, &byte, 1, 3));
    check("byte 3 is 'F'", byte == 'F');
    report("pread at a negative offset", pread(fd, &byte, 1, -1));
    report("FD_CLOEXEC set by O_CLOEXEC", fcntl(fd, F_GETFD));
    struct stat by_path;
    report("stat", stat(self, &by_path));
    check("same inode and device", by_path.st_ino == by_fd.st_ino && by_path.st_dev == by_fd.st_dev);
    close(fd);

    // Errors.
    report("open a missing file", open("/no/such/file", O_RDONLY));
    report("open an empty path", open("", O_RDONLY));
    char below_file[4096];
    snprintf(below_file, sizeof below_file, "%s/x", self);
    report("open below a file", open(below_file, O_RDONLY));
    report("open / for writing", open("/", O_WRONLY));
    report("open a file as a directory", open(self, O_RDONLY | O_DIRECTORY));
    report("open with a bad pointer", syscall(SYS_open, 8, O_RDONLY));
    int ends[2];
    pipe(ends);
    report("lseek on a pipe", lseek(ends[0], 0, SEEK_SET));
    struct stat info;
    report("stat a missing file", stat("/no/such/file", &info));
    report("stat to a bad buffer", syscall(SYS_stat, "/", 8));
    report("fstat a closed descriptor", fstat(99, &info));

    // Directories and the current directory.
    report("chdir /", chdir("/"));
    char cwd[256];
    report("getcwd length", syscall(SYS_getcwd, cwd, sizeof cwd));
    printf("getcwd: %s\n", cwd);
    report("getcwd with a small buffer", syscall(SYS_getcwd, cwd, 1));
    report("chdir /dev", chdir("/dev"));
    getcwd(cwd, sizeof cwd);
    printf("getcwd: %s\n", cwd);
    report("stat relative null", stat("null", &info));
    check("null is a character device 1:3",
          S_ISCHR(info.st_mode) && major(info.st_rdev) == 1 && minor(info.st_rdev) == 3);
    report("chdir ..", chdir(".."));
    getcwd(cwd, sizeof cwd);
    printf("getcwd: %s\n", cwd);
    report("chdir to a file", chdir(self));
    report("chdir to a missing directory", chdir("/no/such"));
    int dev = open("/dev", O_RDONLY | O_DIRECTORY);
    check("open /dev as a directory", dev >= 0);
    report("fstat /dev is a directory", fstat(dev, &info) == 0 && S_ISDIR(info.st_mode));
    report("read a directory", read(dev, &byte, 1));
    int null = openat(dev, "null", O_RDWR);
    check("openat /dev null", null >= 0);
    report("read /dev/null", read(null, &byte, 1));
    report("write /dev/null", write(null, "abc", 3));
    report("newfstatat empty path",
           fstatat(dev, "", &info, AT_EMPTY_PATH) == 0 && S_ISDIR(info.st_mode));
    report("openat relative to a file", openat(null, "x", O_RDONLY));
    report("openat absolute ignores dirfd", openat(99, "/dev/null", O_RDONLY) >= 0);
    report("fchdir /dev", fchdir(dev));
    getcwd(cwd, sizeof cwd);
    printf("getcwd: %s\n", cwd);
    report("fchdir to a non-directory", fchdir(null));
    chdir("/");

    // access.
    report("access own file, X_OK", access(self, X_OK));
    report("access own file, R_OK", access(self, R_OK));
    report("access a missing file", access("/no/such/file", F_OK));
    report("access with a bad mode", access(self, 8));
    report("faccessat /dev null", faccessat(dev, "null", R_OK, 0));

    // Symbolic links: /bin/sh is one on every system (to busybox, bash, dash…).
    report("lstat /bin/sh", lstat("/bin/sh", &info));
    check("/bin/sh is a link", S_ISLNK(info.st_mode));
    report("stat /bin/sh follows it", stat("/bin/sh", &info) == 0 && S_ISREG(info.st_mode));
    char target[256];
    ssize_t length = readlink("/bin/sh", target, sizeof target);
    check("readlink /bin/sh gives a name", length > 0);
    report("readlink of a file", readlink(self, target, sizeof target));
    report("readlink with size 0", readlink("/bin/sh", target, 0));
    report("readlinkat missing", readlinkat(dev, "missing", target, sizeof target));
    report("open O_NOFOLLOW on a link", open("/bin/sh", O_RDONLY | O_NOFOLLOW));

    // Identity: root on OchraOS, the test's user on the host; the same either way.
    check("getuid equals geteuid", getuid() == geteuid());
    check("getgid equals getegid", getgid() == getegid());
    return 0;
}
