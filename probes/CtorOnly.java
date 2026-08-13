/**
 * ONE loop, one shape, nothing else — so a `perf record` profile of the whole
 * process is a profile of that shape. Pick the shape with argv[0]:
 *   field  = `new X(i)` where X(int v) { i = v; }   (no getstatic, no invoke —
 *            the shape `resolve_inline_site` already admits)
 *   atomic = `new X()`  where X()      { i = ATOMIC.getAndIncrement(); }
 *   alloc  = `new Object()`                          (the floor)
 */
import java.util.concurrent.atomic.AtomicInteger;

public class CtorOnly {
    static final AtomicInteger NEXT = new AtomicInteger();
    static Object sink;

    static class F { final int i; F(int v) { i = v; } }
    static class A { final int i; A() { i = NEXT.getAndIncrement(); } }

    static long field(int n)  { long t=System.nanoTime(); for (int i=0;i<n;i++) sink=new F(i);      return System.nanoTime()-t; }
    static long atomic(int n) { long t=System.nanoTime(); for (int i=0;i<n;i++) sink=new A();       return System.nanoTime()-t; }
    static long alloc(int n)  { long t=System.nanoTime(); for (int i=0;i<n;i++) sink=new Object();  return System.nanoTime()-t; }

    public static void main(String[] a) {
        String shape = a.length > 0 ? a[0] : "field";
        int warm = a.length > 1 ? Integer.parseInt(a[1]) : 1_000_000;
        int n    = a.length > 2 ? Integer.parseInt(a[2]) : 30_000_000;
        long ns;
        switch (shape) {
            case "atomic": atomic(warm); ns = atomic(n); break;
            case "alloc":  alloc(warm);  ns = alloc(n);  break;
            default:       field(warm);  ns = field(n);  break;
        }
        System.out.printf("%s ns/op=%.1f ops/s=%.0f%n", shape, (double) ns/n, n/(ns/1e9));
    }
}
