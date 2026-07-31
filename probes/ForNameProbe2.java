import java.nio.charset.Charset;
import java.util.Locale;

/**
 * Isolates the TestCharsetCachePerformance control arm (`Charset.forName`) from
 * the two cached arms, and separates it from a bare `String.toLowerCase(Locale)`
 * so a change in the case-conversion natives can be told apart from a change in
 * the charset lookup itself.
 *
 * Each worker accumulates into a LOCAL and publishes once at the end — a
 * per-iteration write to a shared volatile costs more than anything being
 * measured here and hides differences of this size behind cache-line traffic.
 */
public final class ForNameProbe2 {
    static final String[] NAMES = {"ISO-8859-1","ISO-8859-2","ISO-8859-3","ISO-8859-4","ISO-8859-5"};
    static volatile long sink;

    interface Op { long run(int i); }

    static double bench(Op op, int threads, int iters) throws Exception {
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) ts[t] = new Thread(() -> {
            long local = 0;
            for (int i = 0; i < iters; i++) local += op.run(i);
            sink += local;
        });
        long s = System.nanoTime();
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        return (System.nanoTime() - s) / (double) iters;   // ns/op/thread
    }

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300_000;
        Op forName  = i -> Charset.forName(NAMES[i % 5]).hashCode();
        Op lower    = i -> NAMES[i % 5].toLowerCase(Locale.ENGLISH).length();
        Op lowerDef = i -> NAMES[i % 5].toLowerCase().length();
        // Poor-locality variant: a fresh receiver every call, so the
        // per-receiver memo can never hit and only pays for itself.
        Op lowerCold = i -> ("ISO-8859-" + i).toLowerCase(Locale.ENGLISH).length();

        bench(forName, 1, 50_000); bench(lower, 1, 50_000);
        bench(lowerDef, 1, 50_000); bench(lowerCold, 1, 50_000);

        for (int t : new int[]{1, 10}) {
            System.out.printf("threads=%-3d forName=%9.1f lower=%9.1f lowerDefault=%9.1f lowerCold=%9.1f ns/op%n",
                t, bench(forName, t, iters), bench(lower, t, iters),
                bench(lowerDef, t, iters), bench(lowerCold, t, iters));
        }
        System.out.println("sink=" + sink);
    }
}
