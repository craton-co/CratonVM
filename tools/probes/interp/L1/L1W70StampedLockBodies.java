// Interpreter round i2 wave 70, lane L1:
//
// * `CRATONVM_COMPAT_JDK_STAMPED_LOCK` (the `JdkStampedLock` HotSpot-alignment
//   row; docs/internal/fixed-bugs/interpreter-i69-L1-proposal-compatible-stampedlock-runs-its-jdk-bodies-DONE-20261102.md):
//   `--compatible` over a real JDK no longer registers the 31 `StampedLock` /
//   lock-view stand-ins, so the JDK 25 bodies run (the real `state` word, a
//   contended acquire parked through `LockSupport.park(this)` on the JDK's
//   wait queue), as `--jdk-only` already did.
//
// Columns, read without an agent: the state; `jmx-same` (JMX agrees);
// `lock=` the class of `ThreadInfo.getLockName()`; `blocker=` the class of
// `LockSupport.getBlocker(t)`. The stamp rows print the raw stamps: JDK 25's
// encoding (ORIGIN 256, WBIT 128, one reader unit 1) is Java-visible.
//
// Run in both modes: `cratonvm L1W70StampedLockBodies` (jdk-only) and
// `cratonvm --compatible L1W70StampedLockBodies`; both must print the block
// below. `=0` arm: `CRATONVM_COMPAT_JDK_STAMPED_LOCK=0` brings the stand-ins
// back; the `stack` row then moves (no `StampedLock.acquireWrite` frame), and
// any stamp / exception row that moves is a stand-in difference to file.
// The `stress` row is the one to run under `CRATONVM_DBG_STALE_OBJREF=1`
// (with the row on, no native writes the `state` mirror, so it should report
// nothing for this class: the i68-L1 stale-receiver page).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// fresh tryOptimisticRead: 256
// writeLock: 384 isWriteLocked=true isReadLocked=false optimistic-while-written=0
// toString(write-locked): [Write-locked]
// after unlockWrite: validate(256)=false tryOptimisticRead=512
// readLock: 513 count=1 toString=[Read-locks:1]
// tryConvertToWriteLock(read): 640
// unlock(write stamp) then tryOptimisticRead: 768
// readLock x2: count=2 tryWriteLock=0
// unlockRead(bad stamp): java.lang.IllegalMonitorStateException message=null
// unlockWrite(read stamp): java.lang.IllegalMonitorStateException message=null
// asReadLock().newCondition(): java.lang.UnsupportedOperationException
// static isWriteLockStamp(384)=true isReadLockStamp(513)=true isOptimisticReadStamp(256)=true isLockStamp(256)=false
// writeLock (write-held): WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock blocker=java.util.concurrent.locks.StampedLock
// readLock (write-held): WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock blocker=java.util.concurrent.locks.StampedLock
// tryWriteLock(60s) (read-held): TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock blocker=java.util.concurrent.locks.StampedLock
// asWriteLock().lock (write-held): WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock blocker=java.util.concurrent.locks.StampedLock
// asReadLock().lockInterruptibly (write-held): WAITING jmx-same=true lock=java.util.concurrent.locks.StampedLock blocker=java.util.concurrent.locks.StampedLock
// same-lock(writer, holder's lock): true
// writeLockInterruptibly interrupted: java.lang.InterruptedException
// stack(writeLock contended): jdk.internal.misc.Unsafe.park > java.util.concurrent.locks.LockSupport.park > java.util.concurrent.locks.StampedLock.acquireWrite
// stress: count=40000 isLocked=false
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.Lock;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.StampedLock;

public class L1W70StampedLockBodies {
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

    interface Blocking {
        void run() throws Exception;
    }

    static boolean waiting(Thread.State s) {
        return s == Thread.State.WAITING || s == Thread.State.TIMED_WAITING;
    }

    static String className(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static String lockClass(String name) {
        if (name == null) {
            return "null";
        }
        int at = name.indexOf('@');
        return at < 0 ? name : name.substring(0, at);
    }

    static String suffix(Object o) {
        String s = String.valueOf(o);
        int at = s.indexOf('[');
        return at < 0 ? s : s.substring(at);
    }

    static Thread start(String label, Blocking body) {
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
        return t;
    }

    static void settle(Thread t) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!waiting(t.getState()) && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        Thread.sleep(50);
    }

    static ThreadInfo reading(Thread t) throws InterruptedException {
        ThreadInfo info = MX.getThreadInfo(t.threadId());
        for (int i = 0; i < 50; i++) {
            Thread.State s = t.getState();
            if (waiting(s) && info != null && info.getThreadState() == s && info.getLockName() != null) {
                break;
            }
            Thread.sleep(2);
            info = MX.getThreadInfo(t.threadId());
        }
        return info;
    }

    /** Prints one row for a thread blocked on {@code sl}, then runs {@code release} and joins it. */
    static void row(String label, StampedLock sl, Blocking body, Runnable release) throws Exception {
        Thread t = start(label, body);
        settle(t);
        ThreadInfo info = reading(t);
        Thread.State s = t.getState();
        Object blocker = LockSupport.getBlocker(t);
        boolean same = info != null && info.getThreadState() == s;
        System.out.println(label + ": " + s + " jmx-same=" + same
                + " lock=" + lockClass(info == null ? null : info.getLockName())
                + " blocker=" + className(blocker));
        release.run();
        t.join(5000);
        if (t.isAlive()) {
            System.out.println(label + ": still alive after release");
        }
    }

    static String exceptionOf(Blocking b) {
        try {
            b.run();
            return "no exception";
        } catch (Exception e) {
            return e.getClass().getName() + " message=" + e.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        // ---- Stamps (single-threaded; JDK 25's encoding is Java-visible) ----
        StampedLock sl = new StampedLock();
        System.out.println("fresh tryOptimisticRead: " + sl.tryOptimisticRead());
        long ws = sl.writeLock();
        System.out.println("writeLock: " + ws + " isWriteLocked=" + sl.isWriteLocked()
                + " isReadLocked=" + sl.isReadLocked()
                + " optimistic-while-written=" + sl.tryOptimisticRead());
        System.out.println("toString(write-locked): " + suffix(sl));
        sl.unlockWrite(ws);
        System.out.println("after unlockWrite: validate(256)=" + sl.validate(256L)
                + " tryOptimisticRead=" + sl.tryOptimisticRead());
        long rs = sl.readLock();
        System.out.println("readLock: " + rs + " count=" + sl.getReadLockCount() + " toString=" + suffix(sl));
        long cs = sl.tryConvertToWriteLock(rs);
        System.out.println("tryConvertToWriteLock(read): " + cs);
        sl.unlock(cs);
        System.out.println("unlock(write stamp) then tryOptimisticRead: " + sl.tryOptimisticRead());
        long r1 = sl.readLock();
        long r2 = sl.readLock();
        System.out.println("readLock x2: count=" + sl.getReadLockCount() + " tryWriteLock=" + sl.tryWriteLock());
        sl.unlockRead(r2);
        sl.unlockRead(r1);
        System.out.println("unlockRead(bad stamp): " + exceptionOf(() -> sl.unlockRead(12345L)));
        long r3 = sl.readLock();
        System.out.println("unlockWrite(read stamp): " + exceptionOf(() -> sl.unlockWrite(r3)));
        sl.unlockRead(r3);
        try {
            sl.asReadLock().newCondition();
            System.out.println("asReadLock().newCondition(): no exception");
        } catch (RuntimeException e) {
            System.out.println("asReadLock().newCondition(): " + e.getClass().getName());
        }
        System.out.println("static isWriteLockStamp(384)=" + StampedLock.isWriteLockStamp(384L)
                + " isReadLockStamp(513)=" + StampedLock.isReadLockStamp(513L)
                + " isOptimisticReadStamp(256)=" + StampedLock.isOptimisticReadStamp(256L)
                + " isLockStamp(256)=" + StampedLock.isLockStamp(256L));

        // ---- Contended acquires: state, JMX lock, park blocker ----
        StampedLock held = new StampedLock();
        long[] hw = {held.writeLock()};
        row("writeLock (write-held)", held, () -> {
            long s = held.writeLock();
            held.unlockWrite(s);
        }, () -> held.unlockWrite(hw[0]));

        hw[0] = held.writeLock();
        row("readLock (write-held)", held, () -> {
            long s = held.readLock();
            held.unlockRead(s);
        }, () -> held.unlockWrite(hw[0]));

        long[] hr = {held.readLock()};
        row("tryWriteLock(60s) (read-held)", held, () -> {
            long s = held.tryWriteLock(60, TimeUnit.SECONDS);
            if (s != 0L) {
                held.unlockWrite(s);
            }
        }, () -> held.unlockRead(hr[0]));

        Lock wv = held.asWriteLock();
        hw[0] = held.writeLock();
        row("asWriteLock().lock (write-held)", held, () -> {
            wv.lock();
            wv.unlock();
        }, () -> held.unlockWrite(hw[0]));

        Lock rv = held.asReadLock();
        hw[0] = held.writeLock();
        row("asReadLock().lockInterruptibly (write-held)", held, () -> {
            rv.lockInterruptibly();
            rv.unlock();
        }, () -> held.unlockWrite(hw[0]));

        // Two blocked writers name the same lock object (class AND identity hash).
        hw[0] = held.writeLock();
        Thread w = start("writer", () -> {
            long s = held.writeLock();
            held.unlockWrite(s);
        });
        settle(w);
        ThreadInfo wi = reading(w);
        String expect = held.getClass().getName() + '@' + Integer.toHexString(System.identityHashCode(held));
        System.out.println("same-lock(writer, holder's lock): " + (wi != null && expect.equals(wi.getLockName())));
        held.unlockWrite(hw[0]);
        w.join(5000);

        // An interruptible acquire answers an interrupt with InterruptedException.
        hw[0] = held.writeLock();
        String[] outcome = {"no outcome"};
        Thread ti = new Thread(() -> {
            try {
                long s = held.writeLockInterruptibly();
                held.unlockWrite(s);
                outcome[0] = "acquired";
            } catch (InterruptedException e) {
                outcome[0] = e.getClass().getName();
            }
        }, "interruptible");
        ti.setDaemon(true);
        ti.start();
        settle(ti);
        ti.interrupt();
        ti.join(5000);
        System.out.println("writeLockInterruptibly interrupted: " + outcome[0]);
        held.unlockWrite(hw[0]);

        // The three top frames of a contended writer.
        hw[0] = held.writeLock();
        Thread stacked = start("stacked", () -> {
            long s = held.writeLock();
            held.unlockWrite(s);
        });
        settle(stacked);
        StackTraceElement[] st = stacked.getStackTrace();
        StringBuilder top = new StringBuilder();
        for (int i = 0; i < Math.min(3, st.length); i++) {
            if (i > 0) {
                top.append(" > ");
            }
            top.append(st[i].getClassName()).append('.').append(st[i].getMethodName());
        }
        System.out.println("stack(writeLock contended): " + top);
        held.unlockWrite(hw[0]);
        stacked.join(5000);

        // ---- Contended stress with allocation (run under CRATONVM_DBG_STALE_OBJREF=1) ----
        StampedLock stress = new StampedLock();
        long[] count = {0};
        Thread[] workers = new Thread[4];
        for (int i = 0; i < workers.length; i++) {
            workers[i] = new Thread(() -> {
                Object sink = null;
                for (int j = 0; j < 10_000; j++) {
                    sink = new byte[256];
                    if ((j & 3) == 0) {
                        long s = stress.readLock();
                        try {
                            sink = new long[] {count[0]};
                        } finally {
                            stress.unlockRead(s);
                        }
                    }
                    long s = stress.writeLock();
                    try {
                        count[0]++;
                    } finally {
                        stress.unlockWrite(s);
                    }
                }
                if (sink == null) {
                    System.out.println("unreachable");
                }
            }, "stress-" + i);
            workers[i].start();
        }
        for (Thread t : workers) {
            t.join();
        }
        System.out.println("stress: count=" + count[0] + " isLocked=" + (stress.isReadLocked() || stress.isWriteLocked()));
        System.out.println("done");
    }
}
