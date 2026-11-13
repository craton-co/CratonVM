/**
 * j27 wave 4 (lane STRIPOSR): fact rows for the strip mine's array bodies and
 * long inductions (CRATONVM_JIT_IR_STRIP_MINE=1 with the opt-in sub-switches
 * CRATONVM_JIT_IR_STRIP_MINE_ARRAYS=1 and CRATONVM_JIT_IR_STRIP_MINE_LONG=1),
 * page docs/feature-designs/jit-osr-exit-and-recompile.md.
 *
 * Counted loops over int[] / long[] / byte[] whose element reads the range
 * check proves (i < a.length, a[i]; i < a.length - 1, a[i + 1]), reads it must
 * keep (a bound that is not the length: the AIOOBE must still be thrown at the
 * same index, with HotSpot's message), loops with a long induction across
 * 2^31 and at Long.MIN_VALUE / Long.MAX_VALUE, and a worker in a long array
 * loop another thread must stop at a safepoint (System.gc() and
 * Thread.getStackTrace while it runs). Every answer is Java int/long
 * arithmetic, so the expected lines are HotSpot's on any JVM.
 *
 * j27 wave 5 (lane STRIPOSR2, CRATONVM_JIT_IR_STRIP_MINE_OSR=1): the "osr-*"
 * rows run FIRST, each a cold method called once with a long loop, so each
 * loop is entered through the OSR door (static-cell shapes counterInt /
 * twoCells, a locals-only loop, lim = Integer.MAX_VALUE, stride 3, and a
 * worker in OSR-entered loops while this thread collects and walks its stack).
 *
 * java J27StripMine2Rows               facts only (deterministic)
 * java -Dj27.time=1 J27StripMine2Rows  also "time <row> <ns>" lines
 */
public class J27StripMine2Rows {
    static final long IMAX = Integer.MAX_VALUE;
    static final long LMAX = Long.MAX_VALUE;
    static final long LMIN = Long.MIN_VALUE;

    static int[] ints(int n) {
        int[] a = new int[n];
        for (int j = 0; j < n; j++) a[j] = j * 7 + 1;
        return a;
    }

    static long[] longs(int n) {
        long[] a = new long[n];
        for (int j = 0; j < n; j++) a[j] = (long) j * 0x9E3779B97F4A7C15L;
        return a;
    }

    static byte[] bytes(int n) {
        byte[] a = new byte[n];
        for (int j = 0; j < n; j++) a[j] = (byte) (j * 13);
        return a;
    }

    static int hashArr(int[] a) {
        int s = 1;
        for (int i = 0; i < a.length; i++) s = s * 31 + a[i];
        return s;
    }

    static int hashArrN(int[] a, int n) {
        int s = 1;
        for (int i = 0; i < n; i++) s = s * 31 + a[i];
        return s;
    }

    static long hashLongs(long[] a) {
        long s = 1;
        for (int i = 0; i < a.length; i++) s = s * 31 + a[i];
        return s;
    }

    static int hashBytes(byte[] a) {
        int s = 1;
        for (int i = 0; i < a.length; i++) s = s * 31 + a[i];
        return s;
    }

    static int pairs(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length - 1; i++) s += a[i] * a[i + 1];
        return s;
    }

    static int dot(int[] a, int[] b) {
        int s = 0;
        for (int i = 0; i < a.length; i++) s += a[i] * b[i];
        return s;
    }

    static int lmix(long from, long to) {
        int acc = 1;
        for (long i = from; i < to; i++) acc = acc * 31 + (int) i;
        return acc;
    }

    static int lmixStep3(long from, long to) {
        int acc = 1;
        for (long i = from; i < to; i += 3) acc = acc * 31 + (int) (i ^ (i >>> 32));
        return acc;
    }

    static long lsumSq(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += i * i;
        return acc;
    }

    // ── j27 wave 5 (lane STRIPOSR2, item 1, CRATONVM_JIT_IR_STRIP_MINE_OSR=1):
    // each method below is called ONCE, cold, with a long loop, so the run
    // enters its loop through the OSR door (never warmed by warm()).

    static int counter;
    static int other;

    /** The parent page's counterInt shape: a static cell (promoted; its write-back follows the outer header). */
    static int osrCounterInt(int n) {
        counter = 0;
        for (int i = 0; i < n; i++) counter += i;
        return counter;
    }

    /** The parent page's twoCells shape: two static cells. */
    static int osrTwoCells(int n) {
        counter = 0;
        other = 0;
        for (int i = 0; i < n; i++) {
            counter += 3;
            other ^= i;
        }
        return counter * 7 + other;
    }

    /** The same loop over locals only: the OSR form's pure body. */
    static int osrLocalMix(int n) {
        int a = 1;
        for (int i = 0; i < n; i++) a = a * 31 + i;
        return a;
    }

    /** lim = Integer.MAX_VALUE: the last trip is at i = MAX - 1, no wrap. */
    static int osrMixTop(int from) {
        int a = 1;
        for (int i = from; i < Integer.MAX_VALUE; i++) a = a * 31 + i;
        return a;
    }

    /** A stride that does not divide the strip. */
    static int osrMixStep3(int n) {
        int a = 1;
        for (int i = 0; i < n; i += 3) a = a * 31 + i;
        return a;
    }

    /** The safepoint worker's body: one OSR entry, then compiled calls. */
    static int osrWorkerMix(int n) {
        int a = 1;
        for (int i = 0; i < n; i++) a = a * 31 + i;
        return a;
    }

    /** A worker inside OSR-entered pure loops while this thread collects and walks its stack. */
    static void osrSafepointRow() throws InterruptedException {
        final int expected = -745145535; // osrWorkerMix(10_000_000), Java int arithmetic
        final boolean[] allRight = {true};
        final int[] rounds = {0};
        osrStarted = false;
        osrStop = false;
        Thread worker = new Thread(() -> {
            osrStarted = true;
            while (!osrStop) {
                if (osrWorkerMix(10_000_000) != expected) allRight[0] = false;
                rounds[0]++;
            }
        }, "j27-striposr2-worker");
        worker.start();
        while (!osrStarted) Thread.onSpinWait();
        for (int k = 0; k < 3; k++) System.gc();
        StackTraceElement[] st = worker.getStackTrace();
        osrStop = true;
        worker.join(120_000);
        fact("osr-safepoint-gc-returned", true);
        fact("osr-safepoint-stack-walked", st != null);
        fact("osr-safepoint-worker-finished", !worker.isAlive());
        fact("osr-safepoint-worker-answers", allRight[0] && rounds[0] > 0);
    }

    static volatile boolean osrStarted;
    static volatile boolean osrStop;

    static void osrRows() throws InterruptedException {
        fact("osr-counterInt-1000000", osrCounterInt(1_000_000));
        fact("osr-twoCells-1000000", osrTwoCells(1_000_000));
        fact("osr-localMix-3000000", osrLocalMix(3_000_000));
        int top = Integer.MAX_VALUE - 3_000_000;
        fact("osr-mixTop-" + top, osrMixTop(top));
        fact("osr-mixStep3-3000001", osrMixStep3(3_000_001));
        osrSafepointRow();
    }

    static final int[] W_INT = ints(80);
    static final long[] W_LONG = longs(80);
    static final byte[] W_BYTE = bytes(80);

    static long warm(int r) {
        int n = 64 + (r & 7);
        long h = hashArrN(W_INT, n);
        h = h * 31 + hashArr(W_INT);
        h = h * 31 + pairs(W_INT);
        h = h * 31 + hashLongs(W_LONG);
        h = h * 31 + hashBytes(W_BYTE);
        h = h * 31 + dot(W_INT, W_INT);
        h = h * 31 + lmix(r, r + n);
        h = h * 31 + lmixStep3(-r, 3L * n - r);
        h = h * 31 + lsumSq(n);
        return h;
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static String caught(java.util.function.IntSupplier r) {
        try {
            return "none " + r.getAsInt();
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static volatile boolean started;
    static volatile boolean stop;

    /** A worker in a long array loop while this thread collects and walks its stack. */
    static void safepointRow() throws InterruptedException {
        final int[] big = ints(4_000_000);
        final int expected = hashArr(big);
        final boolean[] allRight = {true};
        final int[] rounds = {0};
        Thread worker = new Thread(() -> {
            started = true;
            while (!stop) {
                if (hashArr(big) != expected) allRight[0] = false;
                rounds[0]++;
            }
        }, "j27-strip2-worker");
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
        fact("safepoint-hashArr-4000000", expected);
        if (Boolean.getBoolean("j27.time")) {
            System.out.println("time safepoint-3gc " + (t1 - t0));
            System.out.println("time safepoint-stack " + (t2 - t1));
        }
    }

    public static void main(String[] args) throws InterruptedException {
        // First, while every OSR row's method is still cold.
        osrRows();
        long w = 0;
        for (int r = 0; r < 20_000; r++) w += warm(r);
        fact("warm", w);
        for (int n : new int[] {0, 1, 3, 4, 5, 1000, 1_000_003}) fact("hashArr-" + n, hashArr(ints(n)));
        final int[] a3000 = ints(3000);
        fact("hashArrN-ok", caught(() -> hashArrN(a3000, 3000)));
        fact("hashArrN-oob", caught(() -> hashArrN(a3000, 3001)));
        fact("hashArr-null", caught(() -> hashArr(null)).startsWith("java.lang.NullPointerException"));
        for (int n : new int[] {0, 5, 1000, 100_003}) fact("hashLongs-" + n, hashLongs(longs(n)));
        for (int n : new int[] {0, 5, 1000, 100_003}) fact("hashBytes-" + n, hashBytes(bytes(n)));
        for (int n : new int[] {0, 1, 2, 5, 1000, 100_003}) fact("pairs-" + n, pairs(ints(n)));
        final int[] a1000 = ints(1000);
        final int[] a999 = ints(999);
        fact("dot-ok", caught(() -> dot(a1000, a1000)));
        fact("dot-oob", caught(() -> dot(a1000, a999)));
        long[][] lmixRows = {
            {0, 0}, {0, 1}, {0, 1000}, {IMAX - 100, IMAX + 5000}, {-(1L << 40), -(1L << 40) + 100_000},
            {LMAX - 1000, LMAX}, {LMIN, LMIN + 1000}, {LMAX, LMAX}, {5, 3}, {0, 3_000_000},
        };
        for (long[] r : lmixRows) fact("lmix-" + r[0] + "-" + r[1], lmix(r[0], r[1]));
        long[][] step3 = {{0, 3001}, {LMAX - 999, LMAX}, {LMAX - 3000, LMAX}, {LMIN, LMIN + 30_000}, {IMAX - 300, IMAX + 3000}};
        for (long[] r : step3) fact("lmixStep3-" + r[0] + "-" + r[1], lmixStep3(r[0], r[1]));
        for (long n : new long[] {0, 1000, 1_000_000, 3_000_000}) fact("lsumSq-" + n, lsumSq(n));
        safepointRow();
        if (Boolean.getBoolean("j27.time")) {
            int[] big = ints(10_000_000);
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                int s = hashArr(big);
                long t1 = System.nanoTime();
                int l = lmix(0, 100_000_000L);
                long t2 = System.nanoTime();
                System.out.println("time hashArr-1e7 " + (t1 - t0) + " " + (s & 1));
                System.out.println("time lmix-1e8 " + (t2 - t1) + " " + (l & 1));
            }
        }
    }
}
