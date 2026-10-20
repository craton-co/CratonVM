// Interpreter round i2 wave 59, lane L6: `ThreadInfo.getPriority()` and
// `ThreadInfo.isDaemon()` of the current thread and of two blocked threads
// (a daemon at priority 7 in `LockSupport.park`, a user thread at priority 3
// in `Object.wait`), from `ThreadMXBean.getThreadInfo(long)` and
// `dumpAllThreads(false, false)`; asked twice
// (docs/internal/fixed-bugs/interpreter-L6-jmx-thread-info-priority-and-daemon-are-constants-FIXED-20261023.md).
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box, MEASURED):
//
//   round 1
//   info main: priority=5 daemon=false
//   info w59-daemon: priority=7 daemon=true
//   info w59-user: priority=3 daemon=false
//   dump w59-daemon: priority=7 daemon=true
//   dump w59-user: priority=3 daemon=false
//   round 2
//   (the same five lines)
//   done
//
// CratonVM before wave 59 (debug build, both modes): every row
// `priority=5 daemon=false`.
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.locks.LockSupport;

public class L6W59ThreadInfoPriorityDaemon {
    static String row(ThreadInfo info) {
        return info.getThreadName() + ": priority=" + info.getPriority() + " daemon=" + info.isDaemon();
    }

    static void round(long n) throws Exception {
        System.out.println("round " + n);
        AtomicBoolean done = new AtomicBoolean();
        Thread parker = new Thread(() -> {
            while (!done.get()) {
                LockSupport.park();
            }
        }, "w59-daemon");
        parker.setDaemon(true);
        parker.setPriority(7);
        Object lock = new Object();
        Thread waiter = new Thread(() -> {
            synchronized (lock) {
                while (!done.get()) {
                    try {
                        lock.wait();
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }
        }, "w59-user");
        waiter.setPriority(3);
        parker.start();
        waiter.start();
        long deadline = System.nanoTime() + 2_000_000_000L;
        while (System.nanoTime() < deadline
                && (parker.getState() != Thread.State.WAITING || waiter.getState() != Thread.State.WAITING)) {
            Thread.sleep(1);
        }
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        System.out.println("info " + row(mx.getThreadInfo(Thread.currentThread().threadId())));
        System.out.println("info " + row(mx.getThreadInfo(parker.threadId())));
        System.out.println("info " + row(mx.getThreadInfo(waiter.threadId())));
        // By name: the dump's thread order is not this probe's question.
        java.util.TreeSet<String> dumped = new java.util.TreeSet<>();
        for (ThreadInfo info : mx.dumpAllThreads(false, false)) {
            if (info.getThreadName().startsWith("w59-")) {
                dumped.add("dump " + row(info));
            }
        }
        for (String line : dumped) {
            System.out.println(line);
        }
        done.set(true);
        LockSupport.unpark(parker);
        synchronized (lock) {
            lock.notifyAll();
        }
        parker.join(10_000);
        waiter.join(10_000);
    }

    public static void main(String[] args) throws Exception {
        round(1);
        round(2);
        System.out.println("done");
    }
}
