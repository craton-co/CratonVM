// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 9 (lane W9RPL): the service-copy window at a direct call in an OSR body
// (CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW, opt-in; pages
// docs/known-issues/jit/j27-osr-direct-call-service-copy-window-proposal-FIXED-20261029.md and
// docs/feature-designs/native-dispatch-architecture.md).
//
// `osrLoop` is entered ONCE, so its loop runs in an OSR body. Its last iteration passes an array
// of 55% of the heap to `drop`, which stops using it before it allocates a second one. HotSpot 25
// roots neither. Without splicing (CRATONVM_JIT_IR_INLINE=0) the call stays a direct call in the
// OSR body, the owned hand-off refuses it (no bci-0 state), and the call keeps service copies of
// its arguments; the window decides whether the collector may clear them.
//
// Expected HotSpot 25 output (-Xmx96m -XX:-UseCompressedOops):
//   fact osr-oome false
//   fact osr-sum 1799970
//   fact after-oome false
//   fact after-sum 1799980
//   fact pad 0
// `osr-sum`: the 199999 small iterations, each (i % 13 + 1) + 2 (the big one is left out, BIG
// depends on -Xmx). `osrAfter` runs all 200000 small calls in its loop and the big call after it.
// Windows result (lane W9RPL, 2026-10-29, CRATONVM_JIT_IR_INLINE=0): both rows OOME with and
// without the switch. The switch reaches the OSR site (`window n.. bci=20: none, the call is on a
// cycle` appears only with it on), but argument_only_homes refuses every call on a loop cycle, and
// the after-loop site (bci 41) is refused by the window's site terms in both kinds of compile
// (docs/known-issues/jit/j27-service-copy-window-names-no-loop-or-after-loop-osr-call-20261029.md).
//
// Run: javac -d <out> J27OsrServiceCopyRows.java
//   java -Xmx96m -XX:-UseCompressedOops -cp <out> J27OsrServiceCopyRows
//   cratonvm -Xmx96m -cp <out> J27OsrServiceCopyRows
//     x {CRATONVM_JIT_IR_INLINE=0, default} x {unset, CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW=1}
//     x {--real-jdk, --compatible}
// Engagement (switch on, IR_INLINE=0): CRATONVM_DBG_IR_SLOTS=1 CRATONVM_DBG_JIT_ROOTSCAN=1
//   prints a non-empty `[ir-owned-args] service-copies ret=.. recorded [..]` for the OSR body and
//   `[jitrootscan] service-copies cleared=N caller=J27OsrServiceCopyRows.osrLoop..` at the big GC.
public class J27OsrServiceCopyRows {
    static final int BIG = (int) (Runtime.getRuntime().maxMemory() * 0.55);
    static final int ITERS = 200_000;
    static long pad;

    static int drop(byte[] p, int size) {
        int n = p.length;
        byte[] q = new byte[size];
        q[0] = 1;
        return n + q.length;
    }

    static long osrLoop() {
        long s = 0;
        for (int i = 0; i < ITERS; i++) {
            boolean last = i == ITERS - 1;
            int size = last ? BIG : i % 13 + 1;
            int r = drop(new byte[size], last ? BIG : 2);
            s += last ? 0 : r;
        }
        return s;
    }

    // The big call AFTER the loop: not on a cycle, so the window's rule can name its homes.
    static long osrAfter() {
        long s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += drop(new byte[i % 13 + 1], 2);
        }
        return s + drop(new byte[BIG], BIG) - 2L * BIG;
    }

    public static void main(String[] args) {
        long s = -1;
        boolean oome = false;
        try {
            s = osrLoop();
        } catch (OutOfMemoryError e) {
            oome = true;
        }
        System.out.println("fact osr-oome " + oome);
        System.out.println("fact osr-sum " + s);
        s = -1;
        oome = false;
        try {
            s = osrAfter();
        } catch (OutOfMemoryError e) {
            oome = true;
        }
        System.out.println("fact after-oome " + oome);
        System.out.println("fact after-sum " + s);
        System.out.println("fact pad " + pad);
    }
}
