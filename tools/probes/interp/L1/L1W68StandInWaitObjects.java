// Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`;
// docs/internal/fixed-bugs/interpreter-i67-L1-proposal-regions-that-publish-their-monitor-DONE-20261031.md):
// the object each blocked thread names, read without an agent, for every
// stand-in wait whose `--compatible` row named no lock on the wave-67 host run
// (`L1W65ServedWaitStates`, `L1W67ClassifiedStandInWaits`):
//
// * `lock=`: the class of `ThreadInfo.getLockName()` -- HotSpot's waiting
//   monitor for an `Object.wait`, the park blocker for a park;
// * `blocker=`: the class of `LockSupport.getBlocker(t)` -- the thread's
//   `parkBlocker`, which the JDK's `LockSupport.park*(Object, ..)` bodies set
//   around the park (null for a monitor wait).
//
// What the switch changes under `--compatible`: the `LockSupport.park*(Object,
// ..)` bridges set `parkBlocker` as the JDK bodies do (and `getBlocker` reads
// it), the `StampedLock` stand-ins publish the lock as their blocker, the
// `ReferenceQueue.remove` poll publishes the queue's `lock` as its waiting
// monitor, and `Process.waitFor` publishes a `ConditionObject`.
// `CRATONVM_STAND_IN_WAIT_OBJECTS=0` brings the wave-67 answers back
// (`lock=null`, `blocker=null` on those rows).
//
// Known remaining difference (not this switch): the `--compatible`
// `CountDownLatch.await` stand-in waits on the latch's monitor and names
// `CountDownLatch$Sync` for `lock=`, but sets no `parkBlocker`, so its
// `blocker=` reads null where HotSpot reads `CountDownLatch$Sync`.
//
// Run in both modes: `cratonvm L1W68StandInWaitObjects` (jdk-only) and
// `cratonvm --compatible L1W68StandInWaitObjects`; both must print the block
// below. The process rows need a `sleep` binary (UNIX).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// CountDownLatch.await(): WAITING jmx-same=true lock=java.util.concurrent.CountDownLatch$Sync blocker=java.util.concurrent.CountDownLatch$Sync
// Semaphore.acquire: WAITING jmx-same=true lock=java.util.concurrent.Semaphore$NonfairSync blocker=java.util.concurrent.Semaphore$NonfairSync
// Semaphore.tryAcquire(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.Semaphore$NonfairSync blocker=java.util.concurrent.Semaphore$NonfairSync
// LinkedBlockingQueue.take: WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject blocker=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// LinkedBlockingQueue.poll(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject blocker=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// StampedLock.readLock: WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock blocker=java.util.concurrent.locks.StampedLock
// StampedLock.writeLock: WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock blocker=java.util.concurrent.locks.StampedLock
// StampedLock.tryWriteLock(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock blocker=java.util.concurrent.locks.StampedLock
// StampedLock.asWriteLock().lock: WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock blocker=java.util.concurrent.locks.StampedLock
// ReentrantReadWriteLock.readLock().lock: WAITING jmx-same=true lock=java.util.concurrent.locks.ReentrantReadWriteLock$NonfairSync blocker=java.util.concurrent.locks.ReentrantReadWriteLock$NonfairSync
// ReentrantReadWriteLock.writeLock().lock: WAITING jmx-same=true lock=java.util.concurrent.locks.ReentrantReadWriteLock$NonfairSync blocker=java.util.concurrent.locks.ReentrantReadWriteLock$NonfairSync
// DelayQueue.take delayed head: TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject blocker=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// ExecutorCompletionService.poll(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject blocker=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// ReferenceQueue.remove(): WAITING jmx-same=true lock=java.lang.ref.ReferenceQueue$Lock blocker=null
// ReferenceQueue.remove(ms): TIMED_WAITING jmx-same=true lock=java.lang.ref.ReferenceQueue$Lock blocker=null
// Process.waitFor(): WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject blocker=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// Process.waitFor(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject blocker=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// getBlocker(running thread): null
// getBlocker(null): java.lang.NullPointerException
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.lang.ref.ReferenceQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.DelayQueue;
import java.util.concurrent.Delayed;
import java.util.concurrent.ExecutorCompletionService;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.Semaphore;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.concurrent.locks.StampedLock;

public class L1W68StandInWaitObjects {
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

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

    static boolean waiting(Thread.State s) {
        return s == Thread.State.WAITING || s == Thread.State.TIMED_WAITING;
    }

    static String className(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static String lockClass(ThreadInfo info) {
        if (info == null || info.getLockName() == null) {
            return "null";
        }
        String name = info.getLockName();
        int at = name.indexOf('@');
        return at < 0 ? name : name.substring(0, at);
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
        while (!waiting(t.getState()) && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        // Settled; a stand-in that polls in slices may be between two of them
        // for a few microseconds, so take a reading in which the thread waits,
        // JMX agrees, and both name an object if they ever will.
        Thread.sleep(50);
        Thread.State s = t.getState();
        ThreadInfo info = MX.getThreadInfo(t.threadId());
        Object blocker = LockSupport.getBlocker(t);
        for (int i = 0; i < 50; i++) {
            if (waiting(s) && info != null && info.getThreadState() == s && info.getLockName() != null) {
                break;
            }
            Thread.sleep(2);
            s = t.getState();
            info = MX.getThreadInfo(t.threadId());
            blocker = LockSupport.getBlocker(t);
        }
        boolean same = info != null && info.getThreadState() == s;
        System.out.println(label + ": " + s + " jmx-same=" + same + " lock=" + lockClass(info)
                + " blocker=" + className(blocker));
        t.interrupt();
        t.join(2000);
    }

    public static void main(String[] args) throws Exception {
        row("CountDownLatch.await()", () -> new CountDownLatch(1).await());
        row("Semaphore.acquire", () -> new Semaphore(0).acquire());
        row("Semaphore.tryAcquire(ms)", () -> new Semaphore(0).tryAcquire(60, TimeUnit.SECONDS));
        row("LinkedBlockingQueue.take", () -> new LinkedBlockingQueue<Object>().take());
        row("LinkedBlockingQueue.poll(ms)", () -> new LinkedBlockingQueue<Object>().poll(60, TimeUnit.SECONDS));

        StampedLock sl = new StampedLock();
        long w = sl.writeLock();
        row("StampedLock.readLock", () -> sl.readLock());
        row("StampedLock.writeLock", () -> sl.writeLock());
        row("StampedLock.tryWriteLock(ms)", () -> sl.tryWriteLock(60, TimeUnit.SECONDS));
        row("StampedLock.asWriteLock().lock", () -> sl.asWriteLock().lock());
        sl.unlockWrite(w);

        ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
        rw.writeLock().lock();
        row("ReentrantReadWriteLock.readLock().lock", () -> rw.readLock().lock());
        rw.writeLock().unlock();
        rw.readLock().lock();
        row("ReentrantReadWriteLock.writeLock().lock", () -> rw.writeLock().lock());
        rw.readLock().unlock();

        row("DelayQueue.take delayed head", () -> {
            DelayQueue<Later> q = new DelayQueue<>();
            q.put(new Later());
            q.take();
        });

        ExecutorService pool = Executors.newSingleThreadExecutor();
        row("ExecutorCompletionService.poll(ms)",
                () -> new ExecutorCompletionService<Object>(pool).poll(60, TimeUnit.SECONDS));
        pool.shutdownNow();

        ReferenceQueue<Object> q = new ReferenceQueue<>();
        row("ReferenceQueue.remove()", () -> q.remove());
        row("ReferenceQueue.remove(ms)", () -> q.remove(60_000));

        Process p = new ProcessBuilder("sleep", "30").start();
        row("Process.waitFor()", () -> p.waitFor());
        row("Process.waitFor(ms)", () -> p.waitFor(60, TimeUnit.SECONDS));
        p.destroyForcibly();
        p.waitFor(5, TimeUnit.SECONDS);

        System.out.println("getBlocker(running thread): " + className(LockSupport.getBlocker(Thread.currentThread())));
        try {
            LockSupport.getBlocker(null);
            System.out.println("getBlocker(null): no exception");
        } catch (RuntimeException e) {
            System.out.println("getBlocker(null): " + e.getClass().getName());
        }
        System.out.println("done");
    }
}
