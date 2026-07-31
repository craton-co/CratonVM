import java.nio.charset.Charset;
import java.util.HashMap;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;

/**
 * Per-call cost of a handful of natives that the charset-cache arms depend on,
 * against a trivial one (`String.length()`) that establishes the floor for
 * "call a native and come back". A body that costs many times the floor is
 * doing real work; a floor that is itself far above HotSpot's is dispatch
 * overhead, and the two need different fixes.
 *
 * Usage: NativeCallCostProbe [iterations] [threads]
 */
public final class NativeCallCostProbe {

    static final String[] NAMES =
            { "ISO-8859-1", "ISO-8859-2", "ISO-8859-3", "ISO-8859-4", "ISO-8859-5" };
    static final String[] LOWER =
            { "iso-8859-1", "iso-8859-2", "iso-8859-3", "iso-8859-4", "iso-8859-5" };

    interface Op { long run(int n); String name(); }

    static final class Length implements Op {
        public String name() { return "String.length() [floor]"; }
        public long run(int n) {
            long a = 0;
            for (int i = 0; i < n; i++) a += NAMES[i % 5].length();
            return a;
        }
    }

    static final class HashCode implements Op {
        public String name() { return "String.hashCode()"; }
        public long run(int n) {
            long a = 0;
            for (int i = 0; i < n; i++) a += NAMES[i % 5].hashCode();
            return a;
        }
    }

    static final class LowerRepeated implements Op {
        public String name() { return "toLowerCase(L) repeated receiver"; }
        public long run(int n) {
            long a = 0;
            for (int i = 0; i < n; i++) a += NAMES[i % 5].toLowerCase(Locale.ENGLISH).length();
            return a;
        }
    }

    static final class LowerFresh implements Op {
        private final String[] fresh = new String[64];
        LowerFresh() { for (int i = 0; i < fresh.length; i++) fresh[i] = "ISO-8859-" + i; }
        public String name() { return "toLowerCase(L) fresh receiver"; }
        public long run(int n) {
            long a = 0;
            for (int i = 0; i < n; i++) a += fresh[i % fresh.length].toLowerCase(Locale.ENGLISH).length();
            return a;
        }
    }

    static final class HashMapGet implements Op {
        private final Map<String,Charset> m = new HashMap<>();
        HashMapGet() { for (Charset c : Charset.availableCharsets().values()) m.put(c.name().toLowerCase(Locale.ENGLISH), c); }
        public String name() { return "Map.get -> HashMap"; }
        public long run(int n) {
            long a = 0;
            for (int i = 0; i < n; i++) if (m.get(LOWER[i % 5]) != null) a++;
            return a;
        }
    }

    static final class ChmGet implements Op {
        private final ConcurrentMap<String,Charset> m = new ConcurrentHashMap<>();
        ChmGet() { for (Charset c : Charset.availableCharsets().values()) m.put(c.name().toLowerCase(Locale.ENGLISH), c); }
        public String name() { return "ConcurrentMap.get -> CHM"; }
        public long run(int n) {
            long a = 0;
            for (int i = 0; i < n; i++) if (m.get(LOWER[i % 5]) != null) a++;
            return a;
        }
    }

    static final class ForName implements Op {
        public String name() { return "Charset.forName [control arm]"; }
        public long run(int n) {
            long a = 0;
            for (int i = 0; i < n; i++) if (Charset.forName(NAMES[i % 5]) != null) a++;
            return a;
        }
    }

    static volatile long sink;

    static double time(Op op, int threads, int iterations) throws Exception {
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) ts[i] = new Thread(() -> { sink += op.run(iterations); });
        long s = System.nanoTime();
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        return (System.nanoTime() - s) / (double) iterations;
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 500_000;
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 10;

        Op[] ops = { new Length(), new HashCode(), new LowerRepeated(), new LowerFresh(),
                     new HashMapGet(), new ChmGet(), new ForName() };
        for (Op o : ops) time(o, 1, Math.min(iterations, 200_000));

        System.out.printf("%-38s %11s %11s %8s%n", "native", "1t ns/op", threads + "t ns/op", "scale");
        for (Op o : ops) {
            double one = time(o, 1, iterations);
            double many = time(o, threads, iterations);
            System.out.printf("%-38s %11.1f %11.1f %7.2fx%n", o.name(), one, many, (one * threads) / many);
        }
        System.out.println("sink=" + sink);
    }
}
