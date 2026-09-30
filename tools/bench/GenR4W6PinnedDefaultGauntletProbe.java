// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w6/pinstale6 (2026-09-24): THE GATE for making the pinned in-place
 * young copy ({@code CRATONVM_GEN_PINNED_YOUNG_COPY}) the default.
 *
 * <p>Multi-threaded and JIT-warm. Every call of every worker descends a
 * {@value #DEPTH}-frame call chain; each frame allocates a small {@code Box}
 * and a medium {@code int[]} (8..256 elements) and holds BOTH in locals across
 * the recursive call, which allocates everything below it — so compiled
 * frames, their spill slots and callee-saved registers hold young oops across
 * allocation, at every depth, on every thread, which is the population the
 * pinned copy pins. The bottom frame churns small garbage while an element
 * index into a live {@code byte[]} is held across each allocation, builds a
 * String, and parks the bottom {@code Box} (and with it the whole 24-frame
 * chain) in a per-thread RING of {@value #RING} entries, so a medium-lived
 * population survives many young collections. Every {@value #LARGE_EVERY}th
 * call holds a 128 KiB {@code int[]} across the descent, every
 * {@value #HUGE_EVERY}th a 1 MiB one: small, medium and large objects mixed.
 *
 * <p>Every frame re-reads its own objects after the callee returned, and a
 * final pass re-derives every ring entry from its position; a lost, stale or
 * half-copied object is a {@code bad} count and {@code FAIL}. The checksum is
 * summed from the DATA read back (not from constants), per thread, so it does
 * not depend on scheduling or on the collector, and has a closed form:
 *
 * <pre>
 *   per call c of thread t:  24*t*K1 + 1536*c + 174516
 *                            + 32768 [c % 64 == 0] + 262144 [c % 1024 == 0]
 *   per ring entry:          173988    (min(calls, RING) entries per thread)
 *   K1 = 1000003
 * </pre>
 *
 * <p>Expected output (HotSpot 25, {@code -XX:+UseSerialGC -Xmx256m}, and any
 * correct collector):
 * <pre>
 *   PASS gauntlet threads=4 calls=100000 depth=24 bad=0 checksum=45190562680832
 *   (args 4 20000)   PASS gauntlet threads=4 calls=20000 depth=24 bad=0 checksum=4123483131904
 *   (args 1 100000)  PASS gauntlet threads=1 calls=100000 depth=24 bad=0 checksum=7697629870208
 * </pre>
 * Commands (the flip matrix is in
 * {@code docs/internal/reviews/gengc-round4-w6-pinstale6-20260924.md}):
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR4W6PinnedDefaultGauntletProbe
 *   CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W6PinnedDefaultGauntletProbe
 *   CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_DBG=gc-stress=250000 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W6PinnedDefaultGauntletProbe 4 20000
 * </pre>
 * Optional args: {@code threads calls}. Exit status 1 on FAIL.
 */
public final class GenR4W6PinnedDefaultGauntletProbe {
    static final int DEPTH = 24;
    static final int RING = 1024;
    static final long K1 = 1_000_003L;
    static final long K2 = 64L;
    static final int LARGE_EVERY = 64;
    static final int LARGE_LEN = 32 * 1024;
    static final int HUGE_EVERY = 1024;
    static final int HUGE_LEN = 256 * 1024;

    static final class Box {
        final long s;
        final int[] mid;
        final Box up;

        Box(long s, int[] mid, Box up) {
            this.s = s;
            this.mid = mid;
            this.up = up;
        }
    }

    /** 8, 16, 32, 64, 128, 256, repeating with depth. */
    static int midLen(int d) {
        return 8 << (d % 6);
    }

    /**
     * One frame of the chain. {@code mine}, {@code mid} and {@code up} are live
     * across the recursive call, which allocates the rest of the chain.
     * Returns this frame's and every deeper frame's data sum.
     */
    static long descend(long base, int d, Box up, Box[] ring, int slot, int[] bad) {
        final long s = base + d;
        final int n = midLen(d);
        final int[] mid = new int[n];
        for (int k = 0; k < n; k++) {
            mid[k] = (int) (s + k);
        }
        final Box mine = new Box(s, mid, up);
        long acc;
        if (d + 1 < DEPTH) {
            acc = descend(base, d + 1, mine, ring, slot, bad);
        } else {
            acc = churn(s, bad);
            ring[slot] = mine;
        }
        if (mine.s != s || mine.mid != mid || mine.up != up || mid.length != n) {
            bad[0]++;
        }
        acc += mine.s;
        final int si = (int) mine.s;
        for (int k = 0; k < n; k++) {
            final int delta = mid[k] - si;
            if (delta != k) {
                bad[0]++;
            }
            acc += delta;
        }
        return acc;
    }

    /**
     * Small garbage at the bottom of the chain: a 32-node list built while an
     * element of a live {@code byte[]} is written between allocations, and a
     * 32-char String. Returns 496 + 32 = 528 when intact.
     */
    static long churn(long s, int[] bad) {
        final byte[] bytes = new byte[96];
        Box head = null;
        for (int i = 0; i < 32; i++) {
            bytes[i * 3] = (byte) i;
            head = new Box(s + i, null, head);
        }
        long acc = 0;
        int expect = 31;
        for (Box b = head; b != null; b = b.up) {
            if (b.s != s + expect) {
                bad[0]++;
            }
            acc += b.s - s;
            expect--;
        }
        if (expect != -1) {
            bad[0]++;
        }
        for (int i = 0; i < 32; i++) {
            if (bytes[i * 3] != (byte) i) {
                bad[0]++;
            }
        }
        final StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 32; i++) {
            sb.append((char) ('a' + (i % 26)));
        }
        final String str = sb.toString();
        if (str.length() != 32 || str.charAt(31) != 'f') {
            bad[0]++;
        }
        return acc + str.length();
    }

    /** One call: the large arrays, when due, are live across the whole descent. */
    static long call(int t, int c, Box[] ring, int[] bad) {
        final long base = t * K1 + c * K2;
        int[] large = null;
        int[] huge = null;
        if (c % LARGE_EVERY == 0) {
            large = new int[LARGE_LEN];
            for (int k = 0; k < LARGE_LEN; k++) {
                large[k] = k ^ c;
            }
        }
        if (c % HUGE_EVERY == 0) {
            huge = new int[HUGE_LEN];
            for (int k = 0; k < HUGE_LEN; k++) {
                huge[k] = k + c;
            }
        }
        long acc = descend(base, 0, null, ring, c & (RING - 1), bad);
        if (large != null) {
            long ok = 0;
            for (int k = 0; k < LARGE_LEN; k++) {
                if (large[k] == (k ^ c)) {
                    ok++;
                } else {
                    bad[0]++;
                }
            }
            acc += ok;
        }
        if (huge != null) {
            long ok = 0;
            for (int k = 0; k < HUGE_LEN; k++) {
                if (huge[k] == k + c) {
                    ok++;
                } else {
                    bad[0]++;
                }
            }
            acc += ok;
        }
        return acc;
    }

    /**
     * Re-derive ring entry {@code j} of thread {@code t}: a 24-box chain from
     * the bottom frame up, whose base must be {@code t*K1 + c*K2} for a call
     * {@code c} with {@code c % RING == j}. Returns 276 + 173712 when intact.
     */
    static long verifyEntry(int t, int j, Box bottom, int[] bad) {
        final long base = bottom.s - (DEPTH - 1);
        final long rel = base - t * K1;
        if (rel < 0 || rel % K2 != 0 || (rel / K2) % RING != j) {
            bad[0]++;
        }
        long acc = 0;
        int d = DEPTH - 1;
        for (Box b = bottom; b != null; b = b.up, d--) {
            if (d < 0) {
                bad[0]++;
                break;
            }
            if (b.s != base + d) {
                bad[0]++;
            }
            acc += b.s - base;
            final int[] mid = b.mid;
            if (mid == null || mid.length != midLen(d)) {
                bad[0]++;
                continue;
            }
            final int si = (int) b.s;
            for (int k = 0; k < mid.length; k++) {
                final int delta = mid[k] - si;
                if (delta != k) {
                    bad[0]++;
                }
                acc += delta;
            }
        }
        if (d != -1) {
            bad[0]++;
        }
        return acc;
    }

    public static void main(String[] args) throws InterruptedException {
        final int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        final int calls = args.length > 1 ? Integer.parseInt(args[1]) : 100_000;
        final long[] results = new long[threads];
        final int[][] bads = new int[threads][1];
        final Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                final int[] bad = bads[id];
                final Box[] ring = new Box[RING];
                long acc = 0;
                for (int c = 0; c < calls; c++) {
                    acc += call(id, c, ring, bad);
                }
                for (int j = 0; j < RING; j++) {
                    if (ring[j] == null) {
                        if (j < calls) {
                            bad[0]++;
                        }
                        continue;
                    }
                    acc += verifyEntry(id, j, ring[j], bad);
                }
                results[id] = acc;
            }, "gauntlet-" + t);
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long checksum = 0;
        long badTotal = 0;
        for (int t = 0; t < threads; t++) {
            checksum += results[t];
            badTotal += bads[t][0];
        }
        final String line = "gauntlet threads=" + threads + " calls=" + calls + " depth=" + DEPTH
                + " bad=" + badTotal + " checksum=" + checksum;
        if (badTotal != 0) {
            System.out.println("FAIL " + line);
            System.exit(1);
        }
        System.out.println("PASS " + line);
    }
}
