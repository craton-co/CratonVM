// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w4/young4 (2026-09-24): young-generation EVACUATION throughput with a
 * medium live set.
 *
 * <p>A ring of {@code live} nodes (each a node object plus a small {@code int[]}
 * payload, ~72 bytes together) is kept reachable while every iteration replaces
 * one pseudo-randomly chosen slot with a fresh node and drops one short-lived
 * {@code byte[]}. So every young collection finds a live set spread over the
 * whole nursery — the replaced slots are young, the untouched ones age and get
 * promoted — which is the shape where the copy loop ({@code cheney_drain} /
 * {@code evac_drain}) and promotion ({@code PromotionQueue} on the serial path,
 * per-worker buffers on the parallel one) dominate the pause.
 *
 * <p>The checksum folds the value of every node the program REPLACES and, at
 * the end, every node still in the ring; every payload is re-derived from its
 * node's value and checked, so a mis-copied or mis-promoted object is a FAIL,
 * not a quiet wrong number. Deterministic and independent of the collector.
 *
 * <p>Expected output (defaults; HotSpot 25, any collector):
 * <pre>
 *   PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0
 * </pre>
 * and nothing else on stdout. {@code time_ms=...} (the whole run) and the
 * {@code [evac-timing]} line go to STDERR (gce e2/y; {@code time_ms} used to be
 * a second stdout line, which made every battery comparison DIFF).
 * Commands (A/B the serial and parallel copy phases; interleave ABBA, compare
 * {@code cheney_drain} / {@code evac_drain} medians from {@code [gcpause]} and
 * {@code [GC] promote_pressure:} / {@code objects_promoted}):
 * <pre>
 *   java -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
 *   CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
 *   CRATONVM_GC_PAR_EVAC=0 CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
 *   CRATONVM_GC_PAR_EVAC=0 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -cp tools/bench GenR4W4EvacThroughputProbe 65536 20000000
 * </pre>
 * The last line is the small-heap (serial, below {@code CRATONVM_GC_PAR_MIN_BYTES})
 * shape the serial promotion buffer is for; its expected line is
 * {@code PASS evac live=65536 iters=20000000 checksum=9065873663750453210 corrupt=0}.
 * (Both values were computed by an exact 64-bit model of the loop and the
 * model was checked against HotSpot 25 at {@code 1000 5000}:
 * {@code checksum=2129108302571970058}.)
 * Optional args: {@code live iters}. Exit status 1 on FAIL.
 *
 * <p>gce e2/y: a SHORT arm (a fifth of the iterations, under a minute even
 * with {@code --nojit} on a loaded host), from the same model:
 * {@code 262144 4000000} prints
 * {@code PASS evac live=262144 iters=4000000 checksum=6471610074841698382 corrupt=0};
 * {@code 65536 4000000} prints
 * {@code PASS evac live=65536 iters=4000000 checksum=7690339906893138022 corrupt=0}.
 * Every run also prints one {@code [evac-timing]} line on STDERR (the main
 * loop's tenths, their median and the steady-state median, excluding the
 * first tenth); compare {@code steady_median_ms} across interleaved runs.
 */
public final class GenR4W4EvacThroughputProbe {
    static final class Node {
        final long value;
        final int[] payload;

        Node(long value) {
            this.value = value;
            this.payload = new int[4];
            for (int k = 0; k < 4; k++) {
                payload[k] = (int) (value * (k + 1));
            }
        }

        boolean intact() {
            for (int k = 0; k < 4; k++) {
                if (payload[k] != (int) (value * (k + 1))) {
                    return false;
                }
            }
            return true;
        }
    }

    static volatile Object sink;

    /**
     * gce e2/y: one STDERR line per run, for comparing medians across runs on
     * a noisy host: every tenth of the main loop, their median, and the median
     * of the last {@code n - 1} (the steady state, after the JIT warms up).
     * <pre>
     *   [evac-timing] epochs=10 epoch_ms=a,b,...,j median_ms=M steady_median_ms=S loop_ms=L
     * </pre>
     */
    static void reportTiming(long[] epochMs, int n, long loopMs) {
        final StringBuilder sb = new StringBuilder("[evac-timing] epochs=").append(n).append(" epoch_ms=");
        for (int i = 0; i < n; i++) {
            if (i > 0) {
                sb.append(',');
            }
            sb.append(epochMs[i]);
        }
        sb.append(" median_ms=").append(median(epochMs, 0, n));
        sb.append(" steady_median_ms=").append(n > 1 ? median(epochMs, 1, n) : epochMs[0]);
        sb.append(" loop_ms=").append(loopMs);
        System.err.println(sb);
    }

    static long median(long[] a, int from, int to) {
        final long[] c = java.util.Arrays.copyOfRange(a, from, to);
        java.util.Arrays.sort(c);
        final int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2;
    }

    public static void main(String[] args) {
        final int live = args.length > 0 ? Integer.parseInt(args[0]) : 262144;
        final long iters = args.length > 1 ? Long.parseLong(args[1]) : 20_000_000L;
        final long t0 = System.nanoTime();

        final Node[] ring = new Node[live];
        long seed = 0x9E3779B97F4A7C15L;
        for (int i = 0; i < live; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            ring[i] = new Node(seed);
        }
        long checksum = 0;
        long corrupt = 0;
        // gce e2/y: the loop's wall time per tenth, printed on STDERR only (the
        // stdout verdict stays deterministic). One compare per iteration.
        final int epochs = 10;
        final long[] epochMs = new long[epochs];
        final long epochLen = Math.max(1, iters / epochs);
        long nextMark = epochLen;
        int epoch = 0;
        long tEpoch = System.nanoTime();
        final long tLoop = tEpoch;
        for (long it = 0; it < iters; it++) {
            if (it == nextMark && epoch < epochs - 1) {
                final long now = System.nanoTime();
                epochMs[epoch++] = (now - tEpoch) / 1_000_000;
                tEpoch = now;
                nextMark += epochLen;
            }
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            final int idx = (int) ((seed >>> 33) % live);
            final Node old = ring[idx];
            if (!old.intact()) {
                corrupt++;
            }
            checksum = checksum * 31 + old.value;
            ring[idx] = new Node(seed ^ it);
            // One short-lived object per iteration: the garbage the young
            // collector reclaims for free around the live ring.
            sink = new byte[48];
        }
        final long tEnd = System.nanoTime();
        epochMs[epoch] = (tEnd - tEpoch) / 1_000_000;
        reportTiming(epochMs, epoch + 1, (tEnd - tLoop) / 1_000_000);
        for (int i = 0; i < live; i++) {
            if (!ring[i].intact()) {
                corrupt++;
            }
            checksum = checksum * 31 + ring[i].value;
        }
        sink = null;
        final String verdict = corrupt == 0 ? "PASS" : "FAIL";
        System.out.println(verdict + " evac live=" + live + " iters=" + iters
                + " checksum=" + checksum + " corrupt=" + corrupt);
        // gce e2/y: stderr, so stdout is exactly HotSpot's one line.
        System.err.println("time_ms=" + (System.nanoTime() - t0) / 1_000_000);
        if (corrupt != 0) {
            System.exit(1);
        }
    }
}
