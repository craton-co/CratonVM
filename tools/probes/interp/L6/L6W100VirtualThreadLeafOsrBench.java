// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L6 -- the isolating bench row of
// CRATONVM_VTHREAD_COMPILED_LEAF_OSR (page
// i91-L6-virtual-thread-code-is-never-compiled): eight threads each call a
// static leaf ONCE with a 40M-iteration loop (no invoke, no monitor op). Run
// on virtual threads, then on platform threads. On HotSpot the two rates
// match; here, with the switch off, a virtual thread's first activation of
// the leaf is interpreted to its end (OSR is refused on a virtual thread).
// The loop work credited at its pop makes the NEXT call compiled through the
// static leaf door (CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY, default on), so the
// isolating arms turn that door off: then every virtual activation is either
// OSR'd (switch on) or interpreted (switch off).
//
// Timings on lines starting "TIME " (best of `reps`, arg 0, default 5, after
// one warm-up rep each; the "first" line is the first virtual rep alone,
// before any tier-up); stdout otherwise only the checksum line.
//
// A/B (one binary, interleaved, medians; both modes):
//   CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY=0 cratonvm L6W100VirtualThreadLeafOsrBench 5
//   CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY=0 CRATONVM_VTHREAD_COMPILED_LEAF_OSR=1 cratonvm L6W100VirtualThreadLeafOsrBench 5
//   cratonvm L6W100VirtualThreadLeafOsrBench 5   (and with the switch: the "first" line)
//   (java L6W100VirtualThreadLeafOsrBench 5 for HotSpot)
// Expected: off, TIME virtual >> TIME platform; on, they converge.
//
// HOTSPOT_EXPECTED_BEGIN
// checksum match=true
// HOTSPOT_EXPECTED_END
public class L6W100VirtualThreadLeafOsrBench {
    static final int THREADS = 8;
    static final int N = 40_000_000;

    static long spin(int n, int seed) {
        long s = seed;
        for (int i = 0; i < n; i++) {
            s = s * 31 + (i ^ (s >>> 7));
        }
        return s;
    }

    static long row(boolean virtual) throws InterruptedException {
        long[] got = new long[THREADS];
        Thread[] ts = new Thread[THREADS];
        for (int k = 0; k < THREADS; k++) {
            final int kk = k;
            Runnable r = () -> got[kk] = spin(N, kk);
            ts[k] = virtual ? Thread.ofVirtual().start(r) : Thread.ofPlatform().start(r);
        }
        long sum = 0;
        for (int k = 0; k < THREADS; k++) {
            ts[k].join();
            sum += got[k];
        }
        return sum;
    }

    static long firstMs = -1;

    static long best(boolean virtual, int reps, long[] sumOut) throws InterruptedException {
        long f0 = System.nanoTime();
        sumOut[0] = row(virtual);
        if (virtual) {
            firstMs = (System.nanoTime() - f0) / 1_000_000;
        }
        long best = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            long s = row(virtual);
            long ms = (System.nanoTime() - t0) / 1_000_000;
            best = Math.min(best, ms);
            if (s != sumOut[0]) {
                sumOut[0] = Long.MIN_VALUE;
            }
        }
        return best;
    }

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        long[] v = new long[1];
        long[] p = new long[1];
        long vms = best(true, reps, v);
        long pms = best(false, reps, p);
        System.out.println("TIME virtual-first ms=" + firstMs);
        System.out.println("TIME virtual ms=" + vms);
        System.out.println("TIME platform ms=" + pms);
        System.out.println("checksum match=" + (v[0] == p[0] && v[0] != Long.MIN_VALUE));
    }
}
