// JIT round 21 wave 7, lane codecache: RetainedCode handle churn with threads busy in compiled code.
//
// Review rev-w45-jit item 1 asked whether CODECACHE21-2 (CRATONVM_JIT_RETAINED_HOLDER_COUNT, default
// ON) sends every per-thread invoke-cache eviction of a still-published body through the retirement
// queue (generation bump, queue lock, full drain). This probe makes that eviction rate high:
// each thread drives ONE virtual call site over 12 receiver classes in rotation (`Shape.area`), so
// the site's per-thread invoke-cache entry is replaced on nearly every call, while 0 or 4 extra
// threads spin inside compiled code (so a queued entry cannot be released at once). Tier-ups during
// warm-up supersede bodies whose last handles DO take the queue (the case CODECACHE21-2 is for);
// the measured reps run on settled bodies.
//
// Rows (timing lines `time <row> <N> ns/op`, median of 9 reps after 3 warm-up reps; ns per call on
// the measuring thread):
//   time churn-solo       one thread, no spinners
//   time churn-busy4      one thread, four spinners in compiled code
//   time churn-4t-busy4   four churning threads at once (per-thread ns/op), four spinners
// Fact line (deterministic, diff against HotSpot):
//   fact retained-churn ok=true sum=<S>
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21RetainedChurn.java):
//   P=tools/probes/out
//   cratonvm -Xmx256m -cp $P R21RetainedChurn
//   CRATONVM_JIT_RETAINED_HOLDER_COUNT=0 cratonvm -Xmx256m -cp $P R21RetainedChurn   # A/B arm
//   cratonvm --compatible -Xmx256m -cp $P R21RetainedChurn
//   cratonvm --nojit -Xmx256m -cp $P R21RetainedChurn                                 # same fact line
//   java -Xmx256m -cp $P R21RetainedChurn                                             # HotSpot 25
// Interleave the two arms and take medians (in-JVM timings swing ~3x on this host; pin to P-cores
// with `start /affinity FFFF` on Windows). CRATONVM_DBG_JIT_METHOD_STATS=1 prints the code-cache
// report at exit; compare its deferred-bodies / deferred-bytes between the arms.
//
// Expectation: the fact line is identical everywhere. The lane's analysis (wave 7) predicts the
// churn rows equal within noise between the arms: a published body's handle is never the last
// holder (the cache's shard-map entry is a counted RetainedCode), so it drops plainly either way.
// A clear default-ON loss on churn-busy4 / churn-4t-busy4 would contradict that analysis.

public class R21RetainedChurn {
    interface Shape { int area(int x); }
    static final class S0 implements Shape { public int area(int x) { return x + 0; } }
    static final class S1 implements Shape { public int area(int x) { return x + 1; } }
    static final class S2 implements Shape { public int area(int x) { return x + 2; } }
    static final class S3 implements Shape { public int area(int x) { return x + 3; } }
    static final class S4 implements Shape { public int area(int x) { return x + 4; } }
    static final class S5 implements Shape { public int area(int x) { return x + 5; } }
    static final class S6 implements Shape { public int area(int x) { return x + 6; } }
    static final class S7 implements Shape { public int area(int x) { return x + 7; } }
    static final class S8 implements Shape { public int area(int x) { return x + 8; } }
    static final class S9 implements Shape { public int area(int x) { return x + 9; } }
    static final class S10 implements Shape { public int area(int x) { return x + 10; } }
    static final class S11 implements Shape { public int area(int x) { return x + 11; } }

    static final Shape[] SHAPES = {
        new S0(), new S1(), new S2(), new S3(), new S4(), new S5(),
        new S6(), new S7(), new S8(), new S9(), new S10(), new S11(),
    };

    static final int OPS = 200_000;
    static final int WARM = 3;
    static final int REPS = 9;

    static volatile boolean spin;
    static volatile long sink;

    // The churning site: one invokeinterface over 12 receivers in rotation.
    static long churn(int ops, int seed) {
        long sum = 0;
        Shape[] shapes = SHAPES;
        for (int i = 0; i < ops; i++) {
            sum += shapes[(i + seed) % shapes.length].area(i & 1023);
        }
        return sum;
    }

    // Plain arithmetic for churn(ops, seed).
    static long expected(int ops, int seed) {
        long sum = 0;
        for (int i = 0; i < ops; i++) {
            sum += (i & 1023) + ((i + seed) % SHAPES.length);
        }
        return sum;
    }

    static long spinBody(long acc) {
        for (int i = 0; i < 4096; i++) {
            acc = acc * 6364136223846793005L + 1442695040888963407L;
        }
        return acc;
    }

    static Thread[] startSpinners(int n) {
        spin = true;
        Thread[] ts = new Thread[n];
        for (int t = 0; t < n; t++) {
            final long seed = t + 1;
            ts[t] = new Thread(() -> {
                long acc = seed;
                while (spin) {
                    acc = spinBody(acc);
                }
                sink = acc;
            }, "spinner-" + t);
            ts[t].setDaemon(true);
            ts[t].start();
        }
        return ts;
    }

    static void stopSpinners(Thread[] ts) throws InterruptedException {
        spin = false;
        for (Thread t : ts) t.join();
    }

    static boolean ok = true;
    static long total;

    static long oneRep(int threads) throws InterruptedException {
        if (threads == 1) {
            long t0 = System.nanoTime();
            long s = churn(OPS, 0);
            long dt = System.nanoTime() - t0;
            if (s != expected(OPS, 0)) ok = false;
            total += s;
            return dt;
        }
        final long[] sums = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int seed = t;
            ts[t] = new Thread(() -> sums[seed] = churn(OPS, seed), "churn-" + t);
        }
        long t0 = System.nanoTime();
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        long dt = System.nanoTime() - t0;
        for (int t = 0; t < threads; t++) {
            if (sums[t] != expected(OPS, t)) ok = false;
            total += sums[t];
        }
        return dt;
    }

    static void row(String name, int threads, int spinners) throws InterruptedException {
        Thread[] sp = startSpinners(spinners);
        try {
            for (int i = 0; i < WARM; i++) oneRep(threads);
            long[] t = new long[REPS];
            for (int i = 0; i < REPS; i++) t[i] = oneRep(threads);
            java.util.Arrays.sort(t);
            System.out.println("time " + name + " " + (t[REPS / 2] / OPS) + " ns/op");
        } finally {
            stopSpinners(sp);
        }
    }

    public static void main(String[] args) throws Exception {
        row("churn-solo", 1, 0);
        row("churn-busy4", 1, 4);
        row("churn-4t-busy4", 4, 4);
        // Deterministic: (WARM + REPS) reps per row, the rows' seeds fixed.
        long want = 0;
        int reps = WARM + REPS;
        want += reps * expected(OPS, 0);          // churn-solo
        want += reps * expected(OPS, 0);          // churn-busy4
        for (int t = 0; t < 4; t++) want += reps * expected(OPS, t); // churn-4t-busy4
        if (total != want) ok = false;
        System.out.println("fact retained-churn ok=" + ok + " sum=" + want);
    }
}
