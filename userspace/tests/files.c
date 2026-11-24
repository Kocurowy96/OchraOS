// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Files and directories that change: open(2) with O_CREAT/O_EXCL/O_TRUNC/O_APPEND, write(2)
// and pwrite(2) through the page cache (holes, page boundaries), ftruncate(2)/truncate(2),
// mkdir(2)/rmdir(2), unlink(2), link(2), symlink(2)/readlink(2), rename(2)/renameat2(2),
// getdents64(2) (through readdir), chmod(2), chown(2) to one's own IDs, umask(2),
// utimensat(2), getcwd(2) in a removed directory, mknod(2) of a regular file.
//
// Everything happens in a new directory under /tmp, removed at the end. Nothing printed
// depends on being root, on the filesystem type (directory sizes, inode numbers) or on the
// order of a directory listing (names are sorted).

#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#include "common.h"

static int compare_names(const void *a, const void *b) {
    return strcmp(*(char *const *)a, *(char *const *)b);
}

// Prints the sorted names in `path` with their types (d for a directory, l for a link, f for
// a regular file).
static void list(const char *path) {
    DIR *directory = opendir(path);
    if (!directory) {
        report("opendir", -1);
        return;
    }
    char *names[64];
    int count = 0;
    struct dirent *entry;
    while ((entry = readdir(directory)) && count < 64) {
        char kind = entry->d_type == DT_DIR ? 'd' : entry->d_type == DT_LNK ? 'l' : 'f';
        char *name = malloc(strlen(entry->d_name) + 3);
        sprintf(name, "%s:%c", entry->d_name, kind);
        names[count++] = name;
    }
    // A second pass from the start gives the same entries.
    rewinddir(directory);
    int again = 0;
    while (readdir(directory)) {
        again++;
    }
    closedir(directory);
    qsort(names, count, sizeof names[0], compare_names);
    printf("list %s (%d, again %d):", path, count, again);
    for (int i = 0; i < count; i++) {
        printf(" %s", names[i]);
        free(names[i]);
    }
    printf("\n");
    fflush(stdout);
}

static long mode_of(const char *path) {
    struct stat info;
    if (lstat(path, &info) != 0) {
        return -1;
    }
    return info.st_mode;
}

static long size_of(const char *path) {
    struct stat info;
    if (lstat(path, &info) != 0) {
        return -1;
    }
    return info.st_size;
}

static long links_of(const char *path) {
    struct stat info;
    if (lstat(path, &info) != 0) {
        return -1;
    }
    return info.st_nlink;
}

int main(void) {
    struct stat info;
    report("stat /tmp", stat("/tmp", &info));
    printf("/tmp mode: %o\n", info.st_mode);

    char directory[] = "/tmp/ochra-files-XXXXXX";
    check("mkdtemp", mkdtemp(directory) != NULL);
    report("chdir into it", chdir(directory));
    umask(022);
    report("umask again gives 022", umask(022));

    // Creating, writing, reading back.
    int fd = open("a", O_RDWR | O_CREAT | O_EXCL, 0666);
    check("create a", fd >= 0);
    printf("a mode: %lo, links %ld, size %ld\n", mode_of("a"), links_of("a"), size_of("a"));
    report("create a again with O_EXCL", open("a", O_RDWR | O_CREAT | O_EXCL, 0666));
    report("write", write(fd, "hello world", 11));
    report("lseek to 0", lseek(fd, 0, SEEK_SET));
    char buffer[16] = {0};
    report("read", read(fd, buffer, sizeof buffer));
    printf("read back: %s\n", buffer);
    report("fstat", fstat(fd, &info));
    printf("size %ld\n", (long)info.st_size);

    // A hole: bytes never written read as zeros.
    report("pwrite at 20", pwrite(fd, "X", 1, 20));
    printf("size after pwrite: %ld\n", size_of("a"));
    char hole[10];
    memset(hole, 0x55, sizeof hole);
    report("pread the hole", pread(fd, hole, 9, 11));
    int zeros = 1;
    for (int i = 0; i < 9; i++) {
        zeros &= hole[i] == 0;
    }
    check("the hole is zeros", zeros);
    report("position is still 11", lseek(fd, 0, SEEK_CUR));

    // Across page boundaries.
    static unsigned char pattern[3 * 4096 + 500];
    for (unsigned i = 0; i < sizeof pattern; i++) {
        pattern[i] = (unsigned char)(i % 251);
    }
    report("pwrite 3 pages at 4000", pwrite(fd, pattern, sizeof pattern, 4000));
    static unsigned char back[sizeof pattern];
    report("pread them", pread(fd, back, sizeof back, 4000));
    check("same bytes", memcmp(pattern, back, sizeof pattern) == 0);
    printf("size: %ld\n", size_of("a"));

    // Truncating: shorter, then longer again (zeros, not the old bytes).
    report("ftruncate to 5", ftruncate(fd, 5));
    report("ftruncate to 5000", ftruncate(fd, 5000));
    memset(back, 0x55, 100);
    report("pread after the old end", pread(fd, back, 100, 3));
    check("hello, then zeros", memcmp(back, "lo\0\0\0", 5) == 0 && back[99] == 0);
    report("truncate by path to 3", truncate("a", 3));
    printf("size: %ld\n", size_of("a"));
    report("ftruncate to -1", ftruncate(fd, -1));
    close(fd);
    fd = open("a", O_RDONLY);
    report("ftruncate a read-only descriptor", ftruncate(fd, 0));
    report("write to a read-only descriptor", write(fd, "x", 1));
    close(fd);

    // Appending.
    fd = open("a", O_WRONLY | O_APPEND);
    report("lseek to 0 before an append", lseek(fd, 0, SEEK_SET));
    report("append", write(fd, "def", 3));
    report("position after the append", lseek(fd, 0, SEEK_CUR));
    report("F_GETFL has O_APPEND", (fcntl(fd, F_GETFL) & O_APPEND) != 0);
    close(fd);
    fd = open("a", O_RDONLY);
    memset(buffer, 0, sizeof buffer);
    report("read", read(fd, buffer, sizeof buffer));
    printf("contents: %s\n", buffer);
    close(fd);
    fd = open("a", O_WRONLY | O_TRUNC);
    printf("size after O_TRUNC: %ld\n", size_of("a"));
    close(fd);
    fd = creat("b", 0600);
    check("creat b with mode 0600", fd >= 0);
    close(fd);
    printf("b mode: %lo\n", mode_of("b"));

    // Directories.
    report("mkdir d", mkdir("d", 0755));
    printf("d mode %lo, links %ld; the parent has %ld links\n", mode_of("d"), links_of("d"),
           links_of("."));
    report("mkdir d again", mkdir("d", 0755));
    report("mkdir e/ with a slash", mkdir("e/", 0700));
    report("mkdir below a file", mkdir("a/x", 0755));
    report("mkdir in a missing directory", mkdir("missing/x", 0755));
    report("open d for writing", open("d", O_WRONLY));
    report("open a as a directory", open("a", O_RDONLY | O_DIRECTORY));
    report("create with a slash", open("new/", O_RDWR | O_CREAT, 0644));
    report("rmdir a file", rmdir("a"));
    report("unlink a directory", unlink("d"));
    report("unlink a file with a slash", unlink("a/"));
    report("rmdir .", rmdir("."));
    report("rmdir ..", rmdir(".."));

    // Hard links.
    report("link a to d/h", link("a", "d/h"));
    struct stat first, second;
    stat("a", &first);
    stat("d/h", &second);
    check("same inode", first.st_ino == second.st_ino && first.st_dev == second.st_dev);
    printf("a has %ld links\n", links_of("a"));
    report("link to an existing name", link("a", "b"));
    report("link a directory", link("d", "d2"));
    report("rmdir a non-empty directory", rmdir("d"));

    // Symbolic links.
    report("symlink s -> d/h", symlink("d/h", "s"));
    char target[64] = {0};
    report("readlink s", readlink("s", target, sizeof target));
    printf("s -> %s, lstat says link: %d, size %ld\n", target, S_ISLNK(mode_of("s")),
           size_of("s"));
    report("stat s follows", stat("s", &info));
    check("s is a regular file", S_ISREG(info.st_mode));
    // A link in the middle of a path: the walk goes on inside its target.
    report("symlink dl -> d", symlink("d", "dl"));
    report("stat dl/h", stat("dl/h", &info));
    check("dl/h is a regular file", S_ISREG(info.st_mode));
    report("stat dl/../d/h", stat("dl/../d/h", &info));
    report("readlink a regular file", readlink("a", target, sizeof target));
    report("symlink loop", symlink("loop", "loop"));
    report("open the loop", open("loop", O_RDONLY));
    report("open s with O_NOFOLLOW", open("s", O_RDONLY | O_NOFOLLOW));
    report("symlink to an empty target", symlink("", "empty"));

    // Renaming.
    report("rename d/h to c", rename("d/h", "c"));
    report("rename d into its own subdirectory", rename("d", "d/inside"));
    report("rename a file over a directory", rename("c", "d"));
    report("rename a directory over a file", rename("d", "c"));
    report("rename with RENAME_NOREPLACE over b",
           syscall(SYS_renameat2, AT_FDCWD, "c", AT_FDCWD, "b", 1));
    report("rename c over b", rename("c", "b"));
    printf("a has %ld links\n", links_of("a"));
    report("rename a missing file", rename("missing", "z"));
    report("rename a to itself through a link", rename("a", "a"));
    report("rename e to d/e", rename("e", "d/e"));
    printf("d has %ld links\n", links_of("d"));
    report("mkdir f", mkdir("f", 0755));
    report("rename f over the non-empty d", rename("f", "d"));
    report("rename d/e over f", rename("d/e", "f"));
    printf("d has %ld links, f is a directory: %d\n", links_of("d"), S_ISDIR(mode_of("f")));
    list(".");
    list("d");

    // Permissions and owners (only what an ordinary user may do too).
    report("chmod a 0751", chmod("a", 0751));
    printf("a mode: %lo\n", mode_of("a"));
    report("access a X_OK", access("a", X_OK));
    report("chmod a 0644", chmod("a", 0644));
    report("access a X_OK", access("a", X_OK));
    report("access a R_OK|W_OK", access("a", R_OK | W_OK));
    report("access missing", access("missing", F_OK));
    report("chown a to oneself", chown("a", getuid(), getgid()));
    report("chown a, keeping both", chown("a", -1, -1));
    stat("a", &info);
    check("owner is the caller", info.st_uid == getuid() && info.st_gid == getgid());
    report("mknod a regular file", mknod("m", S_IFREG | 0640, 0));
    printf("m mode: %lo\n", mode_of("m"));

    // Times: explicit ones; a read leaves the modification time alone. (Whether it updates
    // the access time depends on the host's mount options, `noatime` or `relatime`, so it is
    // not compared.)
    fd = open("a", O_WRONLY);
    report("write to a again", write(fd, "abc", 3));
    close(fd);
    struct timespec times[2] = {{1000, 0}, {2000, 500}};
    report("utimensat", utimensat(AT_FDCWD, "a", times, 0));
    stat("a", &info);
    printf("atime %ld, mtime %ld.%09ld\n", (long)info.st_atim.tv_sec, (long)info.st_mtim.tv_sec,
           info.st_mtim.tv_nsec);
    struct timespec omit[2] = {{0, UTIME_OMIT}, {0, UTIME_OMIT}};
    report("utimensat omitting both", utimensat(AT_FDCWD, "a", omit, 0));
    struct timespec bad[2] = {{0, 1000000000}, {0, 0}};
    report("utimensat with bad nanoseconds", utimensat(AT_FDCWD, "a", bad, 0));
    fd = open("a", O_RDONLY);
    read(fd, buffer, 1);
    fstat(fd, &info);
    check("reading kept mtime", info.st_mtim.tv_sec == 2000);

    // A removed file lives on while open.
    report("unlink a while open", unlink("a"));
    fstat(fd, &info);
    printf("links of the open file: %ld\n", (long)info.st_nlink);
    report("read it still", pread(fd, buffer, 3, 0));
    close(fd);

    // Long names.
    char long_name[300];
    memset(long_name, 'n', 256);
    long_name[256] = 0;
    report("open a 256-byte name", open(long_name, O_RDONLY | O_CREAT, 0644));

    // The current directory removed under us.
    report("mkdir gone", mkdir("gone", 0755));
    report("chdir gone", chdir("gone"));
    report("rmdir it", rmdir("../gone"));
    char cwd[256];
    report("getcwd in a removed directory", syscall(SYS_getcwd, cwd, sizeof cwd));
    report("create in a removed directory", open("x", O_RDWR | O_CREAT, 0644));
    report("chdir ..", chdir(".."));

    // Clean up.
    report("unlink b", unlink("b"));
    report("unlink s", unlink("s"));
    report("unlink loop", unlink("loop"));
    report("unlink dl", unlink("dl"));
    report("unlink m", unlink("m"));
    report("rmdir d", rmdir("d"));
    report("rmdir f", rmdir("f"));
    list(".");
    report("chdir /", chdir("/"));
    report("rmdir the directory", rmdir(directory));
    return 0;
}
