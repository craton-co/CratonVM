import java.lang.ref.Reference;
import java.util.Arrays;

/**
 * Round nio20 wave 4, lane L3: a decomposition of {@code R17NioRows fence} (HotSpot 6 ns/op,
 * CratonVM w3 43 ns/op under --jdk-only). That row times ONE call per op of
 *
 * <pre>
 *   static int fenceRow(int i) {           // 39 bytes of bytecode
 *       int h = 0;
 *       for (int k = 0; k < 16; k++) {
 *           Object o = PLAIN[(i + k) &amp; 7];
 *           h += k ^ i;
 *           Reference.reachabilityFence(o);
 *       }
 *       return h;
 *   }
 * </pre>
 *
 * from the loop of a SHARED {@code time(int r)} driver whose {@code switch} picks the row. The
 * driver's loop body is compiled (an OSR body) during the FIRST row it times, so the
 * {@code fenceRow} call site is cold in the profile the body was planned on. Each row below
 * removes one ingredient, so the differences name what the 43 ns is made of:
 *
 * <ul>
 *   <li>{@code shared}   -- the R17 shape: the shared switch driver, run for another row first.</li>
 *   <li>{@code own}      -- the same callee from a driver of its own (the site hot when planned).</li>
 *   <li>{@code nofence}  -- {@code own} without the fence (the element load kept, its value used).</li>
 *   <li>{@code noload}   -- the counted loop alone ({@code h += k ^ i}; HotSpot folds it).</li>
 *   <li>{@code hoisted}  -- {@code own} with {@code PLAIN} read once into a local before the loop.</li>
 *   <li>{@code empty}    -- the call of an empty static method (the call's own floor).</li>
 *   <li>{@code inline}   -- the 16-iteration loop written inline in the driver (no call).</li>
 * </ul>
 *
 * Expected reading: {@code shared - own} is the cold-site planning penalty (the call kept),
 * {@code own - nofence} the fence's own cost (should be ~0 since the IR keep-alive),
 * {@code nofence - noload} the per-iteration static + element load, its null/bounds checks and
 * poll, {@code empty} the call floor.
 *
 * <pre>
 *   javac -d /tmp/p20 tools/probes/Nio20FenceRow.java
 *   java    -cp /tmp/p20 Nio20FenceRow                        # HotSpot reference
 *   cratonvm -cp /tmp/p20 Nio20FenceRow                       # --jdk-only (launcher default)
 *   cratonvm --compatible -cp /tmp/p20 Nio20FenceRow
 *   CRATONVM_JIT_IR_REACHABILITY_FENCE_KEEPALIVE=0 cratonvm -cp /tmp/p20 Nio20FenceRow  # fence as a call
 *   CRATONVM_DBG_JITC=1 cratonvm -cp /tmp/p20 Nio20FenceRow 2>&amp;1 | grep -E 'fenceRow|osr-splice|ir-splice'
 * </pre>
 *
 * {@code -Dnio20.rows=a,b} selects rows (default all), {@code -Dnio20.n=} the ops per timed run
 * (default 200 000). Output: one {@code check} line (identical in every VM and arm), then one
 * {@code <row> <N> ns/op} line per row.
 */
public class Nio20FenceRow {
    static final String ROWS = System.getProperty("nio20.rows", "all");
    static final int N = Integer.getInteger("nio20.n", 200_000);
    static final Object[] PLAIN = {
        "s", Integer.valueOf(1), Long.valueOf(2L), new StringBuilder("x"),
        new Object(), new int[0], Boolean.TRUE, Object.class
    };

    static boolean on(String r) {
        return ROWS.equals("all") || Arrays.asList(ROWS.split(",")).contains(r);
    }

    // ---- the callees ----

    static int fenceRow(int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) {
            Object o = PLAIN[(i + k) & 7];
            h += k ^ i;
            Reference.reachabilityFence(o);
        }
        return h;
    }

    static int noFenceRow(int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) {
            Object o = PLAIN[(i + k) & 7];
            h += (k ^ i) + (o == null ? 1 : 0);
        }
        return h;
    }

    static int noLoadRow(int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) {
            h += k ^ i;
        }
        return h;
    }

    static int hoistedRow(int i) {
        Object[] plain = PLAIN;
        int h = 0;
        for (int k = 0; k < 16; k++) {
            Object o = plain[(i + k) & 7];
            h += k ^ i;
            Reference.reachabilityFence(o);
        }
        return h;
    }

    static int emptyRow(int i) {
        return i;
    }

    // ---- the drivers ----

    // The R17 shape: one shared loop, the row picked per op by a switch. Row 0 runs first, so
    // the loop's compiled body is planned on a profile where only `case 0` was reached.
    static int shared(int r, int n) {
        int h = 0;
        for (int i = 0; i < n; i++) {
            switch (r) {
                case 0: h += noLoadRow(i); break;
                case 1: h += emptyRow(i); break;
                default: h += fenceRow(i); break;
            }
        }
        return h;
    }

    static int own(int n) {
        int h = 0;
        for (int i = 0; i < n; i++) {
            h += fenceRow(i);
        }
        return h;
    }

    static int nofence(int n) {
        int h = 0;
        for (int i = 0; i < n; i++) {
            h += noFenceRow(i);
        }
        return h;
    }

    static int noload(int n) {
        int h = 0;
        for (int i = 0; i < n; i++) {
            h += noLoadRow(i);
        }
        return h;
    }

    static int hoisted(int n) {
        int h = 0;
        for (int i = 0; i < n; i++) {
            h += hoistedRow(i);
        }
        return h;
    }

    static int empty(int n) {
        int h = 0;
        for (int i = 0; i < n; i++) {
            h += emptyRow(i);
        }
        return h;
    }

    static int inline(int n) {
        int h = 0;
        for (int i = 0; i < n; i++) {
            for (int k = 0; k < 16; k++) {
                Object o = PLAIN[(i + k) & 7];
                h += k ^ i;
                Reference.reachabilityFence(o);
            }
        }
        return h;
    }

    interface Driver {
        int run(int n);
    }

    static final StringBuilder CHECK = new StringBuilder("check");
    static final StringBuilder OUT = new StringBuilder();

    // Two untimed runs (warm-up, as R17's `time(r); NS[r] = time(r);`), then one timed run.
    static void row(String name, Driver d) {
        if (!on(name)) {
            return;
        }
        int h = d.run(N);
        h ^= d.run(N);
        long t0 = System.nanoTime();
        h ^= d.run(N);
        long ns = (System.nanoTime() - t0) / N;
        CHECK.append(' ').append(name).append('=').append(h);
        OUT.append(name).append(' ').append(ns).append(" ns/op\n");
    }

    public static void main(String[] args) {
        // R17 warms `fenceRow` from elsewhere first (`fenceFacts`): 2 x N calls.
        int warm = 0;
        for (int i = 0; i < 2 * N; i++) {
            warm += fenceRow(i);
        }
        CHECK.append(" warm=").append(warm);
        // The shared driver times row 0 (twice) and row 1 before row 2, like R17's `time(r)`.
        if (on("shared")) {
            int h = shared(0, N) ^ shared(0, N) ^ shared(1, N) ^ shared(1, N);
            h ^= shared(2, N);
            long t0 = System.nanoTime();
            h ^= shared(2, N);
            long ns = (System.nanoTime() - t0) / N;
            CHECK.append(" shared=").append(h);
            OUT.append("shared ").append(ns).append(" ns/op\n");
        }
        row("own", Nio20FenceRow::own);
        row("nofence", Nio20FenceRow::nofence);
        row("noload", Nio20FenceRow::noload);
        row("hoisted", Nio20FenceRow::hoisted);
        row("empty", Nio20FenceRow::empty);
        row("inline", Nio20FenceRow::inline);
        System.out.println(CHECK);
        System.out.print(OUT);
    }
}
