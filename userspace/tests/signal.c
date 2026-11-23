// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Signals within one process: rt_sigaction(2), rt_sigprocmask(2), rt_sigpending(2),
// kill(2) and tkill (raise), handlers and rt_sigreturn(2) (registers and the x87/SSE state
// come back), SA_RESETHAND and SA_NODEFER, a SIGSEGV handler with the fault address, and
// what execve(2) keeps (ignored signals, the mask) and resets (handlers).

#define _GNU_SOURCE
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include "common.h"

static volatile int handled;
static volatile int last_signal;
static volatile int last_code;
static volatile int sender_is_self;
static volatile int nested;

static void on_signal(int signal, siginfo_t *info, void *context) {
    (void)context;
    handled++;
    last_signal = signal;
    last_code = info->si_code;
    sender_is_self = info->si_pid == getpid();
}

static void install(int signal, void (*handler)(int, siginfo_t *, void *), int flags) {
    struct sigaction action = {0};
    action.sa_sigaction = handler;
    action.sa_flags = SA_SIGINFO | flags;
    sigemptyset(&action.sa_mask);
    report("sigaction", sigaction(signal, &action, NULL));
}

// Raises SIGUSR1 from inside the handler once: with SA_NODEFER it runs again at once (nested),
// without it the second one waits until the first handler returns.
static void on_signal_raising(int signal, siginfo_t *info, void *context) {
    on_signal(signal, info, context);
    if (handled == 1) {
        raise(SIGUSR1);
        nested = handled == 2;
    }
}

static sigjmp_buf recover;
static volatile uintptr_t fault_address;

static void on_segv(int signal, siginfo_t *info, void *context) {
    (void)context;
    last_signal = signal;
    last_code = info->si_code;
    fault_address = (uintptr_t)info->si_addr;
    siglongjmp(recover, 1);
}

// Fills xmm8 and MXCSR, raises SIGUSR2 (whose handler changes both), and checks that they came
// back: the handler gets a clean state, the program its own again after rt_sigreturn.
static void on_clobber(int signal) {
    (void)signal;
    unsigned mxcsr = 0;
    __asm__ volatile("stmxcsr %0" : "=m"(mxcsr));
    // A new handler starts with the default MXCSR, as a new program.
    check("handler: MXCSR is the default", mxcsr == 0x1F80);
    unsigned other = 0x7F80;  // every exception masked, round toward zero
    __asm__ volatile("ldmxcsr %0\n pxor %%xmm8, %%xmm8" : : "m"(other) : "xmm8");
}

static void fpu_survives_handler(void) {
    signal(SIGUSR2, on_clobber);
    unsigned pattern = 0x12345678;
    unsigned before = 0x3F80;  // round toward zero
    unsigned after = 0;
    unsigned differs = 0;
    long pid = getpid();
    __asm__ volatile(
        "ldmxcsr %[before]\n"
        "movd %[pattern], %%xmm8\n"
        "pshufd $0, %%xmm8, %%xmm8\n"
        "movdqa %%xmm8, %%xmm9\n"
        "mov %[pid], %%rdi\n"
        "mov $12, %%esi\n"  // SIGUSR2
        "mov $200, %%eax\n"  // tkill
        "syscall\n"
        "stmxcsr %[after]\n"
        "pxor %%xmm9, %%xmm8\n"
        "movd %%xmm8, %[differs]\n"
        : [after] "=m"(after), [differs] "=r"(differs)
        : [before] "m"(before), [pattern] "r"(pattern), [pid] "r"(pid)
        : "rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11", "xmm8", "xmm9", "memory");
    check("xmm8 kept across the handler", differs == 0);
    check("MXCSR kept across the handler", after == before);
    unsigned standard = 0x1F80;
    __asm__ volatile("ldmxcsr %0" : : "m"(standard));
}

int main(int argc, char **argv) {
    if (argc >= 2 && strcmp(argv[1], "again") == 0) {
        struct sigaction old;
        sigaction(SIGUSR1, NULL, &old);
        check("after execve: handler is back to SIG_DFL", old.sa_handler == SIG_DFL);
        sigaction(SIGUSR2, NULL, &old);
        check("after execve: ignored stays ignored", old.sa_handler == SIG_IGN);
        sigset_t mask;
        sigprocmask(SIG_BLOCK, NULL, &mask);
        check("after execve: mask kept", sigismember(&mask, SIGTERM));
        return 0;
    }

    // Errors.
    struct sigaction action = {0};
    action.sa_handler = SIG_IGN;
    report("sigaction(SIGKILL)", sigaction(SIGKILL, &action, NULL));
    report("sigaction(SIGSTOP)", sigaction(SIGSTOP, &action, NULL));
    report("sigaction(0)", syscall(SYS_rt_sigaction, 0, NULL, NULL, 8));
    report("sigaction(65)", syscall(SYS_rt_sigaction, 65, NULL, NULL, 8));
    report("sigaction with a bad set size", syscall(SYS_rt_sigaction, SIGUSR1, NULL, NULL, 4));
    report("sigaction with a bad pointer", syscall(SYS_rt_sigaction, SIGUSR1, 8, NULL, 8));
    report("sigaction(SIGKILL) query", sigaction(SIGKILL, NULL, &action));
    sigset_t set;
    sigemptyset(&set);
    report("sigprocmask with a bad how", syscall(SYS_rt_sigprocmask, 7, &set, NULL, 8));
    report("kill with signal 65", kill(getpid(), 65));
    report("kill with signal 0 to itself", kill(getpid(), 0));
    report("kill of a missing process", kill(0x7FFFFFF0, SIGTERM));
    report("tgkill with the wrong group", syscall(SYS_tgkill, getpid() + 1, getpid(), 0));

    // A handler, through tkill (raise) and kill.
    install(SIGUSR1, on_signal, 0);
    raise(SIGUSR1);
    report("handled after raise", handled);
    check("signal number", last_signal == SIGUSR1);
    check("si_code is SI_TKILL", last_code == SI_TKILL);
    check("si_pid is the sender", sender_is_self);
    kill(getpid(), SIGUSR1);
    report("handled after kill", handled);
    check("si_code is SI_USER", last_code == SI_USER);

    // Blocked: pending until unblocked; SIGKILL cannot be blocked.
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGKILL);
    sigprocmask(SIG_BLOCK, &set, NULL);
    raise(SIGUSR1);
    raise(SIGUSR1);
    report("handled while blocked", handled);
    sigset_t pending;
    sigpending(&pending);
    check("SIGUSR1 pending", sigismember(&pending, SIGUSR1));
    sigset_t blocked;
    sigprocmask(SIG_BLOCK, NULL, &blocked);
    check("SIGKILL not blocked", !sigismember(&blocked, SIGKILL));
    sigprocmask(SIG_UNBLOCK, &set, NULL);
    report("handled after unblocking (one, not queued twice)", handled);

    // Ignored, and an ignored pending signal is dropped.
    signal(SIGUSR1, SIG_IGN);
    raise(SIGUSR1);
    report("handled while ignored", handled);
    install(SIGUSR1, on_signal, 0);
    sigprocmask(SIG_BLOCK, &set, NULL);
    raise(SIGUSR1);
    signal(SIGUSR1, SIG_IGN);
    sigpending(&pending);
    check("ignoring drops the pending signal", !sigismember(&pending, SIGUSR1));
    sigprocmask(SIG_UNBLOCK, &set, NULL);

    // The signal itself is blocked in its handler, unless SA_NODEFER.
    handled = 0;
    install(SIGUSR1, on_signal_raising, 0);
    raise(SIGUSR1);
    check("without SA_NODEFER: not nested", !nested);
    report("without SA_NODEFER: handled", handled);
    handled = 0;
    install(SIGUSR1, on_signal_raising, SA_NODEFER);
    raise(SIGUSR1);
    check("with SA_NODEFER: nested", nested);

    // SA_RESETHAND: one handler call, then the default again.
    install(SIGUSR1, on_signal, SA_RESETHAND);
    raise(SIGUSR1);
    sigaction(SIGUSR1, NULL, &action);
    check("SA_RESETHAND: back to SIG_DFL", action.sa_handler == SIG_DFL);

    fpu_survives_handler();

    // A SIGSEGV handler sees the address, and can leave with siglongjmp.
    install(SIGSEGV, on_segv, 0);
    if (sigsetjmp(recover, 1) == 0) {
        int *volatile bad = (int *)8;
        *bad = 1;
        printf("not reached\n");
    }
    check("SIGSEGV handled", last_signal == SIGSEGV);
    check("si_code is SEGV_MAPERR", last_code == SEGV_MAPERR);
    report("fault address", (long)fault_address);
    sigprocmask(SIG_BLOCK, NULL, &blocked);
    check("siglongjmp restored the mask", !sigismember(&blocked, SIGSEGV));

    // A fault while its signal is blocked ends the process with that signal.
    pid_t child = fork();
    if (child == 0) {
        sigset_t segv;
        sigemptyset(&segv);
        sigaddset(&segv, SIGSEGV);
        sigprocmask(SIG_BLOCK, &segv, NULL);
        int *volatile bad = (int *)8;
        *bad = 1;
        _exit(0);
    }
    int status = 0;
    waitpid(child, &status, 0);
    check("blocked SIGSEGV still kills", WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV);

    // execve keeps the mask and ignored signals, but not handlers.
    child = fork();
    if (child == 0) {
        install(SIGUSR1, on_signal, 0);
        signal(SIGUSR2, SIG_IGN);
        sigset_t term;
        sigemptyset(&term);
        sigaddset(&term, SIGTERM);
        sigprocmask(SIG_BLOCK, &term, NULL);
        char *arguments[] = {"signal", "again", NULL};
        execv((const char *)getauxval(AT_EXECFN), arguments);
        _exit(99);
    }
    waitpid(child, &status, 0);
    report("exit status after execve", WEXITSTATUS(status));
    return 0;
}
