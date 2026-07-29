import org.jboss.threads.EnhancedQueueExecutor;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

public class EqeProbe {
    public static void main(String[] a) throws Exception {
        EnhancedQueueExecutor e = new EnhancedQueueExecutor.Builder()
            .setCorePoolSize(2)
            .setMaximumPoolSize(4)
            .setKeepAliveTime(java.time.Duration.ofSeconds(30))
            .setThreadFactory(r -> { Thread t = new Thread(r, "eqe-probe"); t.setDaemon(true); return t; })
            .build();
        System.out.println("built core=" + e.getCorePoolSize() + " max=" + e.getMaximumPoolSize());
        final CountDownLatch l = new CountDownLatch(1);
        e.execute(new Runnable() { public void run() {
            System.out.println("task ran on " + Thread.currentThread().getName());
            l.countDown();
        }});
        System.out.println("await=" + l.await(15, TimeUnit.SECONDS));
        e.shutdown();
        System.out.println("== DONE OK ==");
    }
}
