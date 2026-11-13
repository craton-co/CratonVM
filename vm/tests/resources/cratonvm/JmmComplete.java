// JAVA21+
package cratonvm;

import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;

/**
 * Session 23: Java Memory Model Compliance.
 * Volatile fields, synchronized blocks, Thread.start/join, atomic operations,
 * happens-before semantics, double-checked locking.
 */
public class JmmComplete {

    // ---- Test 1: Thread.start() and join() basic ----
    static volatile int threadResult = 0;

    public static int testThreadStartJoin() {
        threadResult = 0;
        Thread t = new Thread(() -> {
            threadResult = 42;
        });
        t.start();
        try { t.join(); } catch (InterruptedException e) { return 0; }
        return threadResult; // 42
    }

    // ---- Test 2: Multiple threads with join ----
    static volatile int counter2 = 0;

    public static int testMultipleThreadsJoin() {
        counter2 = 0;
        Thread t1 = new Thread(() -> {
            synchronized (JmmComplete.class) { counter2 += 10; }
        });
        Thread t2 = new Thread(() -> {
            synchronized (JmmComplete.class) { counter2 += 20; }
        });
        t1.start();
        t2.start();
        try { t1.join(); t2.join(); } catch (InterruptedException e) { return 0; }
        return counter2; // 30
    }

    // ---- Test 3: Volatile field visibility ----
    static volatile boolean flag3 = false;
    static int data3 = 0;

    public static int testVolatileVisibility() {
        flag3 = false;
        data3 = 0;
        Thread writer = new Thread(() -> {
            data3 = 99;
            flag3 = true; // volatile write publishes data3
        });
        writer.start();
        try { writer.join(); } catch (InterruptedException e) { return 0; }
        // After join, we should see both flag3=true and data3=99
        return (flag3 && data3 == 99) ? 1 : 0; // 1
    }

    // ---- Test 4: Synchronized block mutual exclusion ----
    static int sharedCounter4 = 0;
    static final Object lock4 = new Object();

    public static int testSynchronizedMutex() {
        sharedCounter4 = 0;
        Thread[] threads = new Thread[5];
        for (int i = 0; i < 5; i++) {
            threads[i] = new Thread(() -> {
                for (int j = 0; j < 100; j++) {
                    synchronized (lock4) {
                        sharedCounter4++;
                    }
                }
            });
        }
        for (Thread t : threads) t.start();
        try {
            for (Thread t : threads) t.join();
        } catch (InterruptedException e) { return 0; }
        return sharedCounter4; // 500
    }

    // ---- Test 5: Thread.currentThread() ----
    public static int testCurrentThread() {
        Thread t = Thread.currentThread();
        return (t != null) ? 1 : 0; // 1
    }

    // ---- Test 6: Thread.sleep() basic ----
    public static int testThreadSleep() {
        long start = System.currentTimeMillis();
        try {
            Thread.sleep(10);
        } catch (InterruptedException e) {
            return 0;
        }
        long elapsed = System.currentTimeMillis() - start;
        return (elapsed >= 5) ? 1 : 0; // 1 (at least some time passed)
    }

    // ---- Test 7: AtomicInteger basic ops ----
    public static int testAtomicIntegerBasic() {
        AtomicInteger ai = new AtomicInteger(0);
        ai.set(10);
        int val = ai.get();
        return val; // 10
    }

    // ---- Test 8: AtomicInteger compareAndSet ----
    public static int testAtomicIntegerCAS() {
        AtomicInteger ai = new AtomicInteger(5);
        boolean ok = ai.compareAndSet(5, 10);
        boolean fail = ai.compareAndSet(5, 20); // should fail, value is now 10
        return (ok && !fail && ai.get() == 10) ? 1 : 0; // 1
    }

    // ---- Test 9: AtomicInteger incrementAndGet ----
    public static int testAtomicIntegerIncrement() {
        AtomicInteger ai = new AtomicInteger(0);
        int r1 = ai.incrementAndGet(); // 1
        int r2 = ai.incrementAndGet(); // 2
        int r3 = ai.incrementAndGet(); // 3
        return r1 + r2 + r3; // 6
    }

    // ---- Test 10: AtomicInteger getAndAdd ----
    public static int testAtomicIntegerGetAndAdd() {
        AtomicInteger ai = new AtomicInteger(10);
        int old = ai.getAndAdd(5);
        return old + ai.get(); // 10 + 15 = 25
    }

    // ---- Test 11: AtomicBoolean ----
    public static int testAtomicBoolean() {
        AtomicBoolean ab = new AtomicBoolean(false);
        ab.set(true);
        boolean val = ab.get();
        boolean swapped = ab.compareAndSet(true, false);
        return (val && swapped && !ab.get()) ? 1 : 0; // 1
    }

    // ---- Test 12: AtomicLong ----
    public static int testAtomicLong() {
        AtomicLong al = new AtomicLong(0L);
        al.set(100L);
        long val = al.get();
        al.incrementAndGet();
        return (val == 100L && al.get() == 101L) ? 1 : 0; // 1
    }

    // ---- Test 13: Synchronized method ----
    static int syncMethodCounter = 0;

    public static synchronized void incrementSync() {
        syncMethodCounter++;
    }

    public static int testSynchronizedMethod() {
        syncMethodCounter = 0;
        Thread[] threads = new Thread[3];
        for (int i = 0; i < 3; i++) {
            threads[i] = new Thread(() -> {
                for (int j = 0; j < 50; j++) {
                    incrementSync();
                }
            });
        }
        for (Thread t : threads) t.start();
        try {
            for (Thread t : threads) t.join();
        } catch (InterruptedException e) { return 0; }
        return syncMethodCounter; // 150
    }

    // ---- Test 14: Thread returning value via shared field ----
    static volatile int threadSum14 = 0;

    public static int testThreadComputation() {
        threadSum14 = 0;
        Thread t = new Thread(() -> {
            int sum = 0;
            for (int i = 1; i <= 10; i++) sum += i;
            threadSum14 = sum;
        });
        t.start();
        try { t.join(); } catch (InterruptedException e) { return 0; }
        return threadSum14; // 55
    }

    // ---- Test 15: AtomicInteger concurrent increment ----
    public static int testAtomicConcurrentIncrement() {
        AtomicInteger ai = new AtomicInteger(0);
        Thread[] threads = new Thread[4];
        for (int i = 0; i < 4; i++) {
            threads[i] = new Thread(() -> {
                for (int j = 0; j < 25; j++) {
                    ai.incrementAndGet();
                }
            });
        }
        for (Thread t : threads) t.start();
        try {
            for (Thread t : threads) t.join();
        } catch (InterruptedException e) { return 0; }
        return ai.get(); // 100
    }

    // ---- Test 16: Double-checked locking pattern ----
    static volatile Object singleton = null;
    static final Object singletonLock = new Object();

    static Object getSingleton() {
        if (singleton == null) {
            synchronized (singletonLock) {
                if (singleton == null) {
                    singleton = new Object();
                }
            }
        }
        return singleton;
    }

    public static int testDoubleCheckedLocking() {
        singleton = null;
        Object s1 = getSingleton();
        Object s2 = getSingleton();
        return (s1 != null && s1 == s2) ? 1 : 0; // 1
    }

    // ---- Test 17: Wait and notify ----
    static volatile boolean ready17 = false;
    static final Object monitor17 = new Object();

    public static int testWaitNotify() {
        ready17 = false;
        Thread producer = new Thread(() -> {
            try { Thread.sleep(10); } catch (InterruptedException e) {}
            synchronized (monitor17) {
                ready17 = true;
                monitor17.notify();
            }
        });
        producer.start();
        synchronized (monitor17) {
            while (!ready17) {
                try { monitor17.wait(1000); } catch (InterruptedException e) { return 0; }
            }
        }
        try { producer.join(); } catch (InterruptedException e) {}
        return ready17 ? 1 : 0; // 1
    }

    // ---- Test 18: Thread.getName() ----
    public static int testThreadName() {
        Thread t = new Thread("TestThread-42");
        String name = t.getName();
        return "TestThread-42".equals(name) ? 1 : 0; // 1
    }

    // ---- Test 19: Thread.isAlive() ----
    public static int testThreadIsAlive() {
        Thread t = new Thread(() -> {
            try { Thread.sleep(50); } catch (InterruptedException e) {}
        });
        boolean beforeStart = t.isAlive();
        t.start();
        boolean afterStart = t.isAlive(); // might be true
        try { t.join(); } catch (InterruptedException e) {}
        boolean afterJoin = t.isAlive();
        return (!beforeStart && !afterJoin) ? 1 : 0; // 1
    }

    // ---- Test 20: Volatile write-read ordering across threads ----
    static volatile int x20 = 0;
    static volatile int y20 = 0;

    public static int testVolatileOrdering() {
        x20 = 0; y20 = 0;
        Thread writer = new Thread(() -> {
            x20 = 1;
            y20 = 1;
        });
        writer.start();
        try { writer.join(); } catch (InterruptedException e) { return 0; }
        // After join, both should be visible
        return (x20 == 1 && y20 == 1) ? 1 : 0; // 1
    }

    // ---- Test 21: AtomicReference ----
    public static int testAtomicReference() {
        AtomicReference<String> ref = new AtomicReference<>("initial");
        String old = ref.get();
        ref.set("updated");
        boolean cas = ref.compareAndSet("updated", "final");
        return ("initial".equals(old) && cas && "final".equals(ref.get())) ? 1 : 0; // 1
    }

    // ---- Test 22: Reentrant synchronization ----
    static int reentrantCount = 0;

    public static int testReentrantSync() {
        reentrantCount = 0;
        Object lock = new Object();
        synchronized (lock) {
            reentrantCount++;
            synchronized (lock) { // same lock, re-entrant
                reentrantCount++;
                synchronized (lock) {
                    reentrantCount++;
                }
            }
        }
        return reentrantCount; // 3
    }

    // ---- Test 23: Thread with Runnable ----
    static volatile int runnableResult = 0;

    public static int testThreadWithRunnable() {
        runnableResult = 0;
        Runnable task = () -> runnableResult = 77;
        Thread t = new Thread(task);
        t.start();
        try { t.join(); } catch (InterruptedException e) { return 0; }
        return runnableResult; // 77
    }

    // ---- Test 24: AtomicInteger decrementAndGet ----
    public static int testAtomicDecrement() {
        AtomicInteger ai = new AtomicInteger(10);
        int r1 = ai.decrementAndGet(); // 9
        int r2 = ai.decrementAndGet(); // 8
        return r1 + r2; // 17
    }

    // ---- Test 25: System.nanoTime() monotonic ----
    public static int testNanoTimeMonotonic() {
        long t1 = System.nanoTime();
        // Do some work
        int sum = 0;
        for (int i = 0; i < 1000; i++) sum += i;
        long t2 = System.nanoTime();
        return (t2 >= t1) ? 1 : 0; // 1
    }

    // ---- Test 26: Volatile array element ----
    static volatile int[] volatileArr = new int[3];

    public static int testVolatileArray() {
        volatileArr = new int[]{10, 20, 30};
        Thread t = new Thread(() -> {
            volatileArr[1] = 99;
        });
        t.start();
        try { t.join(); } catch (InterruptedException e) { return 0; }
        return volatileArr[1]; // 99
    }

    // ---- Test 27: AtomicInteger getAndSet ----
    public static int testAtomicGetAndSet() {
        AtomicInteger ai = new AtomicInteger(42);
        int old = ai.getAndSet(100);
        return old + ai.get(); // 42 + 100 = 142
    }

    // ---- Test 28: Thread.join() with timeout ----
    public static int testJoinTimeout() {
        Thread t = new Thread(() -> {
            try { Thread.sleep(5); } catch (InterruptedException e) {}
        });
        t.start();
        try {
            t.join(2000); // join with generous timeout
        } catch (InterruptedException e) { return 0; }
        return (!t.isAlive()) ? 1 : 0; // 1 (thread should have finished)
    }

    // ---- Test 29: Synchronized block with return value ----
    public static int testSyncBlockReturn() {
        Object lock = new Object();
        int result;
        synchronized (lock) {
            result = 42;
        }
        return result; // 42
    }

    // ---- Test 30: Thread count check ----
    public static int testThreadStartJoinMultiple() {
        AtomicInteger total = new AtomicInteger(0);
        Thread[] threads = new Thread[10];
        for (int i = 0; i < 10; i++) {
            final int val = i + 1;
            threads[i] = new Thread(() -> total.addAndGet(val));
        }
        for (Thread t : threads) t.start();
        try {
            for (Thread t : threads) t.join();
        } catch (InterruptedException e) { return 0; }
        return total.get(); // 1+2+...+10 = 55
    }
}
