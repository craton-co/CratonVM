// Interpreter round i2 wave 60, lane L6: the thread state of a thread
// blocked in the JDK waits that the `--compatible` stand-ins of
// `native-builtins` model with a blocking region
// (docs/internal/fixed-bugs/interpreter-i59-L6-blocking-regions-not-yet-classified-DONE-20261115.md):
// a timed wait reads TIMED_WAITING, an untimed one WAITING.
//
// Each row starts a thread that blocks in one call, waits until it leaves
// RUNNABLE (5 s at most), and prints its `getState()` and whether the JMX
// `ThreadInfo` says the same. `ecs.poll timed` comes first and again last
// (cold-VM first-call gaps).
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box):
//
//   ecs.poll timed: TIMED_WAITING jmx-same=true
//   ecs.take: WAITING jmx-same=true
//   DelayQueue.take delayed head: TIMED_WAITING jmx-same=true
//   DelayQueue.take empty: WAITING jmx-same=true
//   StampedLock.tryReadLock timed: TIMED_WAITING jmx-same=true
//   StampedLock.tryWriteLock timed: TIMED_WAITING jmx-same=true
//   Thread.sleep: TIMED_WAITING jmx-same=true
//   ecs.poll timed: TIMED_WAITING jmx-same=true
//   done
//
// With a real JDK the stand-ins lose to the JDK's bytecode (they are
// `SyntheticStub` / `Bridge` registrations over concrete methods), so this
// probe checks the answer both modes give through the JDK's own Java; the
// stand-ins were reclassified (`BlockKind::TimedWaiting`) to the same states.
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.util.concurrent.DelayQueue;
import java.util.concurrent.Delayed;
import java.util.concurrent.ExecutorCompletionService;
import java.util.concurrent.Executors;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.StampedLock;

public class L6W60TimedStandInStates {
    static final class Later implements Delayed {
        final long at = System.nanoTime() + TimeUnit.SECONDS.toNanos(60);

        public long getDelay(TimeUnit unit) {
            return unit.convert(at - System.nanoTime(), TimeUnit.NANOSECONDS);
        }

        public int compareTo(Delayed o) {
            return Long.compare(getDelay(TimeUnit.NANOSECONDS), o.getDelay(TimeUnit.NANOSECONDS));
        }
    }

    interface Blocking {
        void run() throws Exception;
    }

    static void row(String label, Blocking body) throws Exception {
        Thread t = new Thread(() -> {
            try {
                body.run();
            } catch (InterruptedException e) {
                // Interrupted at the end of the row.
            } catch (Exception e) {
                System.out.println(label + ": threw " + e);
            }
        }, label);
        t.setDaemon(true);
        t.start();
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        Thread.State s = t.getState();
        while ((s == Thread.State.NEW || s == Thread.State.RUNNABLE) && System.nanoTime() < deadline) {
            Thread.sleep(5);
            s = t.getState();
        }
        // Settled: read both answers once more.
        Thread.sleep(50);
        s = t.getState();
        ThreadInfo info = ManagementFactory.getThreadMXBean().getThreadInfo(t.threadId());
        boolean same = info != null && info.getThreadState() == s;
        System.out.println(label + ": " + s + " jmx-same=" + same);
        t.interrupt();
        t.join(2000);
    }

    public static void main(String[] args) throws Exception {
        ExecutorService pool = Executors.newSingleThreadExecutor();
        row("ecs.poll timed", () -> new ExecutorCompletionService<Object>(pool).poll(60, TimeUnit.SECONDS));
        row("ecs.take", () -> new ExecutorCompletionService<Object>(pool).take());
        row("DelayQueue.take delayed head", () -> {
            DelayQueue<Later> q = new DelayQueue<>();
            q.put(new Later());
            q.take();
        });
        row("DelayQueue.take empty", () -> new DelayQueue<Later>().take());
        StampedLock lock = new StampedLock();
        long w = lock.writeLock();
        row("StampedLock.tryReadLock timed", () -> lock.tryReadLock(60, TimeUnit.SECONDS));
        row("StampedLock.tryWriteLock timed", () -> lock.tryWriteLock(60, TimeUnit.SECONDS));
        lock.unlockWrite(w);
        row("Thread.sleep", () -> Thread.sleep(60_000));
        row("ecs.poll timed", () -> new ExecutorCompletionService<Object>(pool).poll(60, TimeUnit.SECONDS));
        pool.shutdownNow();
        System.out.println("done");
    }
}
