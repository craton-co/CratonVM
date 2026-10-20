import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.lang.reflect.Field;

/** Reproduce a GC taken between poll0's head read and head write, under its lock.
 * Run with --add-opens=java.base/java.lang.ref=ALL-UNNAMED.
 * The collector must leave Java queue mutation to a thread taking that lock.
 */
public class GcReferenceQueuePausedPoll {
    static Field field(Class<?> owner, String name) throws Exception {
        Field f = owner.getDeclaredField(name);
        f.setAccessible(true);
        return f;
    }

    static WeakReference<Object> dropped(ReferenceQueue<Object> queue) {
        return new WeakReference<>(new Object(), queue);
    }

    public static void main(String[] args) throws Exception {
        ReferenceQueue<Object> queue = new ReferenceQueue<>();
        WeakReference<Object> marker = new WeakReference<>(new Object(), queue);
        marker.enqueue();
        WeakReference<Object> pending = dropped(queue);
        Field head = field(ReferenceQueue.class, "head");
        Field length = field(ReferenceQueue.class, "queueLength");
        Field next = field(Reference.class, "next");
        Field owner = field(Reference.class, "queue");
        Object lock = field(ReferenceQueue.class, "lock").get(queue);
        Object nullQueue = field(ReferenceQueue.class, "NULL_QUEUE").get(null);
        synchronized (lock) {
            Object r = head.get(queue);
            Object rn = next.get(r);
            System.gc();
            // Finish the operation whose local r/rn preceded the collection.
            owner.set(r, nullQueue);
            head.set(queue, rn == r ? null : rn);
            next.set(r, r);
            length.setLong(queue, length.getLong(queue) - 1);
        }
        boolean delivered = false;
        long end = System.nanoTime() + 5_000_000_000L;
        while (System.nanoTime() < end && !delivered) {
            delivered = queue.poll() == pending;
            if (!delivered) Thread.sleep(2);
        }
        System.out.println("cleared: " + pending.refersTo(null));
        System.out.println("delivered after paused poll: " + delivered);
        Reference.reachabilityFence(marker);
        Reference.reachabilityFence(pending);
    }
}
