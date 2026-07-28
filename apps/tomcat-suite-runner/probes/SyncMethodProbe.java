/*
 * SyncMethodProbe - what does ACC_SYNCHRONIZED cost a method on this VM?
 *
 * This compares the implicit `ACC_SYNCHRONIZED` form with an explicit
 * `synchronized` block. Both forms are eligible for JIT compilation; the
 * implicit form acquires its method monitor in the JIT entry wrapper.
 *
 * `plain` and `sync` below are byte-identical apart from the modifier, and the
 * monitor is always uncontended (single thread), so the ratio isolates the
 * remaining cost of the monitor implementation.
 *
 * `syncBlock` uses a `synchronized (this) { ... }` BLOCK instead: that is an
 * ordinary monitorenter/monitorexit pair in the body, which the backend does
 * lower, so it isolates the modifier from the monitor.
 *
 * Usage:  cratonvm -cp <dir> SyncMethodProbe [iters]
 */
public class SyncMethodProbe {

    private int state;

    int plain(int a, int b) {
        int acc = state;
        for (int i = 0; i < 16; i++) {
            acc += (a ^ (b + i)) * 3;
            acc ^= acc >>> 7;
        }
        state = acc;
        return acc;
    }

    synchronized int sync(int a, int b) {
        int acc = state;
        for (int i = 0; i < 16; i++) {
            acc += (a ^ (b + i)) * 3;
            acc ^= acc >>> 7;
        }
        state = acc;
        return acc;
    }

    int syncBlock(int a, int b) {
        synchronized (this) {
            int acc = state;
            for (int i = 0; i < 16; i++) {
                acc += (a ^ (b + i)) * 3;
                acc ^= acc >>> 7;
            }
            state = acc;
            return acc;
        }
    }

    static int sink;

    static double timePlain(SyncMethodProbe p, int iters) {
        int s = 0; long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) { s += p.plain(i, i + 1); }
        long dt = System.nanoTime() - t0; sink += s; return (double) dt / iters;
    }
    static double timeSync(SyncMethodProbe p, int iters) {
        int s = 0; long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) { s += p.sync(i, i + 1); }
        long dt = System.nanoTime() - t0; sink += s; return (double) dt / iters;
    }
    static double timeSyncBlock(SyncMethodProbe p, int iters) {
        int s = 0; long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) { s += p.syncBlock(i, i + 1); }
        long dt = System.nanoTime() - t0; sink += s; return (double) dt / iters;
    }

    // A StringBuffer (every method synchronized) vs the identical StringBuilder.
    static double timeStringBuffer(int iters) {
        int s = 0; long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            StringBuffer sb = new StringBuffer();
            sb.append("a").append(i).append('-').append(i * 2L);
            s += sb.length();
        }
        long dt = System.nanoTime() - t0; sink += s; return (double) dt / iters;
    }
    static double timeStringBuilder(int iters) {
        int s = 0; long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            StringBuilder sb = new StringBuilder();
            sb.append("a").append(i).append('-').append(i * 2L);
            s += sb.length();
        }
        long dt = System.nanoTime() - t0; sink += s; return (double) dt / iters;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        SyncMethodProbe p = new SyncMethodProbe();
        for (int round = 1; round <= 3; round++) {
            double a = timePlain(p, iters);
            double b = timeSync(p, iters);
            double c = timeSyncBlock(p, iters);
            double d = timeStringBuilder(iters);
            double e = timeStringBuffer(iters);
            System.out.println(String.format(
                "round %d  plain=%.0f sync=%.0f (%.1fx) syncBlock=%.0f (%.1fx) | StringBuilder=%.0f StringBuffer=%.0f (%.1fx)  ns/op",
                round, a, b, b / a, c, c / a, d, e, e / d));
        }
        System.out.println("sink=" + sink);
    }
}
