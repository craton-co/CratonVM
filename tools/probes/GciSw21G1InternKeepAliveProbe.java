import java.util.Arrays;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

/**
 * gci i21/sw (2026-10-15): the keep-alive half of the weak String table on G1
 * ({@code gcg-g2w-proposal-weak-string-table-on-g1-and-zgc-20261001}, CratonVM switch
 * {@code CRATONVM_G1_WEAK_STRING_TABLE=1}).
 *
 * <p>Under the weak table G1's mark root scans leave the intern pool out, so a pooled String nothing
 * else references is unmarked. A mutator that fetches it from the pool ({@code intern()} hit) while
 * the concurrent mark runs and stores it into an array the marker already scanned must keep it
 * alive (HotSpot's StringTable lookup does, through its SATB keep-alive); otherwise the remark drops
 * its entry (a later {@code intern()} answers a second object: an identity mismatch) and cleanup may
 * free it under the holder (a content mismatch or a crash).
 *
 * <p>{@code keepalive [rounds] [n] [threads]} (defaults 30, 20000, 1): every thread, per round,
 * (A) interns {@code n} distinct computed Strings and drops them, (B) churns young garbage so the
 * pool-rooted Strings age through young pauses, (C) re-interns the same texts -- pool hits -- and
 * stores each answer into a long-lived array, (D) checks that {@code intern()} of every text still
 * answers the stored object and that the stored object still holds its text, then clears the array.
 * A driver thread calls {@code System.gc()} back to back for the whole run: run it with
 * {@code -XX:+ExplicitGCInvokesConcurrent} so each call is a CONCURRENT G1 cycle (on both VMs) and
 * phases C and D overlap marking.
 *
 * <p>Stdout is deterministic and identical on HotSpot 25 ({@code identity-mismatches=0
 * content-mismatches=0}). Stderr: elapsed ms and the driver's System.gc() count.
 */
public class GciSw21G1InternKeepAliveProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "keepalive";
        long t0 = System.nanoTime();
        if (!mode.equals("keepalive")) {
            throw new IllegalArgumentException(mode);
        }
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 30;
        int n = args.length > 2 ? Integer.parseInt(args[2]) : 20_000;
        int threads = args.length > 3 ? Integer.parseInt(args[3]) : 1;
        keepAlive(rounds, n, threads);
        System.err.println("elapsed-ms=" + (System.nanoTime() - t0) / 1_000_000);
        System.out.println("PROBE-OK " + mode);
    }

    static String text(int t, int r, int i) {
        return new StringBuilder("sw21-t").append(t).append("-r").append(r).append('-').append(i).toString();
    }

    static void keepAlive(int rounds, int n, int threads) throws Exception {
        AtomicBoolean stop = new AtomicBoolean();
        AtomicLong gcs = new AtomicLong();
        Thread driver = new Thread(() -> {
            while (!stop.get()) {
                System.gc();
                gcs.incrementAndGet();
            }
        }, "sw21-gc-driver");
        driver.setDaemon(true);
        driver.start();

        long[] identity = new long[threads];
        long[] content = new long[threads];
        Thread[] workers = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int tid = t;
            workers[t] = new Thread(() -> {
                long[] r = worker(tid, rounds, n);
                identity[tid] = r[0];
                content[tid] = r[1];
            }, "sw21-worker-" + t);
            workers[t].start();
        }
        for (Thread w : workers) {
            w.join();
        }
        stop.set(true);
        driver.join();

        long id = 0;
        long ct = 0;
        for (int t = 0; t < threads; t++) {
            id += identity[t];
            ct += content[t];
        }
        System.err.println("gc-driver-calls=" + gcs.get());
        System.out.println("keepalive rounds=" + rounds + " n=" + n + " threads=" + threads
                + " identity-mismatches=" + id + " content-mismatches=" + ct);
    }

    /** Returns {identity mismatches, content mismatches} of one thread. */
    static long[] worker(int t, int rounds, int n) {
        String[] holder = new String[n];
        long identity = 0;
        long content = 0;
        for (int r = 0; r < rounds; r++) {
            // (A) pooled, unreferenced.
            for (int i = 0; i < n; i++) {
                sink = text(t, r, i).intern();
            }
            sink = null;
            // (B) young garbage: the pool roots the Strings through young pauses.
            for (int k = 0; k < 2_000; k++) {
                sink = new byte[1024];
            }
            sink = null;
            // (C) pool hits stored into the long-lived holder.
            for (int i = 0; i < n; i++) {
                holder[i] = text(t, r, i).intern();
                if ((i & 63) == 0) {
                    sink = new byte[256];
                }
            }
            sink = null;
            // More garbage while the holder alone keeps the Strings.
            for (int k = 0; k < 2_000; k++) {
                sink = new byte[1024];
            }
            sink = null;
            // (D) identity and content.
            for (int i = 0; i < n; i++) {
                String expect = text(t, r, i);
                String held = holder[i];
                if (held == null || !held.equals(expect)) {
                    content++;
                } else if (expect.intern() != held) {
                    identity++;
                }
            }
            Arrays.fill(holder, null);
        }
        return new long[] {identity, content};
    }
}
