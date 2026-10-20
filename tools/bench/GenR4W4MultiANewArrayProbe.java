// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w4/oom (2026-09-24): a deterministic reproducer for
 * {@code docs/internal/gc/gengc-r4w3-hunter-multianewarray-never-collects-FIXED-20260924.md}.
 * {@code multianewarray} must collect before it throws {@code OutOfMemoryError}.
 *
 * <p>Why the wave-3 snippet did not reproduce: its garbage was young churn,
 * and ordinary young collections reclaimed it long before the matrix was
 * built. What makes {@code alloc_multi_array} fail is an OLD generation full
 * of garbage below the 75% major trigger, with rows too big for the room the
 * young generation has left. On CratonVM's Generational backend at
 * {@code -Xmx128m} the geometry is: 32 MiB semi-spaces, a 64 MiB old
 * generation, and a humongous threshold of 16 MiB (half a semi-space). An
 * array over that threshold goes straight to old gen, and falls back to young
 * only when old gen cannot hold it.
 *
 * <ol>
 *   <li>{@code garbage(40 MiB)}: humongous, so it lands in old gen (62.5% plus
 *       the boot data, under 75%). It is dropped at once. No collection runs:
 *       the young trigger does not see old-gen bytes, and nothing below 75%
 *       asks for a major.</li>
 *   <li>{@code new byte[4][17 MiB]}: row 0 fits the old generation's
 *       remaining ~24 MiB. Row 1 does not, and falls back to young. Row 2
 *       fits neither (old has under 7 MiB, young under 15 MiB). Without the
 *       fix, {@code alloc_multi_array} maps that to {@code OutOfMemoryError}
 *       with no collection. With {@code alloc_multi_array_collecting} the
 *       shared ladder collects: old gen is now over 75%, so the forced
 *       collection runs a major (and {@code last_ditch_reclaim} requests one
 *       anyway), frees the 40 MiB plus the failed attempt's rows, and the
 *       retry places three rows in old gen and the fourth in young.</li>
 * </ol>
 * The first step builds the matrix in {@code main} (interpreted). The second
 * repeats it through a method warmed until it is compiled, so on a JIT build it
 * goes through {@code jit_multianewarray_n}, which shares the same Rust
 * function.
 *
 * <p>HotSpot ({@code -XX:+UseSerialGC -Xmx128m}) has a larger old generation
 * and collects on allocation failure anyway, so it prints:
 * <pre>
 *   multianewarray ok
 *   multianewarray-warm ok
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx128m -cp tools/bench GenR4W4MultiANewArrayProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR4W4MultiANewArrayProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m --nojit -cp tools/bench GenR4W4MultiANewArrayProbe
 * </pre>
 * A binary without {@code alloc_multi_array_collecting} (the round-4 base,
 * before gen r4w3/hunter) prints {@code multianewarray FAILED: ...} on the first
 * line: that is the reproducer. It must not print {@code FAILED} on any binary
 * that has the fix. With {@code CRATONVM_DBG=gc-stats} the exit census names a
 * {@code multianewarray} forced site on the fixed binary.
 */
public final class GenR4W4MultiANewArrayProbe {
    static final int MIB = 1 << 20;
    static final int ROW = 17 * MIB;
    static final int ROWS = 4;
    static final int GARBAGE = 40 * MIB;

    static volatile Object sink;
    static byte[][] keep;
    static boolean ok = true;

    /** Its own frame, so no dead local can root the array afterwards. */
    static void garbage(int bytes) {
        byte[] g = new byte[bytes];
        g[bytes - 1] = 1;
        sink = g;
        sink = null;
    }

    static byte[][] make(int rows, int len) {
        return new byte[rows][len];
    }

    static void check(String label, byte[][] m) {
        boolean shape = m.length == ROWS;
        for (int i = 0; shape && i < ROWS; i++) {
            shape = m[i] != null && m[i].length == ROW && m[i][ROW - 1] == 0;
        }
        System.out.println(label + (shape ? " ok" : " FAILED: wrong shape"));
        ok &= shape;
    }

    public static void main(String[] args) {
        // Step 1: interpreted multianewarray.
        garbage(GARBAGE);
        try {
            keep = new byte[ROWS][ROW];
            check("multianewarray", keep);
        } catch (OutOfMemoryError e) {
            System.out.println("multianewarray FAILED: OutOfMemoryError \"" + e.getMessage()
                    + "\" with " + (GARBAGE / MIB) + " MiB of dropped old-gen garbage");
            ok = false;
        }
        keep = null;

        // Step 2: the same shape through a warmed (compiled, on a JIT build) method.
        long sum = 0;
        for (int i = 0; i < 50_000; i++) {
            sum += make(2, 16).length;
        }
        if (sum != 100_000L) {
            System.out.println("warm-up FAILED sum=" + sum);
            ok = false;
        }
        garbage(GARBAGE);
        try {
            keep = make(ROWS, ROW);
            check("multianewarray-warm", keep);
        } catch (OutOfMemoryError e) {
            System.out.println("multianewarray-warm FAILED: OutOfMemoryError \"" + e.getMessage() + "\"");
            ok = false;
        }
        keep = null;

        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
