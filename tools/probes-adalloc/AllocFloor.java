// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// The per-allocation floor, with its own control rung.
//
// docs/known-issues/perf/perf-netty-adaptive-allocator-throughput-20260817.md
// prices the netty adaptive-allocator gap at "new Object() measures 674 ns
// interpreted against HotSpot's 10 ns" and says that ratio, not code quality
// inside one body, is the number to explain. This probe is what says whether a
// change moved it.
//
// Every rung stores its result into a live array slot, so nothing here is
// dead-code-eliminated or scalar-replaced away, and rung 0 is a control with
// the same loop shape and the same array store and NO allocation — without it
// an OSR artifact in the loop itself reads as allocation cost.
//
// args: [iters] [reps]
public final class AllocFloor {

    static final class Small {
        int a, b, c;
        Small(int a) { this.a = a; }
    }

    static final Object[] SINK = new Object[64];
    static int sinkIdx;

    static long control(int iters) {
        long t0 = System.nanoTime();
        Object o = SINK;
        for (int i = 0; i < iters; i++) {
            SINK[i & 63] = o;
        }
        return System.nanoTime() - t0;
    }

    static long newObject(int iters) {
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            SINK[i & 63] = new Object();
        }
        return System.nanoTime() - t0;
    }

    static long newSmall(int iters) {
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            SINK[i & 63] = new Small(i);
        }
        return System.nanoTime() - t0;
    }

    static long newByteArray(int iters) {
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            SINK[i & 63] = new byte[24];
        }
        return System.nanoTime() - t0;
    }

    static long newStringBuilder(int iters) {
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            SINK[i & 63] = new StringBuilder(16);
        }
        return System.nanoTime() - t0;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;

        // warm every rung before any rung is timed, so no rung pays another's
        // class loading or first-touch TLAB refill.
        for (int w = 0; w < 2; w++) {
            control(iters / 10 + 1);
            newObject(iters / 10 + 1);
            newSmall(iters / 10 + 1);
            newByteArray(iters / 10 + 1);
            newStringBuilder(iters / 10 + 1);
        }

        for (int r = 0; r < reps; r++) {
            long c = control(iters);
            long o = newObject(iters);
            long s = newSmall(iters);
            long b = newByteArray(iters);
            long sb = newStringBuilder(iters);
            System.out.printf(
                "rep=%d control=%.1f new_Object=%.1f new_Small=%.1f new_byte24=%.1f new_StringBuilder=%.1f (ns/op)%n",
                r,
                c / (double) iters,
                o / (double) iters,
                s / (double) iters,
                b / (double) iters,
                sb / (double) iters);
        }
        if (sinkIdx == -1) {
            System.out.println(SINK[0]);
        }
    }
}
