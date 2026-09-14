import java.util.concurrent.atomic.AtomicInteger;

public final class Counter {
    public static void main(String[] args) throws Exception {
        AtomicInteger count = new AtomicInteger();
        System.out.println("Begin");
        Thread thread = Thread.startVirtualThread(() -> {
            count.incrementAndGet();
        });
        thread.join();
        System.out.println("After: n=" + count.get());
    }
}
