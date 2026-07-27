import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.TimeUnit;

/** Does async work actually run when Quarkus's custom common-pool thread factory is set? */
public class FjpProbe {
    public static void main(String[] args) throws Exception {
        System.out.println("threadFactory prop = "
                + System.getProperty("java.util.concurrent.ForkJoinPool.common.threadFactory"));
        ForkJoinPool p = ForkJoinPool.commonPool();
        System.out.println("commonPool = " + p.getClass().getName() + " parallelism=" + p.getParallelism());

        try {
            CompletableFuture<String> f =
                    CompletableFuture.supplyAsync(() -> Thread.currentThread().getName());
            System.out.println("supplyAsync -> " + f.get(15, TimeUnit.SECONDS));
        } catch (Throwable t) {
            System.out.println("supplyAsync FAILED: " + t);
        }

        try {
            System.out.println("commonPool.submit -> "
                    + p.submit(() -> Thread.currentThread().getName()).get(15, TimeUnit.SECONDS));
        } catch (Throwable t) {
            System.out.println("commonPool.submit FAILED: " + t);
        }

        Thread t = new Thread(() -> System.out.println("  plain thread ran on "
                + Thread.currentThread().getName()), "JPA Startup Thread");
        t.start();
        t.join(15000);
        System.out.println("plain thread alive-after-join=" + t.isAlive());
        System.out.println("done");
    }
}
