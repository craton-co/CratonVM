import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;

/**
 * Does GC still enqueue a WeakReference whose referent is unreachable?
 *
 * Reported as broken on the same dev range as the FileLockTable NPE, so this
 * checks whether the two share a cause.
 */
public class EnqProbe {

    public static void main(String... args) throws Exception {
        ReferenceQueue<Object> q = new ReferenceQueue<>();

        // 1. explicit enqueue() still works
        Object a = new Object();
        WeakReference<Object> ra = new WeakReference<>(a, q);
        System.out.println("explicit enqueue() = " + ra.enqueue());
        System.out.println("  polled = " + (q.poll() != null));

        // 2. GC-driven enqueue of an unreachable referent
        Object b = new Object();
        WeakReference<Object> rb = new WeakReference<>(b, q);
        b = null;
        Reference<?> got = null;
        for (int i = 0; i < 20 && got == null; i++) {
            System.gc();
            Thread.sleep(20);
            got = q.poll();
        }
        System.out.println("gc enqueued it = " + (got != null));
        System.out.println("referent cleared = " + (rb.get() == null));
        System.out.println("=== DONE");
    }
}
