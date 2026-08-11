/** Per-call cost of an UNCONTENDED monitor, against the same call without one. */
public class SyncCostProbe {
    static long sink;

    static class Target {
        int v;
        int plain() { return ++v; }
        synchronized int sync() { return ++v; }
    }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        Target t = new Target();
        Object lock = new Object();
        for (int round = 0; round < 4; round++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) { sink += t.plain(); }
            long plain = System.nanoTime() - t0;

            t0 = System.nanoTime();
            for (int i = 0; i < n; i++) { sink += t.sync(); }
            long sync = System.nanoTime() - t0;

            t0 = System.nanoTime();
            for (int i = 0; i < n; i++) { synchronized (lock) { sink += i; } }
            long block = System.nanoTime() - t0;

            System.out.println("ROUND " + round
                    + " plain_ns=" + (plain / n)
                    + " syncMethod_ns=" + (sync / n)
                    + " syncBlock_ns=" + (block / n)
                    + " sink=" + sink);
        }
    }
}
