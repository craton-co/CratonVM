package cratonvm;

import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.locks.LockSupport;

/**
 * Session 24: Thread.interrupt() and Timed Waits — comprehensive tests.
 * Each static method returns an int: expected value on success.
 */
public class InterruptComplete {

    /** Busy-wait for approximately ms milliseconds. */
    private static void busyWait(long ms) {
        long end = System.nanoTime() + ms * 1_000_000L;
        while (System.nanoTime() < end) {
            Thread.yield();
        }
    }

    // -----------------------------------------------------------------------
    // 1. interrupt_sleeping_thread — interrupt a Thread.sleep()
    // -----------------------------------------------------------------------
    public static int interrupt_sleeping_thread() {
        AtomicInteger caught = new AtomicInteger(0);
        Thread t = new Thread(() -> {
            try {
                Thread.sleep(5000);
            } catch (InterruptedException e) {
                caught.set(1);
            }
        });
        t.start();
        busyWait(50);
        t.interrupt();
        try { t.join(); } catch (InterruptedException e) {}
        return caught.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 2. interrupt_waiting_thread — interrupt Object.wait()
    // -----------------------------------------------------------------------
    public static int interrupt_waiting_thread() {
        AtomicInteger caught = new AtomicInteger(0);
        Object lock = new Object();
        Thread t = new Thread(() -> {
            synchronized (lock) {
                try {
                    lock.wait();
                } catch (InterruptedException e) {
                    caught.set(1);
                }
            }
        });
        t.start();
        busyWait(50);
        t.interrupt();
        try { t.join(); } catch (InterruptedException e) {}
        return caught.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 3. interrupt_parked_thread — interrupt LockSupport.park()
    // -----------------------------------------------------------------------
    public static int interrupt_parked_thread() {
        AtomicInteger result = new AtomicInteger(0);
        Thread t = new Thread(() -> {
            LockSupport.park();
            // park returns on interrupt without exception; flag stays set
            if (Thread.currentThread().isInterrupted()) {
                result.set(1);
            }
        });
        t.start();
        busyWait(50);
        t.interrupt();
        try { t.join(); } catch (InterruptedException e) {}
        return result.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 4. is_interrupted_no_clear — isInterrupted() does NOT clear flag
    // -----------------------------------------------------------------------
    public static int is_interrupted_no_clear() {
        Thread.currentThread().interrupt();
        boolean first = Thread.currentThread().isInterrupted();
        boolean second = Thread.currentThread().isInterrupted();
        // Clear it so it doesn't affect other tests
        Thread.interrupted();
        return (first && second) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 5. interrupted_clears_flag — Thread.interrupted() clears flag
    // -----------------------------------------------------------------------
    public static int interrupted_clears_flag() {
        Thread.currentThread().interrupt();
        boolean first = Thread.interrupted(); // should be true, clears
        boolean second = Thread.interrupted(); // should be false
        return (first && !second) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 6. interrupt_before_sleep — flag set before sleep causes immediate throw
    //    (Uses Object.wait since Thread.sleep goes through JDK bytecode)
    // -----------------------------------------------------------------------
    public static int interrupt_before_sleep() {
        // Test: interrupt flag set before a blocking operation causes immediate throw
        Thread.currentThread().interrupt();
        Object lock = new Object();
        synchronized (lock) {
            try {
                lock.wait(5000);
                return 0; // should not reach
            } catch (InterruptedException e) {
                return 1; // expect 1
            }
        }
    }

    // -----------------------------------------------------------------------
    // 7. interrupt_before_wait — flag set before wait causes immediate throw
    // -----------------------------------------------------------------------
    public static int interrupt_before_wait() {
        Object lock = new Object();
        Thread.currentThread().interrupt();
        synchronized (lock) {
            try {
                lock.wait();
                return 0;
            } catch (InterruptedException e) {
                return 1; // expect 1
            }
        }
    }

    // -----------------------------------------------------------------------
    // 8. interrupt_before_park — flag set before park causes immediate return
    // -----------------------------------------------------------------------
    public static int interrupt_before_park() {
        Thread.currentThread().interrupt();
        LockSupport.park();
        boolean wasInterrupted = Thread.interrupted(); // clear it
        return wasInterrupted ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 9. timed_wait_normal — Object.wait(timeout) returns after timeout
    // -----------------------------------------------------------------------
    public static int timed_wait_normal() {
        Object lock = new Object();
        long start = System.currentTimeMillis();
        synchronized (lock) {
            try {
                lock.wait(50);
            } catch (InterruptedException e) {
                return 0;
            }
        }
        long elapsed = System.currentTimeMillis() - start;
        return (elapsed >= 30) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 10. timed_wait_interrupted — wait(timeout) interrupted before timeout
    // -----------------------------------------------------------------------
    public static int timed_wait_interrupted() {
        AtomicInteger caught = new AtomicInteger(0);
        Object lock = new Object();
        Thread t = new Thread(() -> {
            synchronized (lock) {
                try {
                    lock.wait(5000);
                } catch (InterruptedException e) {
                    caught.set(1);
                }
            }
        });
        t.start();
        busyWait(50);
        t.interrupt();
        try { t.join(); } catch (InterruptedException e) {}
        return caught.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 11. thread_interrupt_self — thread can interrupt itself
    // -----------------------------------------------------------------------
    public static int thread_interrupt_self() {
        Thread.currentThread().interrupt();
        boolean interrupted = Thread.interrupted();
        return interrupted ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 12. multiple_interrupts — multiple interrupt calls have same effect as one
    // -----------------------------------------------------------------------
    public static int multiple_interrupts() {
        Thread.currentThread().interrupt();
        Thread.currentThread().interrupt();
        Thread.currentThread().interrupt();
        boolean first = Thread.interrupted(); // clears
        boolean second = Thread.interrupted(); // already cleared
        return (first && !second) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 13. wait_notify_no_interrupt — normal wait/notify without interrupt
    // -----------------------------------------------------------------------
    public static int wait_notify_no_interrupt() {
        AtomicInteger result = new AtomicInteger(0);
        Object lock = new Object();
        Thread t = new Thread(() -> {
            synchronized (lock) {
                try {
                    lock.wait();
                    result.set(1); // woken by notify, not interrupt
                } catch (InterruptedException e) {
                    result.set(0);
                }
            }
        });
        t.start();
        busyWait(50);
        synchronized (lock) {
            lock.notify();
        }
        try { t.join(); } catch (InterruptedException e) {}
        return result.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 14. park_unpark_no_interrupt — normal park/unpark without interrupt
    // -----------------------------------------------------------------------
    public static int park_unpark_no_interrupt() {
        AtomicInteger result = new AtomicInteger(0);
        Thread t = new Thread(() -> {
            LockSupport.park();
            result.set(1);
        });
        t.start();
        busyWait(50);
        LockSupport.unpark(t);
        try { t.join(); } catch (InterruptedException e) {}
        return result.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 15. unpark_before_park — unpark before park = park returns immediately
    // -----------------------------------------------------------------------
    public static int unpark_before_park() {
        AtomicInteger result = new AtomicInteger(0);
        Thread t = new Thread(() -> {
            // Small delay so main thread can unpark first
            busyWait(50);
            LockSupport.park();
            result.set(1);
        });
        t.start();
        LockSupport.unpark(t); // unpark before park
        try { t.join(); } catch (InterruptedException e) {}
        return result.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 16. park_nanos_timeout — parkNanos returns after timeout
    // -----------------------------------------------------------------------
    public static int park_nanos_timeout() {
        long start = System.nanoTime();
        LockSupport.parkNanos(50_000_000L); // 50ms
        long elapsed = System.nanoTime() - start;
        return (elapsed >= 30_000_000L) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 17. sleep_nanos_interrupt — sleepNanos0 responds to interrupt
    // -----------------------------------------------------------------------
    public static int sleep_nanos_interrupt() {
        AtomicInteger caught = new AtomicInteger(0);
        Thread t = new Thread(() -> {
            try {
                Thread.sleep(5000);
            } catch (InterruptedException e) {
                caught.set(1);
            }
        });
        t.start();
        busyWait(50);
        t.interrupt();
        try { t.join(); } catch (InterruptedException e) {}
        return caught.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 18. interrupt_clears_on_exception — InterruptedException clears the flag
    // -----------------------------------------------------------------------
    public static int interrupt_clears_on_exception() {
        // Test: after catching InterruptedException from wait(), flag is cleared
        Thread.currentThread().interrupt();
        Object lock = new Object();
        synchronized (lock) {
            try {
                lock.wait(100);
                return 0;
            } catch (InterruptedException e) {
                // Flag should be cleared after InterruptedException
                boolean stillInterrupted = Thread.currentThread().isInterrupted();
                return stillInterrupted ? 0 : 1; // expect 1 (not interrupted)
            }
        }
    }

    // -----------------------------------------------------------------------
    // 19. wait_reacquires_monitor — after wait returns, thread holds monitor
    // -----------------------------------------------------------------------
    public static int wait_reacquires_monitor() {
        Object lock = new Object();
        AtomicInteger result = new AtomicInteger(0);
        synchronized (lock) {
            try {
                lock.wait(10); // short timed wait
            } catch (InterruptedException e) {
                return 0;
            }
            // If we're here, we re-acquired the monitor
            result.set(1);
        }
        return result.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 20. interrupt_during_timed_park — interrupt a parkNanos
    // -----------------------------------------------------------------------
    public static int interrupt_during_timed_park() {
        AtomicInteger result = new AtomicInteger(0);
        Thread t = new Thread(() -> {
            LockSupport.parkNanos(5_000_000_000L); // 5 seconds
            if (Thread.currentThread().isInterrupted()) {
                result.set(1);
            }
        });
        t.start();
        busyWait(50);
        t.interrupt();
        try { t.join(); } catch (InterruptedException e) {}
        return result.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 21. notify_all_wakes_waiters — notifyAll wakes multiple waiting threads
    // -----------------------------------------------------------------------
    public static int notify_all_wakes_waiters() {
        Object lock = new Object();
        AtomicInteger count = new AtomicInteger(0);
        Thread t1 = new Thread(() -> {
            synchronized (lock) {
                try { lock.wait(); } catch (InterruptedException e) {}
                count.incrementAndGet();
            }
        });
        Thread t2 = new Thread(() -> {
            synchronized (lock) {
                try { lock.wait(); } catch (InterruptedException e) {}
                count.incrementAndGet();
            }
        });
        t1.start();
        t2.start();
        busyWait(50);
        synchronized (lock) {
            lock.notifyAll();
        }
        try { t1.join(); } catch (InterruptedException e) {}
        try { t2.join(); } catch (InterruptedException e) {}
        return count.get(); // expect 2
    }

    // -----------------------------------------------------------------------
    // 22. park_after_interrupt_repeated — park keeps returning immediately
    //     while flag is set
    // -----------------------------------------------------------------------
    public static int park_after_interrupt_repeated() {
        Thread.currentThread().interrupt();
        LockSupport.park(); // returns immediately
        LockSupport.park(); // returns immediately again (flag still set)
        boolean still = Thread.interrupted(); // clear
        return still ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 23. sleep_zero_no_interrupt_check — sleep(0) doesn't throw even if interrupted
    // -----------------------------------------------------------------------
    public static int sleep_zero_no_interrupt_check() {
        Thread.currentThread().interrupt();
        try {
            Thread.sleep(0); // spec: sleep(0) is effectively a no-op or yield
        } catch (InterruptedException e) {
            Thread.interrupted(); // clear
            return 0;
        }
        Thread.interrupted(); // clear
        return 1; // expect 1
    }

    // -----------------------------------------------------------------------
    // 24. timed_join — Thread.join(millis) returns after timeout
    // -----------------------------------------------------------------------
    public static int timed_join() {
        Thread t = new Thread(() -> {
            busyWait(5000);
        });
        t.start();
        long start = System.currentTimeMillis();
        try {
            t.join(50); // should return after ~50ms, not wait for thread to die
        } catch (InterruptedException e) {
            t.interrupt();
            return 0;
        }
        long elapsed = System.currentTimeMillis() - start;
        t.interrupt(); // clean up: wake the sleeping thread
        try { t.join(); } catch (InterruptedException e) {}
        return (elapsed < 2000) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 25. interrupt_not_alive — interrupting a non-started or dead thread is a no-op
    // -----------------------------------------------------------------------
    public static int interrupt_not_alive() {
        Thread t = new Thread(() -> {});
        t.start();
        try { t.join(); } catch (InterruptedException e) {}
        // Thread is dead now. interrupt should be a no-op (no exception)
        t.interrupt();
        return 1; // expect 1 (no crash)
    }

    // -----------------------------------------------------------------------
    // 26. wait_with_notify_all_and_interrupt — one thread notified, one interrupted
    // -----------------------------------------------------------------------
    public static int wait_with_notify_all_and_interrupt() {
        Object lock = new Object();
        AtomicInteger notified = new AtomicInteger(0);
        AtomicInteger interrupted = new AtomicInteger(0);
        Thread t1 = new Thread(() -> {
            synchronized (lock) {
                try {
                    lock.wait();
                    notified.incrementAndGet();
                } catch (InterruptedException e) {
                    interrupted.incrementAndGet();
                }
            }
        });
        Thread t2 = new Thread(() -> {
            synchronized (lock) {
                try {
                    lock.wait();
                    notified.incrementAndGet();
                } catch (InterruptedException e) {
                    interrupted.incrementAndGet();
                }
            }
        });
        t1.start();
        t2.start();
        busyWait(50);
        // Notify one, interrupt the other
        synchronized (lock) {
            lock.notify(); // wakes one
        }
        busyWait(20);
        // Interrupt whichever is still waiting
        t1.interrupt();
        t2.interrupt();
        try { t1.join(); } catch (InterruptedException e) {}
        try { t2.join(); } catch (InterruptedException e) {}
        // Both should have completed (notified + interrupted >= 2)
        return (notified.get() + interrupted.get() >= 2) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 27. concurrent_interrupt_and_join — interrupt + join coordination
    // -----------------------------------------------------------------------
    public static int concurrent_interrupt_and_join() {
        AtomicInteger result = new AtomicInteger(0);
        Thread worker = new Thread(() -> {
            try {
                Thread.sleep(5000);
            } catch (InterruptedException e) {
                result.set(1);
            }
        });
        worker.start();
        busyWait(30);
        worker.interrupt();
        try { worker.join(); } catch (InterruptedException e) {}
        return result.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 28. park_unpark_multiple_threads — multiple threads park/unpark correctly
    // -----------------------------------------------------------------------
    public static int park_unpark_multiple_threads() {
        AtomicInteger count = new AtomicInteger(0);
        Thread[] threads = new Thread[5];
        for (int i = 0; i < 5; i++) {
            threads[i] = new Thread(() -> {
                LockSupport.park();
                count.incrementAndGet();
            });
            threads[i].start();
        }
        busyWait(50);
        for (Thread t : threads) {
            LockSupport.unpark(t);
        }
        for (Thread t : threads) {
            try { t.join(); } catch (InterruptedException e) {}
        }
        return count.get(); // expect 5
    }

    // -----------------------------------------------------------------------
    // 29. wait_interrupt_reacquires_monitor — after InterruptedException,
    //     thread still holds the monitor
    // -----------------------------------------------------------------------
    public static int wait_interrupt_reacquires_monitor() {
        Object lock = new Object();
        AtomicInteger result = new AtomicInteger(0);
        Thread t = new Thread(() -> {
            synchronized (lock) {
                try {
                    lock.wait();
                } catch (InterruptedException e) {
                    // Even on interrupt, we should still hold the monitor
                    // Verify by successfully calling notify (requires ownership)
                    lock.notify();
                    result.set(1);
                }
            }
        });
        t.start();
        busyWait(50);
        t.interrupt();
        try { t.join(); } catch (InterruptedException e) {}
        return result.get(); // expect 1
    }

    // -----------------------------------------------------------------------
    // 30. interrupt_flag_survives_park — park doesn't clear interrupt flag
    // -----------------------------------------------------------------------
    public static int interrupt_flag_survives_park() {
        Thread.currentThread().interrupt();
        LockSupport.park(); // returns immediately, flag still set
        boolean after1 = Thread.currentThread().isInterrupted();
        LockSupport.park(); // returns immediately again
        boolean after2 = Thread.currentThread().isInterrupted();
        Thread.interrupted(); // clean up
        return (after1 && after2) ? 1 : 0; // expect 1
    }
}
