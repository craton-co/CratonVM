import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;

public final class VthreadProbe {
    public static void main(String[] args) throws Exception {
        int threadCount = args.length == 0 ? 10_000 : Integer.parseInt(args[0]);
        CountDownLatch done = new CountDownLatch(threadCount);
        AtomicInteger counted = new AtomicInteger();
        for (int i = 0; i < threadCount; i++) {
            Thread.startVirtualThread(() -> {
                try {
                    Thread.sleep(10);
                    counted.incrementAndGet();
                } catch (InterruptedException e) {
                    throw new AssertionError(e);
                } finally {
                    done.countDown();
                }
            });
        }
        done.await();
        boolean ok = counted.get() == threadCount;
        System.out.println("counted=" + counted.get() + " ok=" + ok);
        if (!ok) {
            throw new AssertionError("virtual thread loss");
        }
        System.out.println("OK");
    }
}
