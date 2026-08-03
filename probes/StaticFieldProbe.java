/**
 * What does reading a `static final` field cost in compiled code?
 *
 * Found while decomposing the virtual-call gap: a loop calling
 * `LEAF.addOne(i)` cost 42 ns/op, and hoisting `LEAF` into a local dropped it
 * to 9.4 — so ~33 ns/op was the FIELD READ, not the dispatch. The emitted code
 * showed why: a `mov rax,<helper>; call rax` per iteration, where HotSpot
 * constant-folds a `static final` reference to its address.
 *
 * This isolates the read. Every rung runs the identical loop body and differs
 * only in where the value comes from.
 *
 * Usage: StaticFieldProbe [iterations]
 */
public final class StaticFieldProbe {

    static final class Box { int v = 7; final int get() { return v; } }

    static final Box SFINAL = new Box();      // static final reference
    static       Box SMUT   = new Box();      // plain static reference
    static final int SFINAL_INT = 7;          // static final primitive (compile-time constant)
    static       int SMUT_INT   = 7;          // plain static primitive

    final Box ifield = new Box();
    static final StaticFieldProbe INST = new StaticFieldProbe();

    static volatile long sink;

    /** Control: no field access at all. */
    static long control(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += i ^ (acc >>> 7); }
        return acc;
    }

    /** static final REFERENCE read inside the loop. */
    static long staticFinalRef(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += (SFINAL.v + i) ^ (acc >>> 7); }
        return acc;
    }

    /** Same, hoisted — the value the loop actually needs is loop-invariant. */
    static long staticFinalRefHoisted(int n) {
        long acc = 0;
        Box b = SFINAL;
        for (int i = 0; i < n; i++) { acc += (b.v + i) ^ (acc >>> 7); }
        return acc;
    }

    /** Mutable static reference — cannot be folded, but should still be one load. */
    static long staticMutRef(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += (SMUT.v + i) ^ (acc >>> 7); }
        return acc;
    }

    /** static final int — javac inlines this as a constant; a floor check. */
    static long staticFinalInt(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += (SFINAL_INT + i) ^ (acc >>> 7); }
        return acc;
    }

    /** Plain static int — a real getstatic of a primitive. */
    static long staticMutInt(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += (SMUT_INT + i) ^ (acc >>> 7); }
        return acc;
    }

    /** Instance field through a hoisted receiver — the getfield comparison. */
    static long instanceField(int n) {
        long acc = 0;
        StaticFieldProbe p = INST;
        for (int i = 0; i < n; i++) { acc += (p.ifield.v + i) ^ (acc >>> 7); }
        return acc;
    }

    interface Rung { long run(int n); String name(); }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 3_000_000;

        String[] names = {
            "control (no field)", "static final REF", "static final REF hoisted",
            "static mutable REF", "static final int", "static mutable int",
            "instance field",
        };

        // 1200, not 200: a rung must be INVOKED past the tier-up threshold
        // (c1_threshold=500) to run compiled from entry. At 200 the only route
        // into compiled code is OSR, and OSR entry into these loops is refused
        // today ("osr-entry-unresumable-exit", see this probe's doc in
        // docs/internal/) — so every rung measured the interpreter and read
        // ~90 ns/op flat, JIT and --nojit alike. Verify with
        // `CRATONVM_DBG=jit-method-stats` (or simply: the control rung must
        // land near HotSpot's ~1 ns/op, not near 90).
        for (int w = 0; w < 1200; w++) {
            sink += control(2_000); sink += staticFinalRef(2_000);
            sink += staticFinalRefHoisted(2_000); sink += staticMutRef(2_000);
            sink += staticFinalInt(2_000); sink += staticMutInt(2_000);
            sink += instanceField(2_000);
        }
        sink += control(200_000); sink += staticFinalRef(200_000);
        sink += staticFinalRefHoisted(200_000); sink += staticMutRef(200_000);
        sink += staticFinalInt(200_000); sink += staticMutInt(200_000);
        sink += instanceField(200_000);

        double[] r = new double[7];
        long s;
        s = System.nanoTime(); sink += control(n);               r[0] = ns(s, n);
        s = System.nanoTime(); sink += staticFinalRef(n);        r[1] = ns(s, n);
        s = System.nanoTime(); sink += staticFinalRefHoisted(n); r[2] = ns(s, n);
        s = System.nanoTime(); sink += staticMutRef(n);          r[3] = ns(s, n);
        s = System.nanoTime(); sink += staticFinalInt(n);        r[4] = ns(s, n);
        s = System.nanoTime(); sink += staticMutInt(n);          r[5] = ns(s, n);
        s = System.nanoTime(); sink += instanceField(n);         r[6] = ns(s, n);

        for (int i = 0; i < r.length; i++) {
            System.out.printf("%-26s %8.2f ns/op  (+%.2f over control)%n",
                names[i], r[i], r[i] - r[0]);
        }
        System.out.println("sink=" + sink);
    }

    static double ns(long start, int n) { return (System.nanoTime() - start) / (double) n; }
}
