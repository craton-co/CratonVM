// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z5/cr (2026-10-13): refusals on a heap that was just collected for a
// refusal. Fills the heap with int[64] held in ONE preallocated Object[] (a
// static field) until the first OutOfMemoryError, then, with everything still
// live, makes `attempts` requests of `bigMb` MiB each (each must fail: the heap
// is full of live data), then drops the data and allocates once more (must
// succeed). Each phase is its own method and the data lives only in a static
// field, so no stale frame slot of the filling loop can hold it after the drop.
//
// stdout (compare with java -XX:+UseZGC on the same -Xmx):
//   zr5cr refusal-after-oome failures=<attempts> recovered=true held=true
// stderr: the collection count of the run (GarbageCollectorMXBean, read after
// the data is dropped), the number the futile-refusal rule moves.
//
// On CratonVM ZGC each post-OOME request used to run a forced collection AND
// the clear-soft rung on a heap its predecessor's ladder had just collected;
// `CRATONVM_ZGC_REFUSAL_SKIP_FUTILE` (default ON) skips the forced one. The
// `[GC] zgc-refusal:` line (CRATONVM_DBG=gc-stats) prints `skipped_futile=`.
//
// Usage: Zr5CrRefusalAfterOome [attempts=5] [bigMb=64]   (run at -Xmx256m)
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

public class Zr5CrRefusalAfterOome {
    static volatile Object sink;
    static Object[] keep;
    static int n;

    public static void main(String[] args) {
        int attempts = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        int bigMb = args.length > 1 ? Integer.parseInt(args[1]) : 64;
        fill();
        int failures = attempt(attempts, bigMb);
        boolean held = held();
        keep = null;
        boolean recovered = recover();
        System.out.println("zr5cr refusal-after-oome failures=" + failures + " recovered=" + recovered
                + " held=" + held);
        long collections = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (b.getName().contains("Cycles") || !b.getName().contains("Pauses")) {
                collections += Math.max(0, b.getCollectionCount());
            }
        }
        System.err.println("[probe] zr5cr retained=" + n + " collections=" + collections);
    }

    static void fill() {
        keep = new Object[(int) Math.min(8_000_000L, Runtime.getRuntime().maxMemory() / 256)];
        try {
            while (n < keep.length) {
                keep[n] = new int[64];
                n++;
            }
        } catch (OutOfMemoryError e) {
            // full of live data
        }
    }

    static int attempt(int attempts, int bigMb) {
        int failures = 0;
        for (int i = 0; i < attempts; i++) {
            try {
                sink = new byte[bigMb << 20];
                sink = null;
            } catch (OutOfMemoryError e) {
                failures++;
            }
        }
        return failures;
    }

    static boolean held() {
        return n > 0 && keep[n - 1] != null && keep[0] != null;
    }

    static boolean recover() {
        try {
            sink = new byte[1 << 20];
            sink = null;
            return true;
        } catch (OutOfMemoryError e) {
            return false;
        }
    }
}
