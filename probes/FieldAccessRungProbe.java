/**
 * What does ONE field access cost, by kind?
 *
 * `probes/LockEntryProbe.java` leaves ~700 ns of an uncontended
 * `ReentrantLock` lock/unlock pair unattributed, and four hypotheses are dead:
 * it is not JIT re-entry (`jit_entries` = 2 438 for 4 000 000 pairs), not the
 * interpreter (`--nojit` is 4.0x slower on the same arm against 51x on the
 * empty control), not the native-shadow caller seal
 * (`CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL=0` engages completely and moves
 * nothing) and not deoptimisation (`deopts=0`).
 *
 * What is left in that chain, once the two `setExclusiveOwnerThread` natives
 * are subtracted, is ordinary compiled Java over AQS's fields — and every one
 * of the fields it touches is `volatile`: `AbstractQueuedSynchronizer.state`,
 * `.head`, `.tail`. `getState()`/`setState()`/`signalNext()` are volatile reads
 * and writes wearing accessor clothes.
 *
 * So: price a field access by KIND, against a plain one, and against HotSpot.
 * If a volatile int write costs ~1 ns here as it does there, this is not the
 * answer and the next hypothesis is somewhere else entirely. If it costs tens
 * of ns, ~10 of them per pair IS the residual.
 *
 * Every rung is its own method with its own receiver class, so no call site
 * goes bimorphic and no loop is inlined into `main` (which on this VM measures
 * the interpreter). Multi-pass; read the LAST, and if `plainInt` moved between
 * the first pass and the last, throw the run away.
 *
 *   javac -d out probes/FieldAccessRungProbe.java
 *   java      -cp out FieldAccessRungProbe        # control
 *   cratonvm --java-home $JDK -cp out FieldAccessRungProbe
 */
public final class FieldAccessRungProbe {

    private static final int PASSES = 4;
    private static final int ROUNDS = 2_000_000;

    private static long sink;
    private static Object osink;

    static final class PlainHolder   { int v; long w; Object o; }
    static final class VolatileHolder { volatile int v; volatile long w; volatile Object o; }

    // ---- rungs ----------------------------------------------------------

    private static long rControl(int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) { a += i; }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rPlainInt(PlainHolder h, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) { h.v = i; a += h.v; }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rVolatileInt(VolatileHolder h, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) { h.v = i; a += h.v; }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rVolatileIntRead(VolatileHolder h, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) { a += h.v; }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rVolatileIntWrite(VolatileHolder h, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { h.v = i; }
        return System.nanoTime() - t0;
    }

    private static long rPlainLong(PlainHolder h, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) { h.w = i; a += h.w; }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rVolatileLong(VolatileHolder h, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) { h.w = i; a += h.w; }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rPlainRef(PlainHolder h, Object v, int n) {
        long t0 = System.nanoTime();
        Object o = null;
        for (int i = 0; i < n; i++) { h.o = v; o = h.o; }
        osink = o;
        return System.nanoTime() - t0;
    }

    private static long rVolatileRef(VolatileHolder h, Object v, int n) {
        long t0 = System.nanoTime();
        Object o = null;
        for (int i = 0; i < n; i++) { h.o = v; o = h.o; }
        osink = o;
        return System.nanoTime() - t0;
    }

    // ---- driver ---------------------------------------------------------

    private interface Rung { long run(int n); }

    private static void pass(String label, Rung r) {
        System.out.printf("%-26s", label);
        for (int p = 0; p < PASSES; p++) {
            long dt = r.run(ROUNDS);
            System.out.printf("%11.2f", dt / (double) ROUNDS);
        }
        System.out.println();
    }

    public static void main(String[] args) {
        PlainHolder plain = new PlainHolder();
        VolatileHolder vol = new VolatileHolder();
        Object v = new Object();

        System.out.printf("%-26s%11s%11s%11s%11s%n",
                "rung", "1", "2", "3", "4");
        pass("control: empty loop",     n -> rControl(n));
        pass("plain int  write+read",   n -> rPlainInt(plain, n));
        pass("volatile int write+read", n -> rVolatileInt(vol, n));
        pass("volatile int read only",  n -> rVolatileIntRead(vol, n));
        pass("volatile int write only", n -> rVolatileIntWrite(vol, n));
        pass("plain long write+read",   n -> rPlainLong(plain, n));
        pass("volatile long write+read", n -> rVolatileLong(vol, n));
        pass("plain ref write+read",    n -> rPlainRef(plain, v, n));
        pass("volatile ref write+read", n -> rVolatileRef(vol, v, n));
        System.out.println("sink=" + (sink == 0 ? 1 : 0) + " osink=" + (osink == null ? 1 : 0));
    }
}
