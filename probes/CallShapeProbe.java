/**
 * WHICH property of a call site costs 60x?
 *
 * `probes/CallFloorProbe.java` reports an invokevirtual leaf at ~7 ns on this
 * VM. `probes/AqsBreakdownProbe.java` reports an "empty instance call" at
 * ~431 ns. Both are a compiled loop calling a tiny method. Something between
 * those two shapes costs sixty times more, and that gap is the whole story of
 * `docs/internal/aqs-thread-handoff-latency-RETIRED-20260805.md` — an
 * uncontended `ReentrantLock.lock()`/`unlock()` is 16 nested calls, and
 * 16 x 7 ns is invisible while 16 x 431 ns is 7 us.
 *
 * CallFloorProbe's leaf lives in a `static final class` and returns a `long`.
 * AqsBreakdownProbe's lives in a non-final class and returns `void`. This
 * probe varies one property at a time, everything else held identical:
 *
 *   receiver class   final vs non-final
 *   method           private vs public vs final-method-on-nonfinal-class
 *   return           long vs void
 *   dispatch         invokestatic vs invokevirtual vs invokeinterface
 *
 * Every rung is its own inline loop in its own method, so no harness
 * abstraction is inside the measurement (that trap cost this investigation two
 * wrong numbers already — see AqsBreakdownProbe's header).
 */
public final class CallShapeProbe {

    private static final int WARMUP = 200_000;
    private static final int ROUNDS = 20_000_000;

    private static long sink;

    // ---- receivers ------------------------------------------------------
    static final class FinalLeaf {
        long addLong(long a) { return a + 1; }
        void addVoid() { }
    }

    static class OpenLeaf {                       // NOT final
        long addLong(long a) { return a + 1; }
        void addVoid() { }
        private void privVoid() { }
        final void finalVoid() { }
    }

    /** A second subtype so the OpenLeaf sites could go poly if we wanted. */
    static final class OpenLeafSub extends OpenLeaf { }

    interface Adder { long add(long a); }
    static final class IfaceLeaf implements Adder {
        public long add(long a) { return a + 1; }
    }

    static long staticLeaf(long a) { return a + 1; }
    static void staticVoid() { }

    // ---- rungs ----------------------------------------------------------
    private static long rFinalLong(FinalLeaf r, int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { a = r.addLong(a); }
        sink += a; return System.nanoTime() - t0;
    }
    private static long rFinalVoid(FinalLeaf r, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { r.addVoid(); }
        return System.nanoTime() - t0;
    }
    private static long rOpenLong(OpenLeaf r, int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { a = r.addLong(a); }
        sink += a; return System.nanoTime() - t0;
    }
    private static long rOpenVoid(OpenLeaf r, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { r.addVoid(); }
        return System.nanoTime() - t0;
    }
    private static long rOpenPrivVoid(OpenLeaf r, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { r.privVoid(); }
        return System.nanoTime() - t0;
    }
    private static long rOpenFinalVoid(OpenLeaf r, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { r.finalVoid(); }
        return System.nanoTime() - t0;
    }
    private static long rIface(Adder r, int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { a = r.add(a); }
        sink += a; return System.nanoTime() - t0;
    }
    private static long rStaticLong(int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { a = staticLeaf(a); }
        sink += a; return System.nanoTime() - t0;
    }
    private static long rStaticVoid(int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { staticVoid(); }
        return System.nanoTime() - t0;
    }
    private static long rNoCall(int n) {
        long t0 = System.nanoTime(); long a = 0;
        for (int i = 0; i < n; i++) { a = a + 1; }
        sink += a; return System.nanoTime() - t0;
    }

    private static void say(String label, long nanos, int n) {
        System.out.printf("%-44s %8.2f ns/op%n", label, nanos / (double) n);
    }

    public static void main(String[] args) {
        FinalLeaf f = new FinalLeaf();
        OpenLeaf o = new OpenLeaf();
        IfaceLeaf ifl = new IfaceLeaf();

        rNoCall(WARMUP); rStaticLong(WARMUP); rStaticVoid(WARMUP);
        rFinalLong(f, WARMUP); rFinalVoid(f, WARMUP);
        rOpenLong(o, WARMUP); rOpenVoid(o, WARMUP);
        rOpenPrivVoid(o, WARMUP); rOpenFinalVoid(o, WARMUP);
        rIface(ifl, WARMUP);

        say("no call (control)", rNoCall(ROUNDS), ROUNDS);
        System.out.println();
        say("invokestatic  -> long", rStaticLong(ROUNDS), ROUNDS);
        say("invokestatic  -> void", rStaticVoid(ROUNDS), ROUNDS);
        System.out.println();
        say("FINAL class   .addLong -> long", rFinalLong(f, ROUNDS), ROUNDS);
        say("FINAL class   .addVoid -> void", rFinalVoid(f, ROUNDS), ROUNDS);
        System.out.println();
        say("open class    .addLong -> long", rOpenLong(o, ROUNDS), ROUNDS);
        say("open class    .addVoid -> void", rOpenVoid(o, ROUNDS), ROUNDS);
        say("open class    .privVoid (private)", rOpenPrivVoid(o, ROUNDS), ROUNDS);
        say("open class    .finalVoid (final method)", rOpenFinalVoid(o, ROUNDS), ROUNDS);
        System.out.println();
        say("invokeinterface -> long", rIface(ifl, ROUNDS), ROUNDS);

        if (sink == 42) { System.out.println("(unreachable)"); }
    }
}
