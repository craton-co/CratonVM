// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// tools/text-order/texttrap.c -- LD_PRELOAD census of the executable's text
// pages a run executes (Linux, x86_64). Part of tools/text-order (see README.md).
//
// Before main, the executable's r-x mapping is made PROT_NONE. The first
// execution (or read) of each page faults; the SIGSEGV handler records the
// page's offset, gives the page back PROT_READ|PROT_EXEC and returns, so the
// faulting instruction re-runs. Every other SIGSEGV is chained to whatever
// handler the program installs: its sigaction() calls for SIGSEGV are
// interposed and stored instead of replacing this handler.
//
// Output: $TEXTTRAP_OUT.<pid>, 8-byte little-endian words: the mapping's base
// address, then one page offset per executed page, in first-execution order.
// The preload stays in the environment, so a re-executed launcher is traced
// too (one file per pid; the census keeps the largest).
//
// Build: gcc -O2 -shared -fPIC -o texttrap.so texttrap.c -ldl
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static uintptr_t lo, hi;
static int out = -1;
static int armed;
static struct sigaction chained;
static int (*real_sigaction)(int, const struct sigaction *, struct sigaction *);

static void on_segv(int sig, siginfo_t *si, void *uc) {
  uintptr_t a = (uintptr_t)si->si_addr;
  if (armed && a >= lo && a < hi) {
    uintptr_t pg = a & ~(uintptr_t)4095;
    if (mprotect((void *)pg, 4096, PROT_READ | PROT_EXEC) == 0) {
      uint64_t off = pg - lo;
      if (write(out, &off, 8) < 0) {
      }
      return;
    }
  }
  if (chained.sa_flags & SA_SIGINFO) {
    if (chained.sa_sigaction) {
      chained.sa_sigaction(sig, si, uc);
      return;
    }
  } else if (chained.sa_handler != SIG_DFL && chained.sa_handler != SIG_IGN) {
    chained.sa_handler(sig);
    return;
  }
  struct sigaction d;
  memset(&d, 0, sizeof d);
  d.sa_handler = SIG_DFL;
  real_sigaction(SIGSEGV, &d, 0);
}

int sigaction(int sig, const struct sigaction *act, struct sigaction *old) {
  if (!real_sigaction) real_sigaction = dlsym(RTLD_NEXT, "sigaction");
  if (armed && sig == SIGSEGV) {
    if (old) *old = chained;
    if (act) chained = *act;
    return 0;
  }
  return real_sigaction(sig, act, old);
}

__attribute__((constructor)) static void init(void) {
  if (!real_sigaction) real_sigaction = dlsym(RTLD_NEXT, "sigaction");
  const char *o = getenv("TEXTTRAP_OUT");
  if (!o) return;
  char fn[4200];
  snprintf(fn, sizeof fn, "%s.%d", o, (int)getpid());
  out = open(fn, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (out < 0) return;
  char exe[4096];
  ssize_t n = readlink("/proc/self/exe", exe, sizeof exe - 1);
  if (n < 0) return;
  exe[n] = 0;
  FILE *f = fopen("/proc/self/maps", "r");
  if (!f) return;
  char line[8192];
  while (fgets(line, sizeof line, f)) {
    unsigned long l, h;
    char perms[8], path[4096] = "";
    if (sscanf(line, "%lx-%lx %7s %*s %*s %*s %4095s", &l, &h, perms, path) >= 3 &&
        strcmp(path, exe) == 0 && perms[2] == 'x') {
      lo = l;
      hi = h;
    }
  }
  fclose(f);
  if (!lo) return;
  uint64_t base = lo;
  if (write(out, &base, 8) < 0) return;
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = on_segv;
  sa.sa_flags = SA_SIGINFO | SA_NODEFER;
  sigemptyset(&sa.sa_mask);
  real_sigaction(SIGSEGV, &sa, 0);
  armed = 1;
  mprotect((void *)lo, hi - lo, PROT_NONE);
}
