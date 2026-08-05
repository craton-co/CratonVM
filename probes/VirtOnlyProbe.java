/**
 * The virtual-call rung of `CallFloorProbe`, on its own.
 *
 * `CallFloorProbe` exercises five bodies, and the JDK calls in the other four
 * flood any per-call diagnostic that caps its output — the first N lines came
 * back entirely `java/lang/String.length`, hiding the rung under test. This
 * probe calls exactly one final method on a final class, so a capped
 * diagnostic prints the site you actually care about.
 *
 * Usage: VirtOnlyProbe [iterations]
 */
public final class VirtOnlyProbe {

    static final class Leaf {
        int addOne(int i) { return i + 1; }
    }

    interface Adder { int addOne(int i); }
    static final class IfaceLeaf implements Adder {
        public int addOne(int i) { return i + 1; }
    }

    static final Leaf LEAF = new Leaf();
    static final Adder IFACE = new IfaceLeaf();

    static volatile long sink;

    /** Call-free control: the identical loop body with no call at all. */
    static long arith(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += i ^ (acc >>> 7); }
        return acc;
    }

    static int addOneStatic(int i) { return i + 1; }

    /** invokestatic, same loop — the already-fast rung, as the reference. */
    static long staticCall(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += addOneStatic(i) ^ (acc >>> 7); }
        return acc;
    }

    // The receiver is hoisted into a LOCAL before the loop on purpose.
    //
    // Reading `LEAF` from its static field inside the loop made every
    // iteration pay a getstatic helper CALL that the `staticCall` rung never
    // pays — so the "virtual dispatch" column was really "static field read +
    // dispatch", and ~30 of its ~40 ns belonged to the field read. Hoisting
    // leaves exactly one difference between these rungs and `staticCall`: how
    // the call itself is dispatched.
    static long virtualCall(int n) {
        long acc = 0;
        Leaf r = LEAF;
        for (int i = 0; i < n; i++) { acc += r.addOne(i) ^ (acc >>> 7); }
        return acc;
    }

    static long ifaceCall(int n) {
        long acc = 0;
        Adder r = IFACE;
        for (int i = 0; i < n; i++) { acc += r.addOne(i) ^ (acc >>> 7); }
        return acc;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 5_000_000;

        // 1200 invocations, not 200: past the tier-up threshold
        // (c1_threshold=500) each rung runs compiled from entry, which is the
        // tier this probe means to measure. Lowering it to 200 is the way to
        // isolate OSR instead — below the threshold, an OSR entry at the loop
        // header is the only route into compiled code.
        //
        // Both routes were dead until 2026-08-03: every OSR entry was refused
        // `osr-entry-unresumable-exit`, so at 200 every rung silently measured
        // the INTERPRETER at ~90 ns/op, identically under `--nojit`. See
        // osr-entry-unresumable-exit-FIXED-20260803.md. The cheap
        // check, either way: the control rung must land near HotSpot's ~1
        // ns/op, not near 90.
        for (int r = 0; r < 1200; r++) {
            sink += arith(2_000); sink += staticCall(2_000);
            sink += virtualCall(2_000); sink += ifaceCall(2_000);
        }
        sink += arith(Math.min(n, 200_000));
        sink += staticCall(Math.min(n, 200_000));
        sink += virtualCall(Math.min(n, 200_000));
        sink += ifaceCall(Math.min(n, 200_000));

        long s;
        s = System.nanoTime(); sink += arith(n);
        double a = (System.nanoTime() - s) / (double) n;
        s = System.nanoTime(); sink += staticCall(n);
        double t = (System.nanoTime() - s) / (double) n;
        s = System.nanoTime(); sink += virtualCall(n);
        double v = (System.nanoTime() - s) / (double) n;
        s = System.nanoTime(); sink += ifaceCall(n);
        double f = (System.nanoTime() - s) / (double) n;

        System.out.printf("arith (no call)  %8.2f ns/op%n", a);
        System.out.printf("invokestatic     %8.2f ns/op  (+%.2f over control)%n", t, t - a);
        System.out.printf("invokevirtual    %8.2f ns/op  (+%.2f over control)%n", v, v - a);
        System.out.printf("invokeinterface  %8.2f ns/op  (+%.2f over control)%n", f, f - a);
        System.out.println("sink=" + sink);
    }
}
