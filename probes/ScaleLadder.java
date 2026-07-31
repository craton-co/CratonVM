import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.Map;

/**
 * A ladder of increasingly VM-dependent per-iteration operations, each run at
 * 1 and N threads, so the point where multi-thread scaling collapses can be
 * attributed to one kind of operation rather than to a whole benchmark arm.
 *
 * Every rung does the same loop shape; only the body differs. `scale` is
 * aggregate throughput relative to one thread — 10.00x is perfect scaling at
 * ten threads, 1.00x means ten threads do no more total work than one.
 *
 * Usage: ScaleLadder [iterations] [threads]
 */
public final class ScaleLadder {

    interface Op {
        long run(int iterations);
        String name();
    }

    /** Pure arithmetic. No call, no allocation, no VM service. */
    static final class Arith implements Op {
        public String name() { return "arith (no call)"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { acc += i ^ (acc >>> 7); }
            return acc;
        }
    }

    static int staticSink;
    static final int STATIC_READ = 7;

    /** putstatic per iteration, no call. */
    static final class PutStatic implements Op {
        public String name() { return "putstatic (no call)"; }
        public long run(int n) {
            for (int i = 0; i < n; i++) { staticSink += i; }
            return staticSink;
        }
    }

    /** getstatic per iteration, no call. */
    static final class GetStatic implements Op {
        public String name() { return "getstatic (no call)"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { acc += STATIC_READ + i; }
            return acc;
        }
    }

    static void emptyStatic(int i) { staticSink += i; }
    static int add1(int i) { return i + 1; }

    /** A static Java call that also writes a static field. */
    static final class StaticCallPutStatic implements Op {
        public String name() { return "static call + putstatic"; }
        public long run(int n) {
            for (int i = 0; i < n; i++) { emptyStatic(i); }
            return staticSink;
        }
    }

    /** A static Java call that touches no static state. */
    static final class StaticCallPure implements Op {
        public String name() { return "static call (pure)"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { acc += add1(i); }
            return acc;
        }
    }

    interface Tiny { int id(int i); }
    static class TinyImpl implements Tiny { public int id(int i) { return i + 1; } }

    /** invokeinterface per iteration. */
    static final class IfaceCall implements Op {
        private final Tiny t = new TinyImpl();
        public String name() { return "interface call"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { acc += t.id(i); }
            return acc;
        }
    }

    /** invokevirtual per iteration, concrete receiver type. */
    static final class VirtualCall implements Op {
        private final TinyImpl t = new TinyImpl();
        public String name() { return "virtual call"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { acc += t.id(i); }
            return acc;
        }
    }

    /** A primitive-returning native. */
    static final class NativePrim implements Op {
        private final Object o = new Object();
        public String name() { return "native, primitive return"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { acc += System.identityHashCode(o); }
            return acc;
        }
    }

    /** An object-returning native: HashMap.get on a warm one-entry map. */
    static final class NativeObj implements Op {
        private final Map<String,String> m = new HashMap<>();
        private final String k = "k";
        NativeObj() { m.put(k, "v"); }
        public String name() { return "native obj return (HashMap.get)"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { if (m.get(k) != null) acc++; }
            return acc;
        }
    }

    /** An object-returning native with a different backing native. */
    static final class NativeObj2 implements Op {
        private final Map<Object,Object> m = new IdentityHashMap<>();
        private final Object k = new Object();
        NativeObj2() { m.put(k, "v"); }
        public String name() { return "native obj return (IdentityHashMap.get)"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { if (m.get(k) != null) acc++; }
            return acc;
        }
    }

    /** Allocation only. */
    static final class Alloc implements Op {
        public String name() { return "allocation (new int[4])"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { int[] a = new int[4]; a[0] = i; acc += a[0]; }
            return acc;
        }
    }

    /** Allocation of a small object rather than an array. */
    static final class AllocObj implements Op {
        public String name() { return "allocation (new TinyImpl)"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { Tiny t = new TinyImpl(); acc += t.id(i); }
            return acc;
        }
    }

    /** An uncontended monitor: each thread locks its own object. */
    static final class Monitor implements Op {
        private final Object lock = new Object();
        public String name() { return "uncontended synchronized block"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { synchronized (lock) { acc += i; } }
            return acc;
        }
    }

    /** String.toLowerCase(Locale) — the operation the charset-cache arms use. */
    static final class Lower implements Op {
        private final String s = "ISO-8859-1";
        public String name() { return "String.toLowerCase(Locale.ENGLISH)"; }
        public long run(int n) {
            long acc = 0;
            for (int i = 0; i < n; i++) { acc += s.toLowerCase(java.util.Locale.ENGLISH).length(); }
            return acc;
        }
    }

    static volatile long sink;

    static double time(Op op, int threads, int iterations) throws Exception {
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) {
            ts[i] = new Thread(() -> { sink += op.run(iterations); });
        }
        long s = System.nanoTime();
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        return (System.nanoTime() - s) / (double) iterations;
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 10;

        Op[] ops = { new Arith(), new GetStatic(), new PutStatic(),
                     new StaticCallPure(), new StaticCallPutStatic(),
                     new VirtualCall(), new IfaceCall(),
                     new NativePrim(), new NativeObj(), new NativeObj2(),
                     new Alloc(), new AllocObj(), new Monitor(), new Lower() };

        for (Op op : ops) { time(op, 1, Math.min(iterations, 200_000)); }   // warm

        System.out.printf("%-40s %11s %11s %8s%n",
                "op", "1t ns/op", threads + "t ns/op", "scale");
        for (Op op : ops) {
            double one = time(op, 1, iterations);
            double many = time(op, threads, iterations);
            System.out.printf("%-40s %11.1f %11.1f %7.2fx%n",
                    op.name(), one, many, (one * threads) / many);
        }
        System.out.println("sink=" + sink);
    }
}
