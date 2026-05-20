package cratonvm;

/**
 * Session 23: Java Memory Model Compliance tests.
 *
 * Tests volatile visibility, synchronized happens-before,
 * Thread.start()/join() ordering, double-checked locking,
 * and a simplified Dekker's algorithm.
 */
public class MemoryModelTest {

    // ---------------------------------------------------------------
    // Test 1: Volatile write in one thread is visible after join
    // ---------------------------------------------------------------
    static volatile int volatileFlag = 0;

    public static int testVolatileVisibility() {
        volatileFlag = 0;
        Thread t = new Thread(new Runnable() {
            public void run() {
                volatileFlag = 42;
            }
        });
        t.start();
        try { t.join(); } catch (Exception e) { return 0; }
        return volatileFlag == 42 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 2: Non-volatile write visible through join() HB edge
    // ---------------------------------------------------------------
    static int joinVisible = 0;

    public static int testJoinHappensBefore() {
        joinVisible = 0;
        Thread t = new Thread(new Runnable() {
            public void run() {
                joinVisible = 99;
            }
        });
        t.start();
        try { t.join(); } catch (Exception e) { return 0; }
        // After join(), all writes from the child thread should be visible
        return joinVisible == 99 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 3: Thread.start() happens-before first action in thread
    // ---------------------------------------------------------------
    static int startVisible = 0;
    static volatile int startDone = 0;

    public static int testStartHappensBefore() {
        startVisible = 77;
        startDone = 0;
        final int[] result = new int[1];
        Thread t = new Thread(new Runnable() {
            public void run() {
                // Thread.start() HB: should see writes made before start()
                result[0] = startVisible;
                startDone = 1;
            }
        });
        t.start();
        try { t.join(); } catch (Exception e) { return 0; }
        return result[0] == 77 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 4: Synchronized block establishes happens-before
    // ---------------------------------------------------------------
    static final Object lock = new Object();
    static int sharedData = 0;

    public static int testSynchronizedHappensBefore() {
        sharedData = 0;
        Thread t = new Thread(new Runnable() {
            public void run() {
                synchronized (lock) {
                    sharedData = 123;
                }
            }
        });
        t.start();
        try { t.join(); } catch (Exception e) { return 0; }
        // After join, sharedData should be 123
        int val;
        synchronized (lock) {
            val = sharedData;
        }
        return val == 123 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 5: Double-checked locking with volatile
    // ---------------------------------------------------------------
    static volatile Object singleton = null;
    static final Object singletonLock = new Object();
    static int initCount = 0;

    public static int testDoubleCheckedLocking() {
        singleton = null;
        initCount = 0;

        Runnable init = new Runnable() {
            public void run() {
                if (singleton == null) {
                    synchronized (singletonLock) {
                        if (singleton == null) {
                            synchronized (singletonLock) {
                                initCount++;
                            }
                            singleton = new Object();
                        }
                    }
                }
            }
        };

        Thread t1 = new Thread(init);
        Thread t2 = new Thread(init);
        t1.start();
        t2.start();
        try {
            t1.join();
            t2.join();
        } catch (Exception e) { return 0; }

        // Singleton should be initialized exactly once
        if (singleton == null) return 0;
        if (initCount != 1) return 0;
        return 1;
    }

    // ---------------------------------------------------------------
    // Test 6: Dekker-style mutual exclusion with volatile flags
    // ---------------------------------------------------------------
    static volatile int flag0 = 0;
    static volatile int flag1 = 0;
    static volatile int turn = 0;
    static int criticalSection = 0;  // incremented inside critical section
    static int violations = 0;       // counts simultaneous entries

    public static int testDekkerMutualExclusion() {
        flag0 = 0;
        flag1 = 0;
        turn = 0;
        criticalSection = 0;
        violations = 0;

        final int ITERS = 50;

        Thread t0 = new Thread(new Runnable() {
            public void run() {
                for (int i = 0; i < ITERS; i++) {
                    // Entry protocol for thread 0
                    flag0 = 1;
                    while (flag1 == 1) {
                        if (turn != 0) {
                            flag0 = 0;
                            while (turn != 0) { /* spin */ }
                            flag0 = 1;
                        }
                    }
                    // Critical section
                    criticalSection++;
                    if (criticalSection != 1) violations++;
                    criticalSection--;
                    // Exit protocol
                    turn = 1;
                    flag0 = 0;
                }
            }
        });

        Thread t1 = new Thread(new Runnable() {
            public void run() {
                for (int i = 0; i < ITERS; i++) {
                    // Entry protocol for thread 1
                    flag1 = 1;
                    while (flag0 == 1) {
                        if (turn != 1) {
                            flag1 = 0;
                            while (turn != 1) { /* spin */ }
                            flag1 = 1;
                        }
                    }
                    // Critical section
                    criticalSection++;
                    if (criticalSection != 1) violations++;
                    criticalSection--;
                    // Exit protocol
                    turn = 0;
                    flag1 = 0;
                }
            }
        });

        t0.start();
        t1.start();
        try {
            t0.join();
            t1.join();
        } catch (Exception e) { return 0; }

        // No violations should have occurred
        return violations == 0 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 7: Volatile counter incremented by multiple threads
    // ---------------------------------------------------------------
    static volatile int volatileCounter = 0;

    public static int testVolatileCounter() {
        volatileCounter = 0;
        final int ITERS = 100;

        Thread t1 = new Thread(new Runnable() {
            public void run() {
                for (int i = 0; i < ITERS; i++) {
                    synchronized (lock) {
                        volatileCounter++;
                    }
                }
            }
        });
        Thread t2 = new Thread(new Runnable() {
            public void run() {
                for (int i = 0; i < ITERS; i++) {
                    synchronized (lock) {
                        volatileCounter++;
                    }
                }
            }
        });
        t1.start();
        t2.start();
        try {
            t1.join();
            t2.join();
        } catch (Exception e) { return 0; }

        return volatileCounter == ITERS * 2 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 8: Monitor wait/notify ordering
    // ---------------------------------------------------------------
    static volatile int produced = 0;
    static volatile int consumed = 0;

    public static int testMonitorWaitNotify() {
        produced = 0;
        consumed = 0;
        final Object mon = new Object();

        Thread producer = new Thread(new Runnable() {
            public void run() {
                synchronized (mon) {
                    produced = 1;
                    mon.notify();
                }
            }
        });

        Thread consumer = new Thread(new Runnable() {
            public void run() {
                synchronized (mon) {
                    while (produced == 0) {
                        try { mon.wait(); } catch (Exception e) {}
                    }
                    consumed = produced;
                }
            }
        });

        // Start consumer first so it waits
        consumer.start();
        // Small delay to let consumer enter wait
        try { Thread.sleep(50); } catch (Exception e) {}
        producer.start();

        try {
            producer.join();
            consumer.join();
        } catch (Exception e) { return 0; }

        return consumed == 1 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 9: Multiple threads with synchronized increments
    // ---------------------------------------------------------------
    static int syncCounter = 0;

    public static int testSynchronizedCounter() {
        syncCounter = 0;
        final int ITERS = 200;
        final Object cntLock = new Object();

        Thread t1 = new Thread(new Runnable() {
            public void run() {
                for (int i = 0; i < ITERS; i++) {
                    synchronized (cntLock) {
                        syncCounter++;
                    }
                }
            }
        });
        Thread t2 = new Thread(new Runnable() {
            public void run() {
                for (int i = 0; i < ITERS; i++) {
                    synchronized (cntLock) {
                        syncCounter++;
                    }
                }
            }
        });
        t1.start();
        t2.start();
        try {
            t1.join();
            t2.join();
        } catch (Exception e) { return 0; }

        return syncCounter == ITERS * 2 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 10: Volatile store-load ordering (flag + data pattern)
    // ---------------------------------------------------------------
    static int data = 0;
    static volatile int ready = 0;

    public static int testVolatileStoreLoad() {
        data = 0;
        ready = 0;
        final int[] observed = new int[1];

        Thread writer = new Thread(new Runnable() {
            public void run() {
                data = 42;     // plain write
                ready = 1;     // volatile write (release)
            }
        });

        Thread reader = new Thread(new Runnable() {
            public void run() {
                while (ready == 0) { /* spin on volatile */ }
                observed[0] = data;  // should see 42 due to volatile HB
            }
        });

        writer.start();
        reader.start();
        try {
            writer.join();
            reader.join();
        } catch (Exception e) { return 0; }

        return observed[0] == 42 ? 1 : 0;
    }

    // --- Entry points ---
    public static void runVolatileVisibility()    { Util.tempPrint(testVolatileVisibility()); }
    public static void runJoinHB()                { Util.tempPrint(testJoinHappensBefore()); }
    public static void runStartHB()               { Util.tempPrint(testStartHappensBefore()); }
    public static void runSyncHB()                { Util.tempPrint(testSynchronizedHappensBefore()); }
    public static void runDCL()                   { Util.tempPrint(testDoubleCheckedLocking()); }
    public static void runDekker()                { Util.tempPrint(testDekkerMutualExclusion()); }
    public static void runVolatileCounter()       { Util.tempPrint(testVolatileCounter()); }
    public static void runWaitNotify()            { Util.tempPrint(testMonitorWaitNotify()); }
    public static void runSyncCounter()           { Util.tempPrint(testSynchronizedCounter()); }
    public static void runStoreLoad()             { Util.tempPrint(testVolatileStoreLoad()); }
}
