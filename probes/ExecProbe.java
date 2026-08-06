import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

/** Receiver-shape / async-semantics probe for the ThreadPoolExecutor.execute work. */
public class ExecProbe {
    static void probe(String label, ExecutorService es) throws Exception {
        System.out.println(label + " class=" + es.getClass().getName());
        final String submitting = Thread.currentThread().getName();
        final ArrayBlockingQueue<String> seen = new ArrayBlockingQueue<>(4);
        es.execute(new Runnable() {
            public void run() {
                try { seen.put(Thread.currentThread().getName()); } catch (InterruptedException e) {}
            }
        });
        String ran = seen.poll(10, TimeUnit.SECONDS);
        System.out.println(label + " submitting=" + submitting + " running=" + ran
                + " async=" + (ran != null && !ran.equals(submitting)));
        es.shutdown();
        System.out.println(label + " isShutdown=" + es.isShutdown());
    }

    public static void main(String[] args) throws Exception {
        probe("fixed", Executors.newFixedThreadPool(2));
        probe("cached", Executors.newCachedThreadPool());
        probe("single", Executors.newSingleThreadExecutor());
        probe("sched", Executors.newScheduledThreadPool(1));
        probe("sched1", Executors.newSingleThreadScheduledExecutor());
        probe("direct", new ThreadPoolExecutor(1, 1, 0L, TimeUnit.MILLISECONDS,
                new LinkedBlockingQueue<Runnable>()));
        System.out.println("PROBE-DONE");
    }
}
