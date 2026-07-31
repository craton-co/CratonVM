import java.nio.charset.Charset;
import java.util.HashMap;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;

/**
 * `get` on the same warm map, reached through four different static types.
 *
 * CratonVM recognises a *thin direct helper* for a `get` call site by its
 * constant-pool class, so the declared type of the field decides whether the
 * call gets the direct helper or the generic dispatcher — a distinction that
 * makes an apples-to-oranges comparison very easy to draw by accident.
 * Tomcat's `CharsetCache` declares its field `ConcurrentMap`, so row 3 is the
 * one that matters for
 * `docs/.../23-charsetcache-pathological-slowdown.md`.
 *
 * Keys are already lower-case, so no case conversion is involved.
 *
 * Usage: ChmVsHashMapProbe [iterations] [threads]
 */
public final class ChmVsHashMapProbe {

    static final String[] KEYS =
            { "iso-8859-1", "iso-8859-2", "iso-8859-3", "iso-8859-4", "iso-8859-5" };

    static void fill(Map<String,Charset> into) {
        for (Charset cs : Charset.availableCharsets().values()) {
            into.put(cs.name().toLowerCase(Locale.ENGLISH), cs);
            for (String alias : cs.aliases()) {
                into.put(alias.toLowerCase(Locale.ENGLISH), cs);
            }
        }
    }

    interface Arm { Charset get(String k); String name(); }

    static final class MapOverHashMap implements Arm {
        private final Map<String,Charset> m = new HashMap<>();
        MapOverHashMap() { fill(m); }
        public Charset get(String k) { return m.get(k); }
        public String name() { return "Map field -> HashMap"; }
    }

    static final class MapOverChm implements Arm {
        private final Map<String,Charset> m = new ConcurrentHashMap<>();
        MapOverChm() { fill(m); }
        public Charset get(String k) { return m.get(k); }
        public String name() { return "Map field -> ConcurrentHashMap"; }
    }

    static final class ConcurrentMapOverChm implements Arm {
        private final ConcurrentMap<String,Charset> m = new ConcurrentHashMap<>();
        ConcurrentMapOverChm() { fill(m); }
        public Charset get(String k) { return m.get(k); }
        public String name() { return "ConcurrentMap field -> CHM"; }
    }

    static final class ChmField implements Arm {
        private final ConcurrentHashMap<String,Charset> m = new ConcurrentHashMap<>();
        ChmField() { fill(m); }
        public Charset get(String k) { return m.get(k); }
        public String name() { return "ConcurrentHashMap field"; }
    }

    static volatile long sink;

    static double time(Arm arm, int threads, int iterations) throws Exception {
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) {
            ts[i] = new Thread(() -> {
                long hits = 0;
                for (int j = 0; j < iterations; j++) {
                    if (arm.get(KEYS[j % KEYS.length]) != null) hits++;
                }
                sink += hits;
            });
        }
        long s = System.nanoTime();
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        return (System.nanoTime() - s) / (double) iterations;
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 500_000;
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 10;

        Arm[] arms = { new MapOverHashMap(), new MapOverChm(),
                       new ConcurrentMapOverChm(), new ChmField() };
        for (Arm a : arms) time(a, 1, Math.min(iterations, 200_000));

        System.out.printf("%-34s %11s %11s %8s%n", "arm", "1t ns/op", threads + "t ns/op", "scale");
        for (Arm a : arms) {
            double one = time(a, 1, iterations);
            double many = time(a, threads, iterations);
            System.out.printf("%-34s %11.1f %11.1f %7.2fx%n", a.name(), one, many, (one * threads) / many);
        }
        System.out.println("sink=" + sink);
    }
}
