import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;

/**
 * A thread already blocked in {@code ReferenceQueue.remove()} must be woken
 * when the COLLECTOR enqueues a reference onto that queue, as HotSpot's
 * Reference Handler wakes it through {@code enqueue0}'s
 * {@code lock.notifyAll()}.
 *
 * Found by gc-common wave 2, lane F. Fixed by wave 3 behind
 * {@code CRATONVM_FINALIZER_THREAD=1}, and by wave 5 lane D for every
 * {@code --jdk-only} VM (2026-09-24). Under {@code --jdk-only} the real
 * {@code remove()} bytecode is {@code lock.wait()}. The collector links the
 * reference with raw slot writes, so before the fix nothing notified
 * {@code lock}: the untimed waiter slept until some other {@code enqueue()},
 * and the timed one until its timeout. {@code --compatible} dispatches
 * {@code remove} to natives that poll every 10 ms at most, and must stay
 * {@code PROBE-OK} unchanged.
 *
 * See {@code common-w2f-gc-enqueue-never-wakes-queue-waiters-FIXED-20260923.md}.
 *
 * Prints {@code PROBE-OK} when both waiters get their own reference within
 * five seconds of {@code System.gc()}. HotSpot takes milliseconds.
 */
public class QueueRemoveWakeProbe {
    public static void main(String[] a) throws Exception {
        ReferenceQueue<Object> untimedQ = new ReferenceQueue<>();
        ReferenceQueue<Object> timedQ = new ReferenceQueue<>();
        WeakReference<Object> untimed = new WeakReference<>(new Object(), untimedQ);
        WeakReference<Object> timed = new WeakReference<>(new Object(), timedQ);
        final Object[] got = new Object[2];
        Thread t1 = new Thread(() -> {
            try {
                got[0] = untimedQ.remove();
            } catch (InterruptedException e) {
                // left null: reported as not woken
            }
        }, "untimed-remove");
        Thread t2 = new Thread(() -> {
            try {
                got[1] = timedQ.remove(60_000);
            } catch (InterruptedException e) {
                // left null: reported as not woken
            }
        }, "timed-remove");
        t1.setDaemon(true);
        t2.setDaemon(true);
        t1.start();
        t2.start();
        Thread.sleep(300); // both are now in remove()
        long t0 = System.nanoTime();
        System.gc();
        t1.join(5_000);
        t2.join(5_000);
        long ms = (System.nanoTime() - t0) / 1_000_000;
        boolean ok = got[0] == untimed && got[1] == timed;
        System.out.println("untimed=" + (got[0] == untimed ? "woken" : "ASLEEP")
                + " timed=" + (got[1] == timed ? "woken" : "ASLEEP")
                + " ms=" + ms + (ok ? " PROBE-OK" : " PROBE-FAIL"));
    }
}
