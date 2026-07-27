/* LD_PRELOAD SIGSEGV/SIGBUS reporter.
 *
 * gdb suppresses this particular race entirely (14/14 runs exited cleanly under
 * `gdb -batch` while the bare binary crashes ~1-in-10), so we need something
 * light enough not to perturb thread timing. This installs an SA_SIGINFO
 * handler that dumps the essentials async-signal-safely-ish and then re-raises
 * with the default disposition so the exit status is still 139.
 *
 * Prints: signal, si_addr, si_code, thread name + tid, the full GP register
 * set, the /proc/self/maps line containing RIP (so we can tell JIT'd code —
 * an anonymous rwx mapping — from the Rust binary), and a backtrace.
 *
 * Build:
 *   gcc -shared -fPIC -O1 -g -o segvtrap.so segvtrap.c -ldl
 * Use:
 *   LD_PRELOAD=/path/segvtrap.so <cmd>            (report to stderr)
 *   SEGVTRAP_OUT=/path/prefix LD_PRELOAD=... <cmd> (report to prefix.<pid>)
 */
#define _GNU_SOURCE
#include <errno.h>
#include <execinfo.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <ucontext.h>
#include <unistd.h>

static int out_fd = 2;

static void wr(const char *s) { ssize_t n = write(out_fd, s, strlen(s)); (void)n; }

static void wrhex(unsigned long long v) {
    char buf[19];
    int i = 18;
    buf[i--] = '\0';
    if (v == 0) { buf[i--] = '0'; }
    while (v && i >= 2) { int d = v & 0xf; buf[i--] = d < 10 ? '0' + d : 'a' + d - 10; v >>= 4; }
    buf[i--] = 'x';
    buf[i] = '0';
    wr(&buf[i]);
}

static void wrdec(long v) {
    char buf[24];
    int i = 23;
    int neg = v < 0;
    unsigned long u = neg ? (unsigned long)(-v) : (unsigned long)v;
    buf[i--] = '\0';
    if (u == 0) buf[i--] = '0';
    while (u && i >= 1) { buf[i--] = '0' + (u % 10); u /= 10; }
    if (neg) buf[i--] = '-';
    wr(&buf[i + 1]);
}

static void reg(const char *name, unsigned long long v) {
    wr("  "); wr(name); wr("="); wrhex(v); wr("\n");
}

/* Print the /proc/self/maps line whose range contains `addr`. */
static void map_for(unsigned long long addr) {
    int fd = open("/proc/self/maps", O_RDONLY);
    if (fd < 0) return;
    static char buf[65536];
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0) return;
    buf[n] = '\0';
    char *line = buf;
    while (line && *line) {
        char *nl = strchr(line, '\n');
        if (nl) *nl = '\0';
        unsigned long long lo = 0, hi = 0;
        if (sscanf(line, "%llx-%llx", &lo, &hi) == 2 && addr >= lo && addr < hi) {
            wr("  map: "); wr(line); wr("\n");
            if (nl) *nl = '\n';
            return;
        }
        if (!nl) break;
        *nl = '\n';
        line = nl + 1;
    }
    wr("  map: <not found — freed/unmapped?>\n");
}

static void handler(int sig, siginfo_t *si, void *uctx) {
    ucontext_t *uc = (ucontext_t *)uctx;
    greg_t *g = uc->uc_mcontext.gregs;
    char tname[32] = {0};
    prctl(PR_GET_NAME, tname, 0, 0, 0);

    wr("\n@@SEGVTRAP signal=");
    wrdec(sig);
    wr(" si_code=");
    wrdec(si->si_code);
    wr(" si_addr=");
    wrhex((unsigned long long)(uintptr_t)si->si_addr);
    wr(" tid=");
    wrdec((long)syscall(SYS_gettid));
    wr(" thread='");
    wr(tname);
    wr("'\n");

    unsigned long long rip = (unsigned long long)g[REG_RIP];
    reg("rip", rip);
    map_for(rip);
    wr("  si_addr map:\n");
    map_for((unsigned long long)(uintptr_t)si->si_addr);

    reg("rsp", (unsigned long long)g[REG_RSP]);
    reg("rbp", (unsigned long long)g[REG_RBP]);
    reg("rax", (unsigned long long)g[REG_RAX]);
    reg("rbx", (unsigned long long)g[REG_RBX]);
    reg("rcx", (unsigned long long)g[REG_RCX]);
    reg("rdx", (unsigned long long)g[REG_RDX]);
    reg("rsi", (unsigned long long)g[REG_RSI]);
    reg("rdi", (unsigned long long)g[REG_RDI]);
    reg("r8 ", (unsigned long long)g[REG_R8]);
    reg("r9 ", (unsigned long long)g[REG_R9]);
    reg("r10", (unsigned long long)g[REG_R10]);
    reg("r11", (unsigned long long)g[REG_R11]);
    reg("r12", (unsigned long long)g[REG_R12]);
    reg("r13", (unsigned long long)g[REG_R13]);
    reg("r14", (unsigned long long)g[REG_R14]);
    reg("r15", (unsigned long long)g[REG_R15]);

    wr("  first 16 bytes at rip:");
    {
        unsigned char *p = (unsigned char *)(uintptr_t)rip;
        for (int i = 0; i < 16; i++) { wr(" "); wrhex(p[i]); }
        wr("\n");
    }

    wr("@@SEGVTRAP backtrace:\n");
    void *frames[64];
    int nf = backtrace(frames, 64);
    backtrace_symbols_fd(frames, nf, out_fd);
    wr("@@SEGVTRAP end\n");

    signal(sig, SIG_DFL);
    raise(sig);
}

__attribute__((constructor)) static void install(void) {
    const char *pfx = getenv("SEGVTRAP_OUT");
    if (pfx && *pfx) {
        char path[512];
        snprintf(path, sizeof(path), "%s.%d", pfx, (int)getpid());
        int fd = open(path, O_WRONLY | O_CREAT | O_APPEND, 0644);
        if (fd >= 0) out_fd = fd;
    }
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);
}
