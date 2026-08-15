/**
 * Same body, two invoke kinds, both inside an OSR'd loop.
 *
 * The OSR compile door eagerly compiles and DIRECT-BINDS `invokestatic`
 * callees, but not statically-bound `invokespecial` ones. If that is what the
 * constructor gap costs, `stat` (bound) should be far cheaper than `spec`
 * (dispatched) even though the two callee bodies are identical.
 */
public class CallKind {
    static int field;
    static Object sink;

    static class Holder {
        int i;
        // invokespecial, non-elidable, takes an argument — the `new F(i)` shape.
        Holder(int v) { i = v; }
        // private => invokespecial too, same body, no allocation involved.
        private int spec(int v) { field = v; return v; }
    }

    // invokestatic, same body.
    static int stat(int v) { field = v; return v; }

    static final Holder H = new Holder(0);

    static long loopStat(int n) { long t=System.nanoTime(); int s=0; for (int i=0;i<n;i++) s+=stat(i);   if(s==-1) sink=H; return System.nanoTime()-t; }
    static long loopSpec(int n) { long t=System.nanoTime(); int s=0; for (int i=0;i<n;i++) s+=H.spec(i); if(s==-1) sink=H; return System.nanoTime()-t; }
    static long loopCtor(int n) { long t=System.nanoTime(); for (int i=0;i<n;i++) sink=new Holder(i);    return System.nanoTime()-t; }

    static void report(String k, int n, long ns) {
        System.out.printf("%-9s ns/op=%8.1f%n", k, (double) ns / n);
    }

    public static void main(String[] a) {
        int warm = a.length > 0 ? Integer.parseInt(a[0]) : 1_000_000;
        int n    = a.length > 1 ? Integer.parseInt(a[1]) : 20_000_000;
        loopStat(warm); loopSpec(warm); loopCtor(warm);
        report("stat", n, loopStat(n));
        report("spec", n, loopSpec(n));
        report("ctor", n, loopCtor(n));
    }
}
