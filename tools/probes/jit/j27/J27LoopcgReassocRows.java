/**
 * j27 wave 1 (lane LOOPCG): fact rows for the multi-root linear reassociation
 * (CRATONVM_JIT_IR_LINEAR_REASSOC) and the select-then-test thread-word poll
 * (CRATONVM_JIT_IR_POLL_WORD_SELECT), page
 * docs/known-issues/jit/jr21-promoted-loop-bodies-spill-and-poll-every-trip-20261026.md.
 *
 * Every kernel is an unrolled-accumulation shape: a loop-carried int/long sum of
 * the induction variable, invariants and constants, at trip counts that exercise
 * the main/post split (0, 1, 3, 7, 1000) and int/long wrap-around (1_000_000 and
 * a start near Integer.MAX_VALUE). Each kernel is warmed so the optimizing tier
 * compiles it, then each trip count prints one fact line.
 *
 * java J27LoopcgReassocRows            facts only (deterministic)
 * java -Dj27.time=1 J27LoopcgReassocRows   also "time <row> <ns/op>" lines
 */
public class J27LoopcgReassocRows {
    static int COUNTER;
    static int OTHER;
    static int k = 3;
    static int bias = 11;

    static int sumInt(int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) acc += i;
        return acc;
    }

    static int sumFrom(int start, int n) {
        int acc = 5;
        for (int i = start; i < start + n; i++) acc += i * 3 + 7 - (i << 2);
        return acc;
    }

    static int subChain(int n) {
        int acc = 1;
        for (int i = 0; i < n; i++) acc = acc - i - (i + 1);
        return acc;
    }

    static int invariantScale(int n, int kk, int b) {
        int v = 0, w = 0;
        for (int i = 0; i < n; i++) {
            v += kk * k + i;
            w += b;
        }
        return v * 31 + w;
    }

    static long sumLong(int n) {
        long acc = 0x7FFF_FFFF_0000_0000L;
        for (int i = 0; i < n; i++) acc += i * 0x9E37_79B9_7F4A_7C15L + 1;
        return acc;
    }

    static int counterInt(int n) {
        COUNTER = 0;
        for (int i = 0; i < n; i++) COUNTER += i;
        return COUNTER;
    }

    static int twoCells(int n) {
        COUNTER = 0;
        OTHER = 0;
        for (int i = 0; i < n; i++) {
            COUNTER += 3;
            OTHER ^= i;
        }
        return COUNTER * 7 + OTHER;
    }

    static final int[] TRIPS = {0, 1, 3, 7, 1000, 1_000_000};

    static long all(int n) {
        long h = sumInt(n);
        h = h * 31 + sumFrom(Integer.MAX_VALUE - 5, n);
        h = h * 31 + subChain(n);
        h = h * 31 + invariantScale(n, 17, bias);
        h = h * 31 + sumLong(n);
        h = h * 31 + counterInt(n);
        h = h * 31 + twoCells(n);
        return h;
    }

    public static void main(String[] args) {
        long warm = 0;
        for (int r = 0; r < 20_000; r++) warm += all(64 + (r & 7));
        System.out.println("fact warm " + warm);
        for (int n : TRIPS) {
            System.out.println("fact sumInt-" + n + " " + sumInt(n));
            System.out.println("fact sumFrom-" + n + " " + sumFrom(Integer.MAX_VALUE - 5, n));
            System.out.println("fact subChain-" + n + " " + subChain(n));
            System.out.println("fact invariantScale-" + n + " " + invariantScale(n, 17, bias));
            System.out.println("fact sumLong-" + n + " " + sumLong(n));
            System.out.println("fact counterInt-" + n + " " + counterInt(n));
            System.out.println("fact twoCells-" + n + " " + twoCells(n));
        }
        if (Boolean.getBoolean("j27.time")) {
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                long s = 0;
                for (int r = 0; r < 2000; r++) s += counterInt(1000);
                long t1 = System.nanoTime();
                for (int r = 0; r < 2000; r++) s += twoCells(1000);
                long t2 = System.nanoTime();
                for (int r = 0; r < 2000; r++) s += invariantScale(1000, 17, bias);
                long t3 = System.nanoTime();
                System.out.println("time counterInt " + (t1 - t0) / 2000 + " " + (s & 1));
                System.out.println("time twoCells " + (t2 - t1) / 2000);
                System.out.println("time invariantScale " + (t3 - t2) / 2000);
            }
        }
    }
}
