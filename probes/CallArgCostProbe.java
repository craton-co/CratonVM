/**
 * Does a compiled call cost more when its arguments are REFERENCES?
 *
 * `AssertChainProbe` (Azure host, 2026-08-17) put the JUnit assertion chain at
 * 55.5 ns/iter over an equivalent hand-rolled rung — roughly 14 ns for each of
 * its ~4 extra frames. Every one of those frames is compiled
 * (`hot_but_stuck_in_interpreter=0`, `c2=18`) and direct-bound
 * (`disp_calls=3776` over 2 000 000 iterations, so the generic dispatch helper
 * is not in it). Yet `CallCostProbe` prices a compiled static call at 0.84 ns
 * and a virtual one at 6.20.
 *
 * The chain's frames differ from that microbenchmark in one obvious way: they
 * take and pass **object references**, not ints. A compiled call with a live oop
 * argument has to spill it and publish an oop map for the callee's safepoint;
 * an int-only call does not. This probe is whether that is what the 14 ns is.
 *
 * Arms, each a SEPARATE once-invoked method (the shape a `@Test` body has, so
 * OSR is the only door), all with the identical loop and accumulator so the
 * only difference between two rows is the call:
 *
 *   control    — no call at all
 *   int0/1/2   — a static call taking 0, 1, 2 ints
 *   ref1/ref2  — a static call taking 1, 2 references
 *   refRet     — a static call taking and RETURNING a reference
 *   virtInt    — a virtual call taking an int
 *   virtRef    — a virtual call taking a reference
 *
 * Read the deltas against `control`, never the absolutes. If ref1 - int1 is
 * near zero the hypothesis is dead and the 14 ns is somewhere else; if it is
 * several nanoseconds, oop-argument marshalling is a cost every
 * reference-passing call in every program pays, and this class is just where it
 * happened to be measured.
 *
 *   javac -nowarn -d . probes/CallArgCostProbe.java
 *   java                       -cp . CallArgCostProbe 20000000
 *   cratonvm --java-home <jdk> -cp . CallArgCostProbe 20000000
 */
public final class CallArgCostProbe {

    static long sink;
    static final Object A = new Object();
    static final Object B = new Object();

    interface Box { int take(int v); int takeRef(Object o); }
    static final class RealBox implements Box {
        @Override public int take(int v) { return v; }
        @Override public int takeRef(Object o) { return o == null ? 1 : 0; }
    }
    static final Box BOX = new RealBox();

    static int int0()                       { return 1; }
    static int int1(int a)                  { return a; }
    static int int2(int a, int b)           { return a + b; }
    static int ref1(Object a)               { return a == null ? 1 : 0; }
    static int ref2(Object a, Object b)     { return a == b ? 1 : 0; }
    static Object refRet(Object a)          { return a; }

    static void control(int n) { long s = 0; for (int i = 0; i < n; i++) { s += i; } sink += s; }
    static void armInt0(int n) { long s = 0; for (int i = 0; i < n; i++) { s += int0(); } sink += s; }
    static void armInt1(int n) { long s = 0; for (int i = 0; i < n; i++) { s += int1(i); } sink += s; }
    static void armInt2(int n) { long s = 0; for (int i = 0; i < n; i++) { s += int2(i, i); } sink += s; }
    static void armRef1(int n) { long s = 0; for (int i = 0; i < n; i++) { s += ref1(A); } sink += s; }
    static void armRef2(int n) { long s = 0; for (int i = 0; i < n; i++) { s += ref2(A, B); } sink += s; }
    static void armRefRet(int n) { long s = 0; for (int i = 0; i < n; i++) { s += refRet(A) == null ? 1 : 0; } sink += s; }
    static void armVirtInt(int n) { long s = 0; for (int i = 0; i < n; i++) { s += BOX.take(i); } sink += s; }
    static void armVirtRef(int n) { long s = 0; for (int i = 0; i < n; i++) { s += BOX.takeRef(A); } sink += s; }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000_000;
        double base = row("control", n, time(() -> control(n)), -1);
        row("int0  ", n, time(() -> armInt0(n)), base);
        row("int1  ", n, time(() -> armInt1(n)), base);
        row("int2  ", n, time(() -> armInt2(n)), base);
        row("ref1  ", n, time(() -> armRef1(n)), base);
        row("ref2  ", n, time(() -> armRef2(n)), base);
        row("refRet", n, time(() -> armRefRet(n)), base);
        row("virtInt", n, time(() -> armVirtInt(n)), base);
        row("virtRef", n, time(() -> armVirtRef(n)), base);
        System.out.println("sink=" + sink);
    }

    static long time(Runnable r) { long a = System.nanoTime(); r.run(); return System.nanoTime() - a; }

    static double row(String name, int n, long ns, double base) {
        double per = (double) ns / n;
        if (base < 0) {
            System.out.printf("%-8s %8.2f ns/iter  (control)%n", name, per);
        } else {
            System.out.printf("%-8s %8.2f ns/iter  (+%6.2f over control)%n", name, per, per - base);
        }
        return per;
    }
}
