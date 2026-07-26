import java.util.WeakHashMap;

public class WhmRepro4 {
    static final Object lock = new Object();
    static final WeakHashMap<Thread, String> map = new WeakHashMap<>();

    static String getSelf() {
        synchronized (lock) {
            return map.get(Thread.currentThread());
        }
    }

    static void putSelf(String v) {
        synchronized (lock) {
            map.put(Thread.currentThread(), v);
        }
    }

    public static void main(String[] args) throws Exception {
        putSelf("main-value");

        int rounds = 500;
        for (int r = 0; r < rounds; r++) {
            final int rr = r;
            Thread worker = new Thread(() -> {
                putSelf("worker-value-" + rr);
                for (int i = 0; i < 2000; i++) {
                    getSelf();
                }
            });
            worker.start();
            worker.join(); // full termination before next round, like ThreadLeakControl

            // Suite thread checks its OWN entry right after each per-round
            // forked worker fully terminates -- mirrors popAndDestroy()
            // running on the long-lived suite thread after each test method's
            // forked-thread statement completes.
            String v = getSelf();
            if (v == null) {
                System.out.println("MISS after round " + r + " mapSize=" + map.size());
                System.exit(1);
            }
        }
        System.out.println("Done. rounds=" + rounds + " final=" + getSelf() + " mapSize=" + map.size());
    }
}
