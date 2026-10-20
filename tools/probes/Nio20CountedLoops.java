/**
 * Round nio20 wave 5, lane L3: the counted-loop shapes the optimizing tier's recogniser and full
 * unroller were widened for ({@code CRATONVM_JIT_IR_UNROLL_WIDE_COUNTED},
 * {@code CRATONVM_JIT_IR_UNROLL_PURE_BODY}). Each row is a small static method called from a hot
 * driver, so it is compiled (and, when the row is pure, unrolled) on its own and spliced into the
 * driver.
 *
 * <ul>
 *   <li>{@code t16}    -- {@code for (k = 0; k < 16; k++) h += k ^ i} (trip 16, pure body).</li>
 *   <li>{@code t12}    -- trip 12 with a wider body (still under the 160-node budget).</li>
 *   <li>{@code t40}    -- trip 40: over the cap, offered to the main/post form.</li>
 *   <li>{@code down}   -- {@code for (k = 8; k > 0; k = k - 1)} ({@code isub} stride).</li>
 *   <li>{@code from}   -- {@code for (k = i & 3; k < 16; k++)}: a non-constant init.</li>
 *   <li>{@code after}  -- a pure 16-trip loop followed by a call that THROWS for some inputs, so
 *       a frame after the loop is consulted (the exception carries the loop's result).</li>
 * </ul>
 *
 * <pre>
 *   javac -d /tmp/p20 tools/probes/Nio20CountedLoops.java
 *   java     -cp /tmp/p20 Nio20CountedLoops                      # HotSpot reference
 *   cratonvm -cp /tmp/p20 Nio20CountedLoops                      # --jdk-only
 *   cratonvm --compatible -cp /tmp/p20 Nio20CountedLoops
 *   CRATONVM_JIT_IR_UNROLL_PURE_BODY=0 cratonvm -cp /tmp/p20 Nio20CountedLoops
 *   CRATONVM_JIT_IR_UNROLL_WIDE_COUNTED=0 cratonvm -cp /tmp/p20 Nio20CountedLoops
 *   CRATONVM_DBG_UNROLL=1 cratonvm -cp /tmp/p20 Nio20CountedLoops 2>&amp;1 | grep -E 'pure-body|fully|main/post'
 * </pre>
 *
 * {@code -Dnio20.rows=a,b} selects rows (default all), {@code -Dnio20.n=} ops per timed run
 * (default 200 000). Output: one {@code check} line (identical in every VM and arm), then one
 * {@code <row> <N> ns/op} line per row.
 */
public class Nio20CountedLoops {
    static final String ROWS = System.getProperty("nio20.rows", "all");
    static final int N = Integer.getInteger("nio20.n", 200_000);

    static boolean on(String r) {
        if (ROWS.equals("all")) {
            return true;
        }
        for (String s : ROWS.split(",")) {
            if (s.equals(r)) {
                return true;
            }
        }
        return false;
    }

    static int t16(int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) {
            h += k ^ i;
        }
        return h;
    }

    static int t12(int i) {
        int h = 7;
        for (int k = 0; k < 12; k++) {
            h = h * 31 + ((k + i) ^ (k << 2)) - (i >>> k);
        }
        return h;
    }

    static int t40(int i) {
        int h = 0;
        for (int k = 0; k < 40; k++) {
            h += k * i;
        }
        return h;
    }

    static int down(int i) {
        int h = 0;
        for (int k = 8; k > 0; k = k - 1) {
            h += k * k + i;
        }
        return h;
    }

    static int from(int i) {
        int h = 0;
        for (int k = i & 3; k < 16; k++) {
            h += k ^ i;
        }
        return h;
    }

    static int boom(int h, int i) {
        if ((i & 1023) == 1023) {
            throw new IllegalStateException("h=" + h);
        }
        return h;
    }

    static int after(int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) {
            h += k ^ i;
        }
        return boom(h, i);
    }

    interface Row {
        int op(int i);
    }

    static int drive(Row r, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                acc += r.op(i);
            } catch (IllegalStateException e) {
                acc ^= e.getMessage().hashCode();
            }
        }
        return acc;
    }

    static final StringBuilder CHECK = new StringBuilder("check");
    static final StringBuilder OUT = new StringBuilder();

    static void row(String name, Row r) {
        if (!on(name)) {
            return;
        }
        int h = drive(r, N);
        h ^= drive(r, N);
        long t0 = System.nanoTime();
        h ^= drive(r, N);
        long ns = (System.nanoTime() - t0) / N;
        CHECK.append(' ').append(name).append('=').append(h);
        OUT.append(name).append(' ').append(ns).append(" ns/op\n");
    }

    public static void main(String[] args) {
        row("t16", Nio20CountedLoops::t16);
        row("t12", Nio20CountedLoops::t12);
        row("t40", Nio20CountedLoops::t40);
        row("down", Nio20CountedLoops::down);
        row("from", Nio20CountedLoops::from);
        row("after", Nio20CountedLoops::after);
        System.out.println(CHECK);
        System.out.print(OUT);
    }
}
