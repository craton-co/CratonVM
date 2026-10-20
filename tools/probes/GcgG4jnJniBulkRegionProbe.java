// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcg g4/jn (2026-10-02): large {@code Get/Set<Type>ArrayRegion} round trips,
 * the timing row {@code CRATONVM_JNI_BULK_ARRAY_COPY} never had
 * ({@code docs/internal/gc/common-w9g-proposal-bulk-jni-array-copies-DONE-20261002.md}).
 * {@code Gcd1JniCostProbe}'s {@code int-region} copies ONE element per call,
 * which measures the transition and not the copy.
 *
 * <p>stdout is deterministic (one checksum line per element type, then
 * {@code PASS}); HotSpot 25 {@code -XX:+UseSerialGC} prints the same lines.
 * Timing goes to stderr: {@code [bulk-region] case=<type> len=<n> rounds=<r>
 * ns_per_round=<median of 5 batches>}.
 *
 * <p>Commands (Linux):
 * <pre>
 *   gcc -O1 -shared -fPIC -I"$JDK/include" -I"$JDK/include/linux" \
 *       -o /tmp/libgcgg4jnbulk.so tools/probes/jni/GcgG4jnJniBulkRegionProbe.c
 *   javac -d /tmp/gcgg4jnbulk tools/probes/GcgG4jnJniBulkRegionProbe.java
 *   java -XX:+UseSerialGC -Xmx256m -cp /tmp/gcgg4jnbulk GcgG4jnJniBulkRegionProbe \
 *       /tmp/libgcgg4jnbulk.so [len] [rounds]
 * </pre>
 * Defaults: {@code len = 65536} elements, {@code rounds = 200} per batch.
 */
public final class GcgG4jnJniBulkRegionProbe {

    static native int roundTripInt(int[] src, int[] dst, int rounds);

    static native int roundTripByte(byte[] src, byte[] dst, int rounds);

    static native int roundTripLong(long[] src, long[] dst, int rounds);

    static native int roundTripDouble(double[] src, double[] dst, int rounds);

    interface Batch {
        int run(int rounds);
    }

    static int failures;

    /** Five timed batches after one warm-up batch; the median per round. */
    static void time(String name, int len, int rounds, Batch b) {
        int rc = b.run(rounds);
        long[] ns = new long[5];
        for (int i = 0; i < ns.length && rc == 0; i++) {
            long t0 = System.nanoTime();
            rc = b.run(rounds);
            ns[i] = (System.nanoTime() - t0) / rounds;
        }
        if (rc != 0) {
            failures++;
            System.out.println(name + ": FAIL rc=" + rc);
            return;
        }
        java.util.Arrays.sort(ns);
        System.err.println("[bulk-region] case=" + name + " len=" + len + " rounds=" + rounds
                + " ns_per_round=" + ns[ns.length / 2]);
    }

    public static void main(String[] args) {
        if (args.length < 1 || args.length > 3) {
            System.out.println("usage: GcgG4jnJniBulkRegionProbe /abs/path/libgcgg4jnbulk.so [len] [rounds]");
            System.exit(2);
        }
        System.load(args[0]);
        final int len = args.length > 1 ? Integer.parseInt(args[1]) : 65536;
        final int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 200;

        final int[] is = new int[len];
        final int[] id = new int[len];
        final byte[] bs = new byte[len];
        final byte[] bd = new byte[len];
        final long[] ls = new long[len];
        final long[] ld = new long[len];
        final double[] ds = new double[len];
        final double[] dd = new double[len];
        for (int i = 0; i < len; i++) {
            is[i] = i * 7 + 1;
            bs[i] = (byte) (i * 13);
            ls[i] = i * 3L;
            ds[i] = i * 0.5;
        }

        time("int", len, rounds, r -> roundTripInt(is, id, r));
        time("byte", len, rounds, r -> roundTripByte(bs, bd, r));
        time("long", len, rounds, r -> roundTripLong(ls, ld, r));
        time("double", len, rounds, r -> roundTripDouble(ds, dd, r));

        long h = 17;
        for (int v : id) {
            h = h * 31 + v;
        }
        System.out.println("int len=" + len + " checksum=" + h);
        h = 17;
        for (byte v : bd) {
            h = h * 31 + v;
        }
        System.out.println("byte len=" + len + " checksum=" + h);
        h = 17;
        for (long v : ld) {
            h = h * 31 + v;
        }
        System.out.println("long len=" + len + " checksum=" + h);
        h = 17;
        for (double v : dd) {
            h = h * 31 + Double.doubleToLongBits(v);
        }
        System.out.println("double len=" + len + " checksum=" + h);
        System.out.println(failures == 0 ? "PASS" : "FAIL " + failures);
        System.exit(failures == 0 ? 0 : 1);
    }
}
