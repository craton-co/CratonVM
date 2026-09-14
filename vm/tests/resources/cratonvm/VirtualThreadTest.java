package cratonvm;

/**
 * Session 25: Virtual Threads Integration.
 * Tests virtual thread creation, scheduling, isVirtual(), builder API,
 * and scaling to many concurrent virtual threads.
 *
 * Uses native helper methods because Thread.startVirtualThread() and
 * Thread.ofVirtual() are Java 21 APIs unavailable to javac -source 8.
 */
public class VirtualThreadTest implements Runnable {
    static volatile int counter = 0;

    // Native helpers implemented by our VM
    public static native Thread startVirtualThread(Runnable r);
    public static native boolean threadIsVirtual(Thread t);
    public static native Thread builderOfVirtualStart(Runnable r);
    public static native Thread builderOfPlatformStart(Runnable r);

    public void run() {
        synchronized (VirtualThreadTest.class) {
            counter++;
        }
    }

    // Test 1: Start a single virtual thread, join, verify work done
    public static int testSingleVirtualThread() {
        counter = 0;
        Thread t = startVirtualThread(new VirtualThreadTest());
        try { t.join(); } catch (Exception e) { return -1; }
        return counter; // expect 1
    }

    // Test 2: isVirtual() returns true for virtual threads
    public static int testIsVirtual() {
        Thread t = startVirtualThread(new VirtualThreadTest());
        boolean virt = threadIsVirtual(t);
        try { t.join(); } catch (Exception e) {}
        return virt ? 1 : 0; // expect 1
    }

    // Test 3: isVirtual() returns false for platform threads
    public static int testIsNotVirtualPlatform() {
        Thread t = new Thread(new VirtualThreadTest());
        boolean virt = threadIsVirtual(t);
        return virt ? 0 : 1; // expect 1 (not virtual)
    }

    // Test 4: Start 100 virtual threads, all increment counter
    public static int testHundredVirtualThreads() {
        counter = 0;
        Thread[] threads = new Thread[100];
        for (int i = 0; i < 100; i++) {
            threads[i] = startVirtualThread(new VirtualThreadTest());
        }
        for (int i = 0; i < 100; i++) {
            try { threads[i].join(); } catch (Exception e) { return -1; }
        }
        return counter; // expect 100
    }

    // Test 5: Start 1000 virtual threads (scaled-down from 10K for test speed)
    public static int testThousandVirtualThreads() {
        counter = 0;
        Thread[] threads = new Thread[1000];
        for (int i = 0; i < 1000; i++) {
            threads[i] = startVirtualThread(new VirtualThreadTest());
        }
        for (int i = 0; i < 1000; i++) {
            try { threads[i].join(); } catch (Exception e) { return -1; }
        }
        return counter; // expect 1000
    }

    // Test 6: Builder API — ofVirtual().start(runnable)
    public static int testBuilderVirtualStart() {
        counter = 0;
        Thread t = builderOfVirtualStart(new VirtualThreadTest());
        boolean virt = threadIsVirtual(t);
        try { t.join(); } catch (Exception e) { return -1; }
        if (!virt) return -2;
        return counter; // expect 1
    }

    // Test 7: Builder API — ofPlatform().start(runnable)
    public static int testBuilderPlatformStart() {
        counter = 0;
        Thread t = builderOfPlatformStart(new VirtualThreadTest());
        boolean virt = threadIsVirtual(t);
        try { t.join(); } catch (Exception e) { return -1; }
        if (virt) return -2;
        return counter; // expect 1
    }

    // Test 8: Virtual thread join guarantees happens-before
    public static int testVirtualJoinHappensBefore() {
        counter = 0;
        Thread t = startVirtualThread(new Runnable() {
            public void run() { counter = 42; }
        });
        try { t.join(); } catch (Exception e) { return -1; }
        return counter; // expect 42
    }

    // Test 9: Mix virtual and platform threads on same shared state
    public static int testMixedVirtualPlatform() {
        counter = 0;
        Thread vt1 = startVirtualThread(new VirtualThreadTest());
        Thread vt2 = startVirtualThread(new VirtualThreadTest());
        Thread pt1 = new Thread(new VirtualThreadTest());
        Thread pt2 = new Thread(new VirtualThreadTest());
        pt1.start();
        pt2.start();
        try {
            vt1.join();
            vt2.join();
            pt1.join();
            pt2.join();
        } catch (Exception e) { return -1; }
        return counter; // expect 4
    }

    // Test 10: Virtual thread with data dependency (write before start visible)
    public static int testVirtualStartHappensBefore() {
        counter = 99;
        Thread t = startVirtualThread(new Runnable() {
            public void run() {
                // counter should be 99 (written before start)
                if (counter == 99) {
                    counter = 100;
                }
            }
        });
        try { t.join(); } catch (Exception e) { return -1; }
        return counter; // expect 100
    }
}
