import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

/** SBR5 warmup lane: the downstream rows other lanes measured. usage: Sbr5WarmupDownstream walk|counted|hidden [n]. Prints us/op of the first call and of the steady state. */
public class Sbr5WarmupDownstream {
    public static class Tmpl { static int c; public int f() { return ++c; } }
    static volatile Object sink;
    static int inc(int x, int i) { return x + i; }
    static int zero() { return 0; }
    static int cnt(int n) { return n; }

    public static void main(String[] a) throws Throwable {
        String row = a[0];
        int n = a.length > 1 ? Integer.parseInt(a[1]) : 100;
        Runnable op;
        switch (row) {
            case "walk": { StackWalker w = StackWalker.getInstance(); op = () -> sink = w.walk(s -> s.limit(5).count()); break; }
            case "counted": {
                MethodHandles.Lookup l = MethodHandles.lookup();
                MethodHandle init = l.findStatic(Sbr5WarmupDownstream.class, "zero", MethodType.methodType(int.class));
                MethodHandle iter = l.findStatic(Sbr5WarmupDownstream.class, "inc", MethodType.methodType(int.class, int.class, int.class));
                MethodHandle it = l.findStatic(Sbr5WarmupDownstream.class, "cnt", MethodType.methodType(int.class, int.class));
                op = () -> sink = MethodHandles.countedLoop(it, init, iter);
                break;
            }
            case "hidden": {
                byte[] bytes;
                try (InputStream in = Sbr5WarmupDownstream.class.getResourceAsStream("Sbr5WarmupDownstream$Tmpl.class")) { bytes = in.readAllBytes(); }
                MethodHandles.Lookup l = MethodHandles.lookup();
                op = () -> { try { sink = l.defineHiddenClass(bytes, true).lookupClass(); } catch (Exception e) { throw new RuntimeException(e); } };
                break;
            }
            default: throw new IllegalArgumentException(row);
        }
        long t0 = System.nanoTime(); op.run(); long first = System.nanoTime() - t0;
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 5; r++) { t0 = System.nanoTime(); for (int i = 0; i < n; i++) op.run(); best = Math.min(best, System.nanoTime() - t0); }
        System.out.println("us/op " + row + " first=" + first / 1000 + " steady=" + String.format("%.1f", best / 1000.0 / n));
    }
}
