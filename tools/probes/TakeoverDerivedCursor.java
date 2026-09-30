import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

/**
 * A frozen or blocked peer whose compiled loop keeps only a CURSOR into an
 * array must not lose the array to a collection.
 *
 * Guards gc-common w2-c / w3-g:
 *   docs/internal/gc-common-round-20260923/common-c-takeover-probe-drops-derived-pointers-FIXED-20260923.md
 *   (take-over pass: `CRATONVM_XT_TAKEOVER_INTERIOR`, default ON, `=0` kill
 *   switch; `takeover_word_companion`), and
 *   docs/internal/gc-common-round-20260923/common-w2c-helper-window-probe-misses-the-object-ending-at-a-cursor-FIXED-20260923.md
 *   (helper-window pass: `helper_window_word_probe` / `_companion`, gated by
 *   `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE`).
 *
 * SPINNERS walk a fresh byte[] in a long counted loop (the JIT may hoist the
 * base and keep only a cursor; the take-over freezes them mid-loop). PARKERS
 * walk the array, then park with the loop's cursor live across the park (a
 * blocked peer with compiled frames: a helper window), then read the array
 * again. Both verify a checksum written before the loop. CHURNERS allocate
 * garbage so pauses keep landing while they run.
 *
 * Whether the compiled code really keeps only a cursor is the JIT's choice;
 * the probe cannot force it. A run is evidence, not proof -- read it with
 * `--verbose:gc` (`xt_roots`, `hw_roots`) and, on G1, `CRATONVM_G1_DBG_PINS=1`.
 * Run on all three backends, with and without `CRATONVM_XT_TAKEOVER_INTERIOR=0`
 * and `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE=0`, and record the root / pinned
 * region deltas on the two pages above.
 *
 * Output: `spins=N parks=M bad=K` and one PROBE-OK / PROBE-FAIL line.
 * Runtime is bounded (TIME_LIMIT_MS).
 */
public class TakeoverDerivedCursor {
    static final long TIME_LIMIT_MS = 15_000;
    static final int SPINNERS = 2;
    static final int PARKERS = 2;
    static final int CHURNERS = 2;
    static final AtomicBoolean STOP = new AtomicBoolean(false);
    static final AtomicLong SPINS = new AtomicLong();
    static final AtomicLong PARKS = new AtomicLong();
    static final AtomicLong BAD = new AtomicLong();
    static volatile Object sink;

    static byte[] fresh(int n, int seed) {
        byte[] a = new byte[n];
        for (int i = 0; i < n; i++) {
            a[i] = (byte) (i * 31 + seed);
        }
        return a;
    }

    static long expected(int n, int seed) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s = s * 7 + (byte) (i * 31 + seed);
        }
        return s;
    }

    /** The loop the JIT is invited to strength-reduce to a cursor. */
    static long walk(byte[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s = s * 7 + a[i];
        }
        return s;
    }

    static void spinner(int id) {
        int seed = id * 17;
        while (!STOP.get()) {
            int n = 4096 + (seed & 1023);
            byte[] a = fresh(n, seed);
            long want = expected(n, seed);
            // Several passes per array so a pause often lands mid-walk.
            for (int rep = 0; rep < 64; rep++) {
                if (walk(a) != want) {
                    if (BAD.incrementAndGet() <= 8) {
                        System.out.println("BAD spinner " + id + " n=" + n + " rep=" + rep);
                    }
                    break;
                }
            }
            SPINS.incrementAndGet();
            seed++;
        }
    }

    static void parker(int id) {
        int seed = 1000 + id * 13;
        while (!STOP.get()) {
            int n = 2048 + (seed & 511);
            byte[] a = fresh(n, seed);
            long want = expected(n, seed);
            long first = walk(a);
            // Blocked with the compiled caller's cursor live across the park.
            LockSupport.parkNanos(200_000L);
            long second = walk(a);
            byte last = a[n - 1];
            if (first != want || second != want || last != (byte) ((n - 1) * 31 + seed)) {
                if (BAD.incrementAndGet() <= 8) {
                    System.out.println("BAD parker " + id + " n=" + n);
                }
            }
            PARKS.incrementAndGet();
            seed++;
        }
    }

    static void churner() {
        while (!STOP.get()) {
            for (int i = 0; i < 4096; i++) {
                sink = new byte[512];
            }
            sink = new Object[256];
        }
    }

    public static void main(String[] args) throws Exception {
        Thread[] ts = new Thread[SPINNERS + PARKERS + CHURNERS];
        int k = 0;
        for (int i = 0; i < SPINNERS; i++) {
            final int id = i;
            ts[k++] = new Thread(() -> spinner(id), "spinner-" + i);
        }
        for (int i = 0; i < PARKERS; i++) {
            final int id = i;
            ts[k++] = new Thread(() -> parker(id), "parker-" + i);
        }
        for (int i = 0; i < CHURNERS; i++) {
            ts[k++] = new Thread(TakeoverDerivedCursor::churner, "churner-" + i);
        }
        for (Thread t : ts) {
            t.setDaemon(true);
            t.start();
        }
        Thread.sleep(TIME_LIMIT_MS);
        STOP.set(true);
        for (Thread t : ts) {
            t.join(5_000);
        }
        long bad = BAD.get();
        System.out.println("spins=" + SPINS.get() + " parks=" + PARKS.get() + " bad=" + bad);
        System.out.println(bad == 0 && SPINS.get() > 0 && PARKS.get() > 0 ? "PROBE-OK" : "PROBE-FAIL");
    }
}
