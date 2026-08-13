import java.util.concurrent.atomic.AtomicInteger;

/**
 * The constructor call in a method that is compiled the ordinary way (by
 * invocation count), not by OSR. The OSR loops in Rate2 are compiled BEFORE
 * their callee constructor exists, so a lookup-only direct-call resolver can
 * never bind them; this shape is the common one.
 */
public class CtorCallSiteRateProbe {
    static final AtomicInteger NEXT = new AtomicInteger();
    static int plainCounter;
    static Object sink;

    static class CtorAtomic { final int i; CtorAtomic() { i = NEXT.getAndIncrement(); } }
    static class CtorPlain  { final int i; CtorPlain()  { i = ++plainCounter; } }

    static void mkAtomic() { sink = new CtorAtomic(); }
    static void mkPlain()  { sink = new CtorPlain(); }

    static long loopAtomic(int n) { long t=System.nanoTime(); for (int i=0;i<n;i++) mkAtomic(); return System.nanoTime()-t; }
    static long loopPlain(int n)  { long t=System.nanoTime(); for (int i=0;i<n;i++) mkPlain();  return System.nanoTime()-t; }

    static void report(String name, int n, long ns) {
        System.out.printf("%-12s ns/op=%8.1f ops/s=%12.0f%n", name, (double) ns/n, n/(ns/1e9));
    }

    public static void main(String[] a) throws Exception {
        int warm = a.length > 0 ? Integer.parseInt(a[0]) : 2_000_000;
        int n    = a.length > 1 ? Integer.parseInt(a[1]) : 10_000_000;
        // Warm the CALLEES first, in small batches with a pause, so the tiered
        // manager compiles the constructors before the measured loops are OSR'd.
        for (int r = 0; r < 4; r++) {
            loopAtomic(warm / 4); loopPlain(warm / 4);
            Thread.sleep(200);
        }
        report("mkAtomic", n, loopAtomic(n));
        report("mkPlain",  n, loopPlain(n));
        System.out.println("CTOR-CALLSITE-DONE next=" + NEXT.get() + " plain=" + plainCounter);
    }
}
