import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class ThreadLocalFinalizeProbe {
    static final AtomicInteger LIVE = new AtomicInteger(0);
    static final ThreadLocal<Holder> TL = new ThreadLocal<>();

    static final class Holder {
        Holder() {
            LIVE.incrementAndGet();
        }

        @Override
        protected void finalize() {
            LIVE.decrementAndGet();
        }
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 11; i++) {
            Thread t = new Thread(() -> TL.set(new Holder()));
            t.start();
            t.join();
        }
        System.out.println("threads joined, LIVE=" + LIVE.get());
        long start = System.nanoTime();
        int gcCalls = 0;
        while (LIVE.get() > 0 && System.nanoTime() - start < 30_000_000_000L) {
            System.gc();
            gcCalls++;
            LockSupport.parkNanos(100_000_000L);
        }
        double elapsed = (System.nanoTime() - start) / 1e9;
        System.out.println("DONE live=" + LIVE.get() + " elapsed_s=" + elapsed + " gcCalls=" + gcCalls);
        System.out.println(LIVE.get() == 0 ? "PROBE-OK" : "PROBE-FAIL");
    }
}
