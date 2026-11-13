/**
 * j27 wave 2 (lane STRIPMINE): fact rows for the strip-mined counted-loop poll
 * (CRATONVM_JIT_IR_STRIP_MINE, opt-in; CRATONVM_JIT_IR_STRIP_MINE_ITER, default
 * 1000), page docs/feature-designs/jit-ir-optimizations.md.
 *
 * Pure counted loops (the shape the main/post unroller's main loop takes, and so
 * the one the strip mine takes) at trip counts that are empty, short, one strip,
 * many strips and long-running (10^8, 3 * 10^8), with starts and bounds at
 * Integer.MIN_VALUE / Integer.MAX_VALUE so the strip limit's long arithmetic and
 * its wrap cap are exercised; a loop another thread must stop at a safepoint
 * (System.gc() and Thread.getStackTrace while it runs); exceptions thrown
 * mid-loop. Every answer is Java int/long arithmetic, so the expected lines are
 * HotSpot's on any JVM.
 *
 * java J27StripMineRows               facts only (deterministic)
 * java -Dj27.time=1 J27StripMineRows  also "time <row> <ns>" lines
 */
public class J27StripMineRows {
    static final int MAX = Integer.MAX_VALUE;
    static final int MIN = Integer.MIN_VALUE;

    static int mix(int from, int to) {
        int acc = 1;
        for (int i = from; i < to; i++) acc = acc * 31 + i;
        return acc;
    }

    static int mixStep3(int from, int to) {
        int acc = 1;
        for (int i = from; i < to; i += 3) acc = acc * 31 + i;
        return acc;
    }

    static int mixStep7(int from, int to) {
        int acc = 1;
        for (int i = from; i < to; i += 7) acc = acc * 31 + i;
        return acc;
    }

    static long sumSq(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) acc += (long) i * i;
        return acc;
    }

    static int twoAcc(int n) {
        int a = 0, b = 1;
        for (int i = 0; i < n; i++) {
            a += i ^ b;
            b = b * 5 + 1;
        }
        return a * 31 + b;
    }

    static int divLoop(int n, int k) {
        int acc = 0;
        for (int i = 0; i < n; i++) acc += 1000 / (k - i);
        return acc;
    }

    static int throwAt(int n, int at) {
        int acc = 1;
        for (int i = 0; i < n; i++) {
            acc = acc * 31 + i;
            if (i == at) throw new IllegalStateException("acc " + acc + " at " + i);
        }
        return acc;
    }

    static int arraySum(int[] a, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) s += a[i];
        return s;
    }

    static long warm(int r) {
        int n = 64 + (r & 7);
        long h = mix(r, r + n);
        h = h * 31 + mixStep3(r, r + 3 * n);
        h = h * 31 + mixStep7(-r, 7 * n - r);
        h = h * 31 + sumSq(n);
        h = h * 31 + twoAcc(n);
        h = h * 31 + divLoop(n, n + 1);
        h = h * 31 + throwAt(n, n);
        return h;
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static String caught(Runnable r) {
        try {
            r.run();
            return "none";
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static volatile boolean started;
    static volatile boolean stop;

    /** A worker in a long pure loop while this thread collects and walks its stack. */
    static void safepointRow() throws InterruptedException {
        final int expected = mix(0, 200_000_000);
        final boolean[] allRight = {true};
        final int[] rounds = {0};
        Thread worker = new Thread(() -> {
            started = true;
            while (!stop) {
                if (mix(0, 200_000_000) != expected) allRight[0] = false;
                rounds[0]++;
            }
        }, "j27-strip-worker");
        worker.start();
        while (!started) Thread.onSpinWait();
        long t0 = System.nanoTime();
        for (int k = 0; k < 3; k++) System.gc();
        long t1 = System.nanoTime();
        StackTraceElement[] st = worker.getStackTrace();
        long t2 = System.nanoTime();
        stop = true;
        worker.join(120_000);
        fact("safepoint-gc-returned", true);
        fact("safepoint-stack-walked", st != null);
        fact("safepoint-worker-finished", !worker.isAlive());
        fact("safepoint-worker-answers", allRight[0] && rounds[0] > 0);
        fact("safepoint-mix-0-200000000", expected);
        if (Boolean.getBoolean("j27.time")) {
            System.out.println("time safepoint-3gc " + (t1 - t0));
            System.out.println("time safepoint-stack " + (t2 - t1));
        }
    }

    public static void main(String[] args) throws InterruptedException {
        long w = 0;
        for (int r = 0; r < 20_000; r++) w += warm(r);
        fact("warm", w);
        int[][] mixRows = {
            {0, 0}, {0, 1}, {0, 7}, {0, 1000}, {0, 1_000_000}, {-500, 500},
            {MAX - 1000, MAX}, {MIN, MIN + 1000}, {MAX - 5, MAX - 1000},
            {0, 100_000_000}, {MAX - 300_000_000, MAX},
        };
        for (int[] r : mixRows) fact("mix-" + r[0] + "-" + r[1], mix(r[0], r[1]));
        int[][] step3 = {{0, 3001}, {MAX - 999, MAX}, {MAX - 3000, MAX}, {MIN, MIN + 30_000}};
        for (int[] r : step3) fact("mixStep3-" + r[0] + "-" + r[1], mixStep3(r[0], r[1]));
        int[][] step7 = {{MAX - 700_000, MAX}, {MIN, MIN + 7}, {-7000, 7000}};
        for (int[] r : step7) fact("mixStep7-" + r[0] + "-" + r[1], mixStep7(r[0], r[1]));
        for (int n : new int[] {0, 1000, 1_000_000, 50_000_000}) fact("sumSq-" + n, sumSq(n));
        for (int n : new int[] {0, 7, 1000, 1_000_000}) fact("twoAcc-" + n, twoAcc(n));
        fact("divLoop-zero", caught(() -> divLoop(5000, 4321)));
        fact("divLoop-ok", divLoop(4000, 4321));
        fact("throwAt-2500", caught(() -> throwAt(5000, 2500)));
        fact("arraySum-oob", caught(() -> arraySum(new int[3000], 3001)));
        fact("arraySum-ok", arraySum(new int[3000], 3000));
        safepointRow();
        if (Boolean.getBoolean("j27.time")) {
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                int s = mix(0, 100_000_000);
                long t1 = System.nanoTime();
                long q = sumSq(50_000_000);
                long t2 = System.nanoTime();
                System.out.println("time mix-1e8 " + (t1 - t0) + " " + (s & 1));
                System.out.println("time sumSq-5e7 " + (t2 - t1) + " " + (q & 1));
            }
        }
    }
}
