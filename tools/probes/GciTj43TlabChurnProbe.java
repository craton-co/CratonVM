// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gci i43/tj ({@code gci-i42ta-tlab-totals-refill-counter-wraps-on-a-hot-thread-RETIRED-UNFIXED-20261018}):
 * one thread allocates {@code args[0]} MiB (default 2048) of short-lived
 * {@code byte[256]} in {@code args[1]} rounds (default 4), with a
 * {@code System.gc()} after each round, so {@code -Xlog:gc+tlab=debug} prints
 * a {@code TLAB totals} line per pause.
 *
 * <p>stdout (deterministic): {@code tj43 churn rounds=N done}. stderr: the
 * checksum (keeps the allocations live to the JIT).
 */
public class GciTj43TlabChurnProbe {
    static volatile byte[] sink;

    public static void main(String[] args) {
        long mib = args.length > 0 ? Long.parseLong(args[0]) : 2048L;
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
        System.err.println("[tj43] checksum=" + sum);
        System.out.println("tj43 churn rounds=" + rounds + " done");
    }
}
