import java.util.concurrent.atomic.AtomicInteger;

/**
 * Splits the FastThreadLocal constructor loop into its constituent shapes, so
 * "the constructor call costs ~820 ns" can be attributed: is it the call, or
 * is it that THIS constructor body blocks inlining?
 */
public class CtorShapeRateProbe {
    static final AtomicInteger NEXT = new AtomicInteger();
    static int plainCounter;
    static Object sink;

    /** netty's FastThreadLocal shape: body calls a registered native. */
    static class CtorAtomic { final int i; CtorAtomic() { i = NEXT.getAndIncrement(); } }
    /** Same shape, plain static increment — no native in the body. */
    static class CtorPlain  { final int i; CtorPlain()  { i = ++plainCounter; } }
    /** Body writes only its own field from a parameter. */
    static class CtorField  { final int i; CtorField(int v) { i = v; } }
    /** Genuinely empty — the elidable case. */
    static class CtorEmpty  { CtorEmpty() { } }

    static long l1(int n) { long t=System.nanoTime(); for (int i=0;i<n;i++) sink=new CtorAtomic(); return System.nanoTime()-t; }
    static long l2(int n) { long t=System.nanoTime(); for (int i=0;i<n;i++) sink=new CtorPlain();  return System.nanoTime()-t; }
    static long l3(int n) { long t=System.nanoTime(); for (int i=0;i<n;i++) sink=new CtorField(i); return System.nanoTime()-t; }
    static long l4(int n) { long t=System.nanoTime(); for (int i=0;i<n;i++) sink=new CtorEmpty();  return System.nanoTime()-t; }
    static long l5(int n) { long t=System.nanoTime(); for (int i=0;i<n;i++) sink=new Object();     return System.nanoTime()-t; }
    static long l6(int n) { long t=System.nanoTime(); int s=0; for (int i=0;i<n;i++) s+=NEXT.getAndIncrement(); if (s==42) sink=new Object(); return System.nanoTime()-t; }

    static void report(String name, int n, long ns) {
        System.out.printf("%-12s ns/op=%8.1f ops/s=%12.0f%n", name, (double) ns/n, n/(ns/1e9));
    }

    public static void main(String[] a) {
        int warm = a.length > 0 ? Integer.parseInt(a[0]) : 2_000_000;
        int n    = a.length > 1 ? Integer.parseInt(a[1]) : 10_000_000;
        l1(warm); l2(warm); l3(warm); l4(warm); l5(warm); l6(warm);
        report("ctorAtomic", n, l1(n));
        report("ctorPlain",  n, l2(n));
        report("ctorField",  n, l3(n));
        report("ctorEmpty",  n, l4(n));
        report("allocObject",n, l5(n));
        report("atomicOnly", n, l6(n));
        System.out.println("CTOR-SHAPE-DONE next=" + NEXT.get() + " plain=" + plainCounter);
    }
}
