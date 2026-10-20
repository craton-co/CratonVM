import java.lang.ref.SoftReference;

/** A constructor copies the previous GC clock, including pre-construction age.
 * Run with -Xmx64m -XX:SoftRefLRUPolicyMSPerMB=20.
 */
public class SoftLruClockAgeProbe {
    static volatile boolean start;
    static volatile boolean failed;

    public static void main(String[] args) throws Exception {
        if (args.length != 0 && args[0].equals("contended")) {
            contended();
            return;
        }
        System.gc();
        Thread.sleep(1800);
        SoftReference<Object> idle = new SoftReference<>(new Object());
        // This collection evaluates the old clock, then advances it.
        System.gc();
        System.out.println("kept before clock advance: " + !idle.refersTo(null));
        // Even a just-constructed reference carries the older master clock.
        System.gc();
        System.out.println("cleared after clock advance: " + idle.refersTo(null));
    }

    static void contended() throws Exception {
        System.gc();
        SoftReference<Object> idle = new SoftReference<>(new Object());
        Thread[] workers = new Thread[4];
        for (int i = 0; i < workers.length; i++) {
            workers[i] = new Thread(() -> {
                while (!start) Thread.yield();
                long end = System.nanoTime() + 2_500_000_000L;
                try {
                    do {
                        System.gc();
                        Thread.sleep(5);
                    } while (System.nanoTime() < end);
                } catch (InterruptedException e) {
                    failed = true;
                }
            });
            workers[i].start();
        }
        start = true;
        for (Thread worker : workers) worker.join();
        System.out.println("cleared during contended calls: " + (!failed && idle.refersTo(null)));
    }
}
