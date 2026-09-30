import java.io.IOException;
import java.io.InputStream;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.Random;

/**
 * `BlobStreamCost`, timed on the CPU clock instead of the wall clock.
 *
 * <h2>Why this exists</h2>
 *
 * The original probe times with {@code System.nanoTime()}. On a shared host
 * that measures how busy the box was, not how expensive the code is: two runs
 * of the identical binary, minutes apart, on 2026-09-02 gave
 *
 * <pre>
 *   boxed Long counter        218.7  ->  346.1 ns/op   (+58%)
 *   stream boxed+new Random  2439.5 -> 4172.4 ns/op   (+71%)
 * </pre>
 *
 * while a `Get-Counter` sample showed six spinning shells, another session's
 * `cratonvm` and a `rustc` sharing the machine. A decomposition built out of
 * numbers with that spread cannot rank its own components, and the page this
 * probe serves ranks five of them.
 *
 * {@link ThreadMXBean#getCurrentThreadCpuTime()} counts only the time this
 * thread was ON a CPU, so a descheduled slice does not inflate it. It is
 * quantised to the Windows scheduler tick (15.625 ms, visible directly in
 * `probes/CpuClockCheck.java`), which over an arm of a few seconds is well
 * under 1% — and every arm here is sized to run for seconds.
 *
 * <h2>What it does NOT fix</h2>
 *
 * CPU time still absorbs contention that slows the thread WHILE it runs: a
 * co-resident process evicting cache lines, or an SMT sibling. So this is a
 * better instrument, not an isolated one. That is what the `wall/cpu` column
 * is for: it is the deschedule ratio, and a run where it is far from 1.0 is a
 * run whose absolute numbers still deserve suspicion even here. Quote the
 * ratio beside the number, the way `/proc/loadavg` is quoted beside an `rc`.
 *
 * <p>Arms are identical to {@code BlobStreamCost} so the two are comparable
 * line for line.
 */
public class BlobStreamCostCpu {

    static final int ITERS = Integer.getInteger("iters", 300_000);
    static final int REPS = Integer.getInteger("reps", 3);
    static long sink;
    static final ThreadMXBean TB = ManagementFactory.getThreadMXBean();

    interface Arm { long run(int n) throws IOException; }

    /**
     * The CPU clock's quantum, measured rather than assumed.
     *
     * Windows reports thread CPU time in scheduler ticks — 15.625 ms — so an
     * arm that finishes inside one tick reads as `0.0 ns/op`, which is not a
     * fast measurement but no measurement at all. The first version of this
     * probe printed exactly that for `primitive long counter`, beside a
     * `wall/cpu` of `Infinity` that gave it away.
     *
     * `time` therefore scales each arm's iteration count until it spans at
     * least {@link #MIN_TICKS} quanta, so the quantisation error is bounded by
     * `1/MIN_TICKS` regardless of how cheap the arm turns out to be. The
     * scaled count is printed, because an arm that needed 64x the iterations
     * is telling you something about itself.
     */
    static final long QUANTUM_NS = 15_625_000L;
    static final int MIN_TICKS = Integer.getInteger("minticks", 40);

    static void time(String name, Arm arm) throws IOException {
        arm.run(Math.min(ITERS / 10, 100_000));
        // Grow n until one repetition spans MIN_TICKS quanta. Capped so a
        // pathologically cheap arm cannot run for ever; the cap is reported by
        // the `ticks` column falling below MIN_TICKS.
        int n = ITERS;
        long probe;
        while (true) {
            long c0 = TB.getCurrentThreadCpuTime();
            sink += arm.run(n);
            probe = TB.getCurrentThreadCpuTime() - c0;
            if (probe >= QUANTUM_NS * MIN_TICKS || n >= (1 << 30) / 2) break;
            long want = probe == 0 ? 16 : (QUANTUM_NS * MIN_TICKS * 2) / Math.max(probe, 1);
            n = (int) Math.min((long) n * Math.max(2, Math.min(want, 64)), (1 << 30));
        }
        long bestCpu = Long.MAX_VALUE, wallAtBest = 0;
        for (int r = 0; r < REPS; r++) {
            long c0 = TB.getCurrentThreadCpuTime();
            long w0 = System.nanoTime();
            sink += arm.run(n);
            long w1 = System.nanoTime();
            long dc = TB.getCurrentThreadCpuTime() - c0;
            // Minimum on the CPU clock, and report the WALL of that same
            // repetition — not the minimum wall of any repetition, which would
            // pair two different runs and make the ratio meaningless.
            if (dc < bestCpu) { bestCpu = dc; wallAtBest = w1 - w0; }
        }
        System.out.printf("CK %-30s %9.2f ns/op cpu   (wall/cpu %.2f, n=%d, ticks=%d)%n",
                name, (double) bestCpu / n, (double) wallAtBest / bestCpu,
                n, bestCpu / QUANTUM_NS);
    }

    // --- the constructor split ---------------------------------------
    static long armNewRandomSeeded(int n) {          // no entropy draw
        long a = 0;
        for (int i = 0; i < n; i++) a += new Random(i).nextInt();
        return a;
    }
    static long armNewRandomNoSeed(int n) {          // entropy draw per ctor
        long a = 0;
        for (int i = 0; i < n; i++) a += new Random().nextInt();
        return a;
    }
    /** `nextInt` alone, on a shared generator — isolates the call from the ctor. */
    static final Random SHARED = new Random(7);
    static long armSharedNextInt(int n) {
        long a = 0;
        for (int i = 0; i < n; i++) a += SHARED.nextInt();
        return a;
    }

    // --- the counter split -------------------------------------------
    static long armBoxedCounter(int n) {
        Long count = (long) n;
        long a = 0;
        for (int i = 0; i < n; i++) { if (count > 0) { count--; a++; } }
        return a;
    }
    static long armPrimCounter(int n) {
        long count = n;
        long a = 0;
        for (int i = 0; i < n; i++) { if (count > 0) { count--; a++; } }
        return a;
    }
    /** The `Integer` twin of `armBoxedCounter`, for the claim that this is not a `Long` story. */
    static long armBoxedIntCounter(int n) {
        Integer count = n;
        long a = 0;
        for (int i = 0; i < n; i++) { if (count > 0) { count--; a++; } }
        return a;
    }

    // --- the two streams, identical but for the counter's type -------
    static final class BoxedStream extends InputStream {
        private boolean read = false;
        private Long count;
        BoxedStream(long n) { this.count = n; }
        @Override public int read() {
            read = true;
            if (count > 0) { count--; return new Random().nextInt(); }
            return -1;
        }
        boolean wasRead() { return read; }
    }
    static final class PrimStream extends InputStream {
        private boolean read = false;
        private long count;
        PrimStream(long n) { this.count = n; }
        @Override public int read() {
            read = true;
            if (count > 0) { count--; return new Random().nextInt(); }
            return -1;
        }
        boolean wasRead() { return read; }
    }
    /** Same shape again, with the Random hoisted out — isolates dispatch+counter. */
    static final class PrimSharedRandomStream extends InputStream {
        private boolean read = false;
        private long count;
        private final Random rnd = new Random(7);
        PrimSharedRandomStream(long n) { this.count = n; }
        @Override public int read() {
            read = true;
            if (count > 0) { count--; return rnd.nextInt(); }
            return -1;
        }
        boolean wasRead() { return read; }
    }

    /**
     * The boxed stream with the Random REMOVED — same virtual dispatch, same
     * boxed counter, a constant payload.
     *
     * Exists because `stream boxed+new Random` cannot answer a question about
     * BOXING: `new Random()` is ~1600 of its ~4300 ns and carries essentially
     * all of its variance (that arm spreads 25% within a single binary and a
     * single switch setting), so an A/B of a boxing change reads mostly as
     * Random noise. This arm is the same shape with that term deleted.
     */
    static final class BoxedNoRandomStream extends InputStream {
        private boolean read = false;
        private Long count;
        BoxedNoRandomStream(long n) { this.count = n; }
        @Override public int read() {
            read = true;
            if (count > 0) { count--; return 7; }
            return -1;
        }
        boolean wasRead() { return read; }
    }
    static final class PrimNoRandomStream extends InputStream {
        private boolean read = false;
        private long count;
        PrimNoRandomStream(long n) { this.count = n; }
        @Override public int read() {
            read = true;
            if (count > 0) { count--; return 7; }
            return -1;
        }
        boolean wasRead() { return read; }
    }
    static long armBoxedNoRandomStream(int n) throws IOException {
        BoxedNoRandomStream in = new BoxedNoRandomStream(n);
        long a = 0;
        for (int i = 0; i < n; i++) a += in.read();
        return a + (in.wasRead() ? 1 : 0);
    }
    static long armPrimNoRandomStream(int n) throws IOException {
        PrimNoRandomStream in = new PrimNoRandomStream(n);
        long a = 0;
        for (int i = 0; i < n; i++) a += in.read();
        return a + (in.wasRead() ? 1 : 0);
    }

    static long armBoxedStream(int n) throws IOException {
        BoxedStream in = new BoxedStream(n);
        long a = 0;
        for (int i = 0; i < n; i++) a += in.read();
        return a + (in.wasRead() ? 1 : 0);
    }
    static long armPrimStream(int n) throws IOException {
        PrimStream in = new PrimStream(n);
        long a = 0;
        for (int i = 0; i < n; i++) a += in.read();
        return a + (in.wasRead() ? 1 : 0);
    }
    static long armPrimSharedStream(int n) throws IOException {
        PrimSharedRandomStream in = new PrimSharedRandomStream(n);
        long a = 0;
        for (int i = 0; i < n; i++) a += in.read();
        return a + (in.wasRead() ? 1 : 0);
    }

    public static void main(String[] args) throws Exception {
        if (!TB.isCurrentThreadCpuTimeSupported()) {
            // Refuse rather than silently fall back to the wall clock: a run
            // that quietly measured the wrong thing is the failure this probe
            // exists to remove.
            System.out.println("CK REFUSING: thread CPU time unsupported on this VM");
            System.exit(2);
        }
        TB.setThreadCpuTimeEnabled(true);
        System.out.println("CK BlobStreamCostCpu iters=" + ITERS + " reps=" + REPS);
        time("new Random(seed).nextInt", BlobStreamCostCpu::armNewRandomSeeded);
        time("new Random().nextInt", BlobStreamCostCpu::armNewRandomNoSeed);
        time("shared Random.nextInt", BlobStreamCostCpu::armSharedNextInt);
        time("boxed Long counter", BlobStreamCostCpu::armBoxedCounter);
        time("boxed Integer counter", BlobStreamCostCpu::armBoxedIntCounter);
        time("primitive long counter", BlobStreamCostCpu::armPrimCounter);
        time("stream boxed  no Random", BlobStreamCostCpu::armBoxedNoRandomStream);
        time("stream prim   no Random", BlobStreamCostCpu::armPrimNoRandomStream);
        time("stream boxed+new Random", BlobStreamCostCpu::armBoxedStream);
        time("stream prim +new Random", BlobStreamCostCpu::armPrimStream);
        time("stream prim +shared Random", BlobStreamCostCpu::armPrimSharedStream);
        System.out.println("CK BlobStreamCostCpu sink=" + (sink == 0 ? 0 : 1));
    }
}
