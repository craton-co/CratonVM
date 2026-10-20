// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z2/rv (2026-10-12): a derived pointer more than 256 MiB into a live
// array must keep the array alive.
//
// Since zr z1 a real array past 256 MiB of payload is sized
// (`CRATONVM_ZGC_LARGE_ARRAY_SIZING`) and therefore FREED when the mark misses
// it. The take-over pass roots a frozen compiled frame's derived pointers
// through `resolve_interior_for_pin`, whose backwards scan stops 256 MiB below
// the address; a hot loop whose only live handle on the array is its cursor
// past the 256 MiB mark had its array unmarked and swept under it. The fix is
// the huge-extent table (`ZgcRealHeap::huge_extent_containing`); there is no
// switch (a plain bug fix), so the control is the base binary a1a0d99ce4.
//
// Shape: the main thread runs a long compiled loop over the upper part of an
// `int[]` of `mib` MiB, re-reading what it wrote; a churn thread allocates
// garbage so collections (and take-overs of the looping thread) happen while
// the loop runs. A freed-and-reused array shows up as a checksum mismatch or a
// crash.
//
// Usage: java -Xmx1g Zr2RvHugeCursor [mib=300] [rounds=40]
// Pass: `rounds=<rounds> bad=0 PASS` (HotSpot twin: java -XX:+UseZGC -Xmx1g).
// Engagement: `[GC] zgc-alloc-sweep: ... huge_extent_hits=` (CRATONVM_GC_STATS=1)
// is > 0 on a run where a take-over froze the loop past the 256 MiB mark; 0
// means the row did not reach the hazard (re-run with more rounds).
public final class Zr2RvHugeCursor {
    static volatile boolean stop;

    public static void main(String[] args) throws Exception {
        int mib = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        Thread churn = new Thread(() -> {
            long sink = 0;
            while (!stop) {
                Object[] junk = new Object[4096];
                for (int i = 0; i < junk.length; i++) {
                    junk[i] = new byte[64];
                }
                sink += junk.length;
            }
            if (sink == 42) {
                System.out.println("unreachable");
            }
        });
        churn.setDaemon(true);
        churn.start();
        int bad = 0;
        for (int r = 0; r < rounds; r++) {
            bad += round(mib, r);
        }
        stop = true;
        churn.join();
        System.out.println("rounds=" + rounds + " bad=" + bad + (bad == 0 ? " PASS" : " FAIL"));
    }

    // The array is only reachable through `a` until the cursor loop starts;
    // a JIT that strength-reduces `a[i]` to a pointer induction variable keeps
    // only the cursor live across the loop's safepoint polls.
    static int round(int mib, int seed) {
        int len = (mib << 20) / 4;
        int from = (270 << 20) / 4;
        if (from >= len) {
            from = len / 2;
        }
        int[] a = new int[len];
        for (int i = from; i < len; i++) {
            a[i] = i ^ seed;
        }
        long sum = 0;
        for (int pass = 0; pass < 4; pass++) {
            for (int i = from; i < len; i++) {
                sum += a[i];
            }
        }
        long want = 0;
        for (int i = from; i < len; i++) {
            want += i ^ seed;
        }
        return sum == want * 4 ? 0 : 1;
    }
}
