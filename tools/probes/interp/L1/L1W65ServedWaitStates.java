// Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`;
// docs/internal/fixed-bugs/interpreter-i64-L1-proposal-classify-the-unrecorded-wait-regions-DONE-20261028.md):
// the Java-visible state of a thread blocked in each JDK wait whose
// `--compatible` stand-in this wave classified (a served monitor wait or a
// blocking region that stands for an AQS / StampedLock park), read without an
// agent: `Thread.getState()`, whether JMX's `ThreadInfo.getThreadState()`
// agrees, and the class of `ThreadInfo.getLockName()` (HotSpot: the park
// blocker). The JVMTI kind bit (PARKED vs IN_OBJECT_WAIT) of the same rows is
// `L1W64JvmtiAllWaitKinds` (agent, experimental-debug build).
//
// The row the wave fixes: `CountDownLatch.await(ms)` under `--compatible`
// read WAITING (the served native polled in untimed-looking monitor-wait
// slices); HotSpot reads TIMED_WAITING
// (i64-orch-compatible-timed-countdownlatch-await-reads-waiting-20261028.md).
// `CRATONVM_SERVED_WAIT_KINDS=0` brings the old answer back.
//
// The last row is a region NOT changed here (filed:
// docs/internal/fixed-bugs/interpreter-i65-L1-virtual-thread-unblocker-reads-timed-waiting-FIXED-20261010.md):
// the `VirtualThread-unblocker` thread sits in the JDK native
// `takeVirtualThreadListToUnblock`, RUNNABLE on HotSpot. Since 2026-10-10
// (JDK-driven virtual threads) it exists here too and reads RUNNABLE, both
// modes.
//
// Run in both modes: `cratonvm L1W65ServedWaitStates` (jdk-only) and
// `cratonvm --compatible L1W65ServedWaitStates`; both must print the block
// below.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// CountDownLatch.await(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.CountDownLatch$Sync
// CountDownLatch.await(): WAITING jmx-same=true lock=java.util.concurrent.CountDownLatch$Sync
// Semaphore.acquire: WAITING jmx-same=true lock=java.util.concurrent.Semaphore$NonfairSync
// Semaphore.tryAcquire(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.Semaphore$NonfairSync
// LinkedBlockingQueue.take: WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// LinkedBlockingQueue.poll(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// StampedLock.readLock: WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock
// StampedLock.writeLock: WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock
// StampedLock.tryWriteLock(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock
// StampedLock.asWriteLock().lock: WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock
// ReentrantReadWriteLock.readLock().lock: WAITING jmx-same=true lock=java.util.concurrent.locks.ReentrantReadWriteLock$NonfairSync
// ReentrantReadWriteLock.writeLock().lock: WAITING jmx-same=true lock=java.util.concurrent.locks.ReentrantReadWriteLock$NonfairSync
// DelayQueue.take empty: WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// DelayQueue.take delayed head: TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// ExecutorCompletionService.take: WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// ExecutorCompletionService.poll(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// CountDownLatch.await(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.CountDownLatch$Sync
// VirtualThread-unblocker: RUNNABLE
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.DelayQueue;
import java.util.concurrent.Delayed;
import java.util.concurrent.ExecutorCompletionService;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.Semaphore;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.concurrent.locks.StampedLock;

public class L1W65ServedWaitStates {
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
        // Settled; a stand-in that polls in slices may read RUNNABLE for a
        // few microseconds between two of them, so take a reading in which
        // the thread is waiting and JMX was asked while it was.
        Thread.sleep(50);
        Thread.State s = t.getState();
        ThreadInfo info = MX.getThreadInfo(t.threadId());
        for (int i = 0; i < 50; i++) {
            if (waiting(s) && info != null && info.getThreadState() == s) {
                break;
            }
            Thread.sleep(2);
            s = t.getState();
            info = MX.getThreadInfo(t.threadId());
        }
        boolean same = info != null && info.getThreadState() == s;
        System.out.println(label + ": " + s + " jmx-same=" + same + " lock=" + lockClass(info));
        t.interrupt();
        t.join(2000);
    }

    public static void main(String[] args) throws Exception {
        row("CountDownLatch.await(ms)", () -> new CountDownLatch(1).await(60, TimeUnit.SECONDS));
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

        row("DelayQueue.take empty", () -> new DelayQueue<Later>().take());
        row("DelayQueue.take delayed head", () -> {
            DelayQueue<Later> q = new DelayQueue<>();
            q.put(new Later());
            q.take();
        });

        ExecutorService pool = Executors.newSingleThreadExecutor();
        row("ExecutorCompletionService.take", () -> new ExecutorCompletionService<Object>(pool).take());
        row("ExecutorCompletionService.poll(ms)",
                () -> new ExecutorCompletionService<Object>(pool).poll(60, TimeUnit.SECONDS));
        pool.shutdownNow();
        // Again last: a cold-VM first call is not what the row measures.
        row("CountDownLatch.await(ms)", () -> new CountDownLatch(1).await(60, TimeUnit.SECONDS));

        // The JDK starts the unblocker with the first virtual thread.
        Thread.ofVirtual().start(() -> { }).join();
        Thread unblocker = null;
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            if (t.getName().equals("VirtualThread-unblocker")) {
                unblocker = t;
            }
        }
        if (unblocker == null) {
            System.out.println("VirtualThread-unblocker: absent");
        } else {
            Thread.sleep(50);
            System.out.println("VirtualThread-unblocker: " + unblocker.getState());
        }
        System.out.println("done");
    }
}
