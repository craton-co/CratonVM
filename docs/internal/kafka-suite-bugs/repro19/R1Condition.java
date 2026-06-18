// Repro 1: ReentrantLock + Condition cross-thread handoff — exactly the shape
// of Kafka BufferPool.allocate()/deallocate(). Waiter blocks on a per-waiter
// Condition (lock.newCondition()) that the deallocator signals. PASS means the
// blocking await() was woken across threads; HANG means the wakeup was lost.
import java.util.ArrayDeque;
import java.util.Deque;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.ReentrantLock;

public class R1Condition {
    static final ReentrantLock lock = new ReentrantLock();
    static final Deque<Condition> waiters = new ArrayDeque<>();
    static long available = 0;          // start exhausted
    static volatile boolean allocated = false;

    static void allocate(long size) throws InterruptedException {
        lock.lock();
        try {
            long accumulated = 0;
            Condition moreMemory = lock.newCondition();
            waiters.addLast(moreMemory);
            while (accumulated < size) {
                moreMemory.await();          // <-- the blocking point
                long got = Math.min(available, size - accumulated);
                available -= got;
                accumulated += got;
            }
            waiters.remove(moreMemory);
        } finally {
            lock.unlock();
        }
        allocated = true;
    }

    static void deallocate(long size) {
        lock.lock();
        try {
            available += size;
            Condition first = waiters.peekFirst();
            if (first != null) first.signal();   // <-- the wakeup
        } finally {
            lock.unlock();
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("R1 START");
        Thread waiter = new Thread(() -> {
            try { allocate(1000); } catch (InterruptedException e) { /* ignore */ }
        }, "waiter");
        waiter.start();
        Thread.sleep(300);                 // let waiter reach await()
        deallocate(1000);                  // wake it
        waiter.join(10_000);
        if (waiter.isAlive() || !allocated) {
            System.out.println("R1 RESULT=HANG (await never woke; allocated=" + allocated + ")");
            System.out.flush();
            Runtime.getRuntime().halt(2);
        }
        System.out.println("R1 RESULT=PASS");
        System.out.flush();
    }
}
