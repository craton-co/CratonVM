// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 5 (lane TRAPPROF): a pruned branch's first trap must end the
// pruning at that bci (HotSpot's per-bci trap bit), however fast the eager
// C2 re-queue's worker reads the profile.
// Page: docs/feature-designs/jit-ir-optimizations.md
// Switch: CRATONVM_JIT_PROF_BRANCH_TRAP_BIT=1 (default OFF).
//
// Two kernels, each padded over the 325-byte splice limit so it is its own
// method-entry body (a flat prune, not a spliced one):
//   at  `if (i == t) r = i - 7; else r = i + 7;` -> `if_icmpne`, TAKEN on every
//       warm call: the always-taken prune (UncommonTrap, limit 4);
//   nt  `if (i != t) r = i + 7; else r = i - 7;` -> `if_icmpeq`, never taken
//       while warming: the never-taken prune (UnstableIf, limit 1).
// Rows per kernel: `warm` (40 000 calls, the rare edge never runs), then
// `periodic` (40 000 calls, the rare edge every 1 000th call: 40 times).
//
//   java -cp <dir> J27TrapBranchRecover > hs.txt
//   cratonvm --cp <dir> J27TrapBranchRecover | diff - hs.txt
//     x {default, CRATONVM_JIT_PROF_BRANCH_TRAP_BIT=1} x {--real-jdk, --compatible}
//   engagement / the race (8 interleaved runs per arm):
//     CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1 CRATONVM_DBG_IR_COMPILES=1 ... 2>&1 |
//       grep -E 'J27TrapBranchRecover\.(at|nt)' |
//       grep -E 'reason=(UncommonTrap|UnstableIf)|eager re-queue at C2|WITHDRAWN|retry (ENQUEUED|REFUSED)' |
//       sort | uniq -c
//     =1: one deopt per kernel at its branch bci, no `REFUSED ...: c2_bailout`.
//     default: a run with 2-4 deopts at one bci is the race; 4 ends in c2_bailout.
//   timing (optional): -Dj27.time=true prints `time <row> <ns>` lines.
//
// Expected HotSpot 25 output (facts):
//   fact at-warm 800260000
//   fact at-periodic 800259440 rare=40
//   fact nt-warm 800260000
//   fact nt-periodic 800259440 rare=40
public class J27TrapBranchRecover {
    static long pad;
    static int rare;

    static int at(int i, int t) {
        int r;
        if (i == t) {
            r = i - 7;
            rare++;
        } else {
            r = i + 7;
        }
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        return r;
    }

    static int nt(int i, int t) {
        int r;
        if (i != t) {
            r = i + 7;
        } else {
            r = i - 7;
            rare++;
        }
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i; pad = pad * 31L + i;
        return r;
    }

    static final int N = 40_000;
    static final boolean TIME = Boolean.getBoolean("j27.time");

    static long warm(boolean always) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += always ? at(i, -1) : nt(i, -1);
        }
        return s;
    }

    static long periodic(boolean always) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            int t = (i % 1000 == 999) ? i : -1;
            s += always ? at(i, t) : nt(i, t);
        }
        return s;
    }

    static void kernel(String name, boolean always) {
        long t0 = System.nanoTime();
        long w = warm(always);
        long t1 = System.nanoTime();
        System.out.println("fact " + name + "-warm " + w);
        rare = 0;
        long p = periodic(always);
        long t2 = System.nanoTime();
        System.out.println("fact " + name + "-periodic " + p + " rare=" + rare);
        if (TIME) {
            System.out.println("time " + name + "-warm " + (t1 - t0));
            System.out.println("time " + name + "-periodic " + (t2 - t1));
        }
    }

    public static void main(String[] args) {
        kernel("at", true);
        kernel("nt", false);
    }
}
