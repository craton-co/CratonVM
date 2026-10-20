import java.util.Arrays;

/**
 * nio21 wave 17 lane L4: never-taken branch pruning ({@code CRATONVM_JIT_IR_PRUNE_NEVER_TAKEN},
 * default ON). A conditional branch whose TAKEN edge the profile never saw is compiled as a guard on
 * its fall-through; the taken edge deopts to the interpreter at the branch (HotSpot's
 * {@code unstable_if}), and after four traps at one bci the next compile keeps the branch.
 *
 * <p>Conformance (default, prints answers only; must match HotSpot 25 byte for byte): each method is
 * warmed on its hot arm only, then handed the cold arm -- a value past the limit, a null, a cold arm
 * that throws (class, message and the top three stack frames, so the interpreter's frames must be
 * the bytecode's), a cold arm holding its own loop and a join, and alternating rounds that make the
 * same site trap past its limit.
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21NeverTaken.java
 *   java -cp /tmp/p21 Nio21NeverTaken                                    # HotSpot facts
 *   cratonvm -cp /tmp/p21 Nio21NeverTaken                                # --jdk-only
 *   cratonvm --compatible -cp /tmp/p21 Nio21NeverTaken
 *   CRATONVM_JIT_IR_PRUNE_NEVER_TAKEN=0 cratonvm -cp /tmp/p21 Nio21NeverTaken
 *   CRATONVM_DBG=ir-compiles cratonvm -cp /tmp/p21 Nio21NeverTaken 2>&1 | grep 'never-taken'
 *   cratonvm -cp /tmp/p21 -Dnio21.rows=nt-loop Nio21NeverTaken           # bench: nt-loop N ns/op
 * </pre>
 */
public class Nio21NeverTaken {
    static final String ROWS = System.getProperty("nio21.rows", "");
    static final int WARM = Integer.getInteger("nio21.warm", 40_000);
    static volatile long sink;

    /** The hot arm adds; the cold arm (x >= lim) calls out and multiplies. */
    static long scan(int[] a, int lim) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            int x = a[i];
            // javac: `if_icmpge` to the cold arm, so the TAKEN edge is the cold one.
            if (x < lim) {
                s += x;
            } else {
                s += cold(x) * 3L;
            }
        }
        return s;
    }

    static long cold(int x) {
        return (long) x * x + 7;
    }

    /** The cold arm (o == null) answers -1. */
    static int len(Object o) {
        if (o != null) {
            return o.hashCode() & 0xff;
        }
        return -1;
    }

    /** The cold arm (x < 0) throws from a callee, so the trace has two Java frames above main's. */
    static int checked(int x) {
        if (x >= 0) {
            return x + 1;
        }
        throw boom(x);
    }

    static IllegalStateException boom(int x) {
        return new IllegalStateException("negative " + x);
    }

    /** The cold arm (k > 100) holds a loop of its own and rejoins before the return. */
    static int coldLoop(int k) {
        int r;
        if (k <= 100) {
            r = k;
        } else {
            int t = 0;
            for (int j = 0; j < k % 13; j++) {
                t += j * k;
            }
            r = t ^ k;
        }
        return r * 2;
    }

    static void conf() {
        int[] hot = new int[64];
        for (int i = 0; i < hot.length; i++) {
            hot[i] = i;
        }
        long s = 0;
        for (int w = 0; w < WARM; w++) {
            s += scan(hot, 1000);
            s += len("k" + (w & 7));
            s += checked(w & 1023);
            s += coldLoop(w & 63);
        }
        System.out.println("warm " + s);

        int[] spiked = hot.clone();
        spiked[40] = 5000;
        System.out.println("scan-cold " + scan(spiked, 1000));
        System.out.println("len-null " + len(null));
        System.out.println("coldLoop " + coldLoop(1234) + " " + coldLoop(101) + " " + coldLoop(7));
        try {
            checked(-5);
            System.out.println("checked no throw");
        } catch (IllegalStateException e) {
            StackTraceElement[] st = e.getStackTrace();
            System.out.println("checked " + e.getClass().getName() + ": " + e.getMessage());
            for (int i = 0; i < Math.min(3, st.length); i++) {
                System.out.println("  at " + st[i]);
            }
        }
        // The same sites trapped over and over: past the per-bci limit the next compile keeps the
        // branch; every answer stays the bytecode's.
        for (int round = 0; round < 12; round++) {
            long r = 0;
            for (int w = 0; w < 3_000; w++) {
                r += scan(hot, 1000);
            }
            r += scan(spiked, 1000);
            r += len(round % 3 == 0 ? null : "x");
            r += coldLoop(round * 50);
            System.out.println("round " + round + " " + r);
        }
        sink = s;
    }

    static void bench() {
        int[] hot = new int[4096];
        for (int i = 0; i < hot.length; i++) {
            hot[i] = i & 511;
        }
        for (String row : ROWS.split(",")) {
            if (!row.equals("nt-loop")) {
                System.out.println(row + " unknown");
                continue;
            }
            for (int w = 0; w < 5_000; w++) {
                sink += scan(hot, 1000);
            }
            int n = Integer.getInteger("nio21.n", 20_000);
            long t0 = System.nanoTime();
            for (int k = 0; k < n; k++) {
                sink += scan(hot, 1000);
            }
            System.out.println(row + " " + (System.nanoTime() - t0) / n + " ns/op");
        }
    }

    public static void main(String[] args) {
        if (ROWS.isEmpty()) {
            conf();
        } else {
            bench();
        }
        if (sink == 42) {
            System.out.println(Arrays.toString(args));
        }
    }
}
