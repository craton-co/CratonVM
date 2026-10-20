// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcj j1/yg ({@code gci-i47w-j43-tlab-totals-line-absent-in-single-thread-churn-RETIRED-UNFIXED-20261018}):
 * the main thread allocates {@code args[0]} MiB (default 1024) of short-lived
 * {@code byte[256]} in {@code args[1]} rounds (default 4), with a
 * {@code System.gc()} after each round. Run with
 * {@code -Xlog:gc+tlab=debug:file=<log>} and NO other {@code -Xlog} rule: HotSpot
 * prints {@code GC(n) TLAB totals: thrds: 1 ...} at every pause of this
 * program; the check is on the log file, not on stdout.
 *
 * <p>stdout (deterministic, HotSpot's): {@code gcj1yg tlab rounds=N done}.
 * stderr: the checksum (keeps the allocations live to the JIT).
 */
public class Gcj1YgTlabTotalsProbe {
    static volatile byte[] sink;

    public static void main(String[] args) {
        long mib = args.length > 0 ? Long.parseLong(args[0]) : 1024L;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        long perRound = mib * 1024L * 1024L / rounds / 256L;
        long sum = 0;
        for (int r = 0; r < rounds; r++) {
            for (long i = 0; i < perRound; i++) {
                byte[] b = new byte[256];
                b[(int) (i & 255)] = (byte) i;
                sink = b;
                sum += b[0];
            }
            System.gc();
        }
        System.err.println("[gcj1yg-tlab] checksum=" + sum);
        System.out.println("gcj1yg tlab rounds=" + rounds + " done");
    }
}
