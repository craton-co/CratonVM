// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s1/ca (2026-10-14): where a compiled small allocation's time goes, by
// part, on one loop shape.
//
// `Zr7CaAllocShapes` prices `loop` (no allocation) against `fixed0` etc., but
// its `loop` row keeps the 64-bit `%` (an idiv) that `fixed0` does not have,
// so `fixed0 - loop` is not the allocation's cost. This probe fixes that:
// every shape runs the SAME loop (64 iterations per call into a 1024-slot
// `Object[]` ring, index `(n + k) & 1023`, no division) and differs only in
// what it stores:
//
//   store0   a preallocated `long[0]` (no allocation): the loop, the aastore
//            with its covariance check and store barrier, the safepoint poll.
//            The floor every other row is read against.
//   fixed0   `new long[0]`: a bare 16-byte header -- the inline bump, the
//            header stores, the ZGC start-bit announce and the commit.
//   fixed8   `new long[8]`: + 64 zeroed bytes (zeroing and footprint).
//   byte16   `new byte[16]`: the `new StringBuilder()` backing array.
//   obj      `new Object()`: the `new` arm of the same sequence.
//   varlen   `new long[(n + k) & 63]`: a RUN-TIME length (mean 31.5 longs,
//            272 bytes) without the idiv: Zr3JaRefArrayAlloc's `prim` shape
//            minus the division.
//
// Reading (ns/op, the mutator thread's CPU from ThreadMXBean):
//   X - store0             the allocation's own cost on this shape;
//   fixed0 - store0        the per-allocation term `a` (the inline sequence);
//   fixed8 - fixed0        zeroing + footprint for 64 bytes;
//   varlen - fixed8        the run-time-length arithmetic + 192 more bytes.
// `gc_ms` (collection time from the GC MXBeans) is on the same stderr line, so
// the mutator-only cost (`cpu - gc`) separates the inline sequence from the
// collector's per-byte work when the collector runs on this thread (CVM's
// stop-the-world cycles). On HotSpot ZGC the cycles are concurrent and
// `gc_ms` is their duration, so read `ns_per_op_cpu` alone there.
//
// A deterministic CHECK line first (stdout, must equal HotSpot's): every
// stored array has the right class, length and an all-zero body, checked
// inside the compiled kernel. Timings go to stderr only.
//
// Usage: java -Xmx512m Zs1CaAllocParts [shape=fixed0] [seconds=5] [rounds=200]
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;

public final class Zs1CaAllocParts {
    static final long[] PRE = new long[0];

    static void burstStore0(Object[] ring, long n) {
        long[] p = PRE;
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = p;
        }
    }

    static void burstFixed0(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[0];
        }
    }

    static void burstFixed8(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[8];
        }
    }

    static void burstByte16(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new byte[16];
        }
    }

    static void burstObj(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new Object();
        }
    }

    static void burstVarlen(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[(int) ((n + k) & 63)];
        }
    }

    static void burst(int code, Object[] ring, long n) {
        switch (code) {
            case 0 -> burstStore0(ring, n);
            case 1 -> burstFixed0(ring, n);
            case 2 -> burstFixed8(ring, n);
            case 3 -> burstByte16(ring, n);
            case 4 -> burstObj(ring, n);
            default -> burstVarlen(ring, n);
        }
    }

    /// One CHECK round; returns the number of failed checks.
    static long checkRound(int code, Object[] ring, long n) {
        burst(code, ring, n);
        long bad = 0;
        for (int k = 0; k < 64; k++) {
            Object o = ring[(int) ((n + k) & 1023)];
            switch (code) {
                case 3 -> {
                    if (!(o instanceof byte[] b) || b.length != 16) {
                        bad++;
                    } else {
                        for (byte v : b) {
                            if (v != 0) {
                                bad++;
                            }
                        }
                    }
                }
                case 4 -> {
                    if (o == null || o.getClass() != Object.class) {
                        bad++;
                    }
                }
                default -> {
                    if (!(o instanceof long[] a)) {
                        bad++;
                        continue;
                    }
                    int want = switch (code) {
                        case 0, 1 -> 0;
                        case 2 -> 8;
                        default -> (int) ((n + k) & 63);
                    };
                    if (a.length != want) {
                        bad++;
                    }
                    for (long v : a) {
                        if (v != 0) {
                            bad++;
                        }
                    }
                    if (code == 0 && a != PRE) {
                        bad++;
                    }
                }
            }
        }
        return bad;
    }

    static long gcCount() {
        long c = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            c += Math.max(0, b.getCollectionCount());
        }
        return c;
    }

    static long gcMillis() {
        long t = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            t += Math.max(0, b.getCollectionTime());
        }
        return t;
    }

    public static void main(String[] args) throws Exception {
        String shape = args.length > 0 ? args[0] : "fixed0";
        int seconds = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 200;
        final int code = switch (shape) {
            case "store0" -> 0;
            case "fixed0" -> 1;
            case "fixed8" -> 2;
            case "byte16" -> 3;
            case "obj" -> 4;
            case "varlen" -> 5;
            default -> throw new IllegalArgumentException(
                    "shape must be store0, fixed0, fixed8, byte16, obj or varlen: " + shape);
        };

        // Phase 1: CHECK (the kernels are interpreted at first, compiled as
        // the rounds go on; every round checks).
        Object[] ring = new Object[1024];
        long bad = 0;
        long n = 0;
        for (int r = 0; r < rounds; r++) {
            bad += checkRound(code, ring, n);
            n += 64;
        }
        System.out.println("Zs1CaAllocParts check shape=" + shape + " rounds=" + rounds
                + " ops=" + n + " bad=" + bad + " ok=" + (bad == 0));

        // Phase 2: warm the timed kernel, then TIME it on this thread.
        for (int r = 0; r < 20000; r++) {
            burst(code, ring, n);
            n += 64;
        }
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        long gc0 = gcCount();
        long gcMs0 = gcMillis();
        long cpu0 = mx.getCurrentThreadCpuTime();
        long wall0 = System.nanoTime();
        long deadline = wall0 + seconds * 1_000_000_000L;
        long ops = 0;
        while (System.nanoTime() < deadline) {
            for (int r = 0; r < 1000; r++) {
                burst(code, ring, n);
                n += 64;
            }
            ops += 64_000;
        }
        long cpu = mx.getCurrentThreadCpuTime() - cpu0;
        long wall = System.nanoTime() - wall0;
        long gcs = gcCount() - gc0;
        long gcMs = gcMillis() - gcMs0;
        double nsCpu = (double) cpu / ops;
        double nsGc = gcMs * 1e6 / ops;
        System.err.printf(java.util.Locale.ROOT, "timing: shape=%s ops=%d ns_per_op_cpu=%.3f ns_per_op_wall=%.3f "
                        + "gc_count=%d gc_ms=%d gc_ns_per_op=%.3f mutator_ns_per_op=%.3f%n",
                shape, ops, nsCpu, (double) wall / ops, gcs, gcMs, nsGc, nsCpu - nsGc);
        // Keep the ring live to the end.
        if (ring[(int) (n & 1023)] == null && ring[0] == null) {
            System.err.println("ring empty");
        }
    }
}
