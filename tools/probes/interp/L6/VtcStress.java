// Virtual-thread continuations (JDK `VirtualThread` on its own `ForkJoinPool`
// carriers): behaviour rows that hold on HotSpot 25 whatever the timing, so the
// output is a byte-for-byte comparison. Each row has its own deadline and prints
// TIMEOUT instead of hanging.
//
//   javac -d /tmp/vtc tools/probes/interp/L6/VtcStress.java
//   java     -cp /tmp/vtc VtcStress
//   cratonvm -cp /tmp/vtc VtcStress
//   cratonvm --compatible -cp /tmp/vtc VtcStress
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

public class VtcStress {
    static void row(String name, Object value) {
        System.out.println(name + ": " + value);
    }

    static boolean joinAll(List<Thread> ts, long millis) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(millis);
        for (Thread t : ts) {
            long left = deadline - System.nanoTime();
            if (left <= 0 || !t.join(java.time.Duration.ofNanos(left))) {
                return false;
            }
        }
        return true;
    }

    static String mask(String s) {
        return s.replaceAll("\\d+", "N");
    }

    public static void main(String[] args) throws Exception {
        // 1. many sleepers
        {
            AtomicInteger done = new AtomicInteger();
            List<Thread> ts = new ArrayList<>();
            for (int i = 0; i < 10_000; i++) {
                ts.add(Thread.ofVirtual().start(() -> {
                    try {
                        Thread.sleep(10);
                    } catch (InterruptedException e) {
                        throw new RuntimeException(e);
                    }
                    done.incrementAndGet();
                }));
            }
            row("sleepers", joinAll(ts, 120_000) ? done.get() : "TIMEOUT " + done.get());
        }
        // 2. park/unpark ping-pong
        {
            final int rounds = 20_000;
            AtomicInteger turn = new AtomicInteger();
            Thread[] pair = new Thread[2];
            for (int k = 0; k < 2; k++) {
                final int me = k;
                pair[k] = Thread.ofVirtual().unstarted(() -> {
                    for (int r = 0; r < rounds; r++) {
                        while (turn.get() % 2 != me) {
                            LockSupport.park();
                        }
                        turn.incrementAndGet();
                        LockSupport.unpark(pair[1 - me]);
                    }
                });
            }
            pair[0].start();
            pair[1].start();
            row("pingpong", joinAll(List.of(pair), 120_000) ? turn.get() : "TIMEOUT " + turn.get());
        }
        // 3. more waiters in Object.wait than carriers
        {
            final Object lock = new Object();
            final int n = Runtime.getRuntime().availableProcessors() * 4;
            AtomicInteger waiting = new AtomicInteger();
            AtomicInteger woke = new AtomicInteger();
            boolean[] go = new boolean[1];
            List<Thread> ts = new ArrayList<>();
            for (int i = 0; i < n; i++) {
                ts.add(Thread.ofVirtual().start(() -> {
                    synchronized (lock) {
                        waiting.incrementAndGet();
                        while (!go[0]) {
                            try {
                                lock.wait();
                            } catch (InterruptedException e) {
                                return;
                            }
                        }
                        woke.incrementAndGet();
                    }
                }));
            }
            // A virtual thread (not main) does the notify, so it needs a carrier.
            Thread notifier = Thread.ofVirtual().start(() -> {
                long deadline = System.nanoTime() + 60_000_000_000L;
                while (waiting.get() < n && System.nanoTime() < deadline) {
                    Thread.onSpinWait();
                    Thread.yield();
                }
                synchronized (lock) {
                    go[0] = true;
                    lock.notifyAll();
                }
            });
            ts.add(notifier);
            boolean ok = joinAll(ts, 90_000);
            row("wait-notify", ok ? woke.get() == n : "TIMEOUT waiting=" + waiting.get() + " woke=" + woke.get());
        }
        // 4. ThreadLocal isolation across unmounts
        {
            ThreadLocal<Integer> tl = new ThreadLocal<>();
            AtomicInteger bad = new AtomicInteger();
            List<Thread> ts = new ArrayList<>();
            for (int i = 0; i < 200; i++) {
                final int v = i;
                ts.add(Thread.ofVirtual().start(() -> {
                    tl.set(v);
                    for (int k = 0; k < 20; k++) {
                        try {
                            Thread.sleep(1);
                        } catch (InterruptedException e) {
                            return;
                        }
                        if (tl.get() != v) {
                            bad.incrementAndGet();
                        }
                    }
                }));
            }
            row("threadlocal", joinAll(ts, 60_000) ? "bad=" + bad.get() : "TIMEOUT");
        }
        // 5. interrupts
        {
            CountDownLatch in = new CountDownLatch(1);
            String[] out = new String[2];
            Thread s = Thread.ofVirtual().start(() -> {
                in.countDown();
                try {
                    Thread.sleep(60_000);
                    out[0] = "slept";
                } catch (InterruptedException e) {
                    out[0] = "IE " + e.getMessage() + " flag=" + Thread.currentThread().isInterrupted();
                }
            });
            in.await();
            while (s.getState() != Thread.State.TIMED_WAITING) {
                Thread.sleep(1);
            }
            s.interrupt();
            s.join(30_000);
            row("interrupt-sleep", out[0]);
            CountDownLatch in2 = new CountDownLatch(1);
            Thread p = Thread.ofVirtual().start(() -> {
                in2.countDown();
                LockSupport.park();
                out[1] = "parked-returned flag=" + Thread.currentThread().isInterrupted();
            });
            in2.await();
            while (p.getState() != Thread.State.WAITING) {
                Thread.sleep(1);
            }
            p.interrupt();
            p.join(30_000);
            row("interrupt-park", out[1]);
        }
        // 6. identity
        {
            String[] out = new String[4];
            Thread t = Thread.ofVirtual().name("idv").start(() -> {
                Thread me = Thread.currentThread();
                out[0] = me.isVirtual() + " " + me.getName() + " " + me.getClass().getName();
                out[1] = mask(me.toString());
                out[2] = String.valueOf(me.isDaemon());
                out[3] = String.valueOf(me.getThreadGroup() == null ? "null" : me.getThreadGroup().getName());
            });
            t.join();
            row("identity", out[0]);
            row("toString", out[1]);
            row("daemon", out[2]);
            row("group", out[3]);
            row("after", mask(t.toString()));
        }
        // 7. executor
        {
            try (ExecutorService ex = Executors.newVirtualThreadPerTaskExecutor()) {
                List<Future<Integer>> fs = new ArrayList<>();
                for (int i = 0; i < 1000; i++) {
                    final int v = i;
                    fs.add(ex.submit(() -> {
                        Thread.sleep(1);
                        return v * 2;
                    }));
                }
                long sum = 0;
                for (Future<Integer> f : fs) {
                    sum += f.get(60, TimeUnit.SECONDS);
                }
                row("executor", sum);
            }
        }
        // 8. ReentrantLock contention
        {
            ReentrantLock lock = new ReentrantLock();
            AtomicLong counter = new AtomicLong();
            long[] plain = new long[1];
            List<Thread> ts = new ArrayList<>();
            for (int i = 0; i < 64; i++) {
                ts.add(Thread.ofVirtual().start(() -> {
                    for (int k = 0; k < 500; k++) {
                        lock.lock();
                        try {
                            plain[0]++;
                            if ((k & 63) == 0) {
                                Thread.yield();
                            }
                        } finally {
                            lock.unlock();
                        }
                        counter.incrementAndGet();
                    }
                }));
            }
            row("reentrantlock", joinAll(ts, 120_000) ? plain[0] + " " + counter.get() : "TIMEOUT");
        }
        // 9. synchronized contention
        {
            Object m = new Object();
            long[] plain = new long[1];
            List<Thread> ts = new ArrayList<>();
            for (int i = 0; i < 64; i++) {
                ts.add(Thread.ofVirtual().start(() -> {
                    for (int k = 0; k < 500; k++) {
                        synchronized (m) {
                            plain[0]++;
                        }
                    }
                }));
            }
            row("synchronized", joinAll(ts, 120_000) ? plain[0] : "TIMEOUT");
        }
        // 10. stack of a parked virtual thread, and states
        {
            CountDownLatch in = new CountDownLatch(1);
            CountDownLatch release = new CountDownLatch(1);
            Thread t = Thread.ofVirtual().unstarted(() -> {
                in.countDown();
                try {
                    release.await();
                } catch (InterruptedException e) {
                }
            });
            row("state-new", t.getState());
            t.start();
            in.await();
            while (t.getState() != Thread.State.WAITING) {
                Thread.sleep(1);
            }
            row("state-parked", t.getState());
            StringBuilder sb = new StringBuilder();
            for (StackTraceElement e : t.getStackTrace()) {
                sb.append(e.getClassName()).append('.').append(e.getMethodName()).append(' ');
            }
            row("parked-stack", sb.toString().trim());
            release.countDown();
            t.join();
            row("state-done", t.getState());
            row("stack-done", t.getStackTrace().length);
        }
        // 11. exception in a virtual thread
        {
            String[] out = new String[1];
            Thread t = Thread.ofVirtual().uncaughtExceptionHandler((th, e) -> {
                StackTraceElement[] st = e.getStackTrace();
                out[0] = e.getMessage() + " bottom=" + st[st.length - 1].getClassName() + "." + st[st.length - 1].getMethodName()
                        + " top=" + st[0].getMethodName();
            }).start(() -> {
                throw new IllegalStateException("boom");
            });
            t.join();
            row("uncaught", out[0]);
        }
        // 12. own stack from inside
        {
            String[] out = new String[1];
            Thread t = Thread.ofVirtual().start(() -> {
                StringBuilder sb = new StringBuilder();
                for (StackTraceElement e : new Throwable().getStackTrace()) {
                    sb.append(e.getClassName()).append('.').append(e.getMethodName()).append(' ');
                }
                out[0] = sb.toString().trim().replaceAll("\\$\\$Lambda[^ ]*", "\\$\\$Lambda");
            });
            t.join();
            row("own-stack", out[0]);
        }
        // 13. nested virtual threads and a producer/consumer queue
        {
            BlockingQueue<Integer> q = new ArrayBlockingQueue<>(4);
            long[] sum = new long[1];
            Thread outer = Thread.ofVirtual().start(() -> {
                Thread prod = Thread.ofVirtual().start(() -> {
                    for (int i = 1; i <= 2000; i++) {
                        try {
                            q.put(i);
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                });
                for (int i = 0; i < 2000; i++) {
                    try {
                        sum[0] += q.take();
                    } catch (InterruptedException e) {
                        return;
                    }
                }
                try {
                    prod.join();
                } catch (InterruptedException e) {
                }
            });
            row("queue", joinAll(List.of(outer), 60_000) ? sum[0] : "TIMEOUT");
        }
        // 14. timed join of a sleeper
        {
            Thread t = Thread.ofVirtual().start(() -> {
                try {
                    Thread.sleep(2_000);
                } catch (InterruptedException e) {
                }
            });
            boolean first = t.join(java.time.Duration.ofMillis(50));
            boolean second = t.join(java.time.Duration.ofSeconds(30));
            row("timed-join", first + " " + second);
        }
        // 15. pinned park inside synchronized
        {
            Object m = new Object();
            String[] out = new String[1];
            CountDownLatch in = new CountDownLatch(1);
            Thread t = Thread.ofVirtual().start(() -> {
                synchronized (m) {
                    in.countDown();
                    LockSupport.park();
                    out[0] = "unparked";
                }
            });
            in.await();
            while (t.getState() != Thread.State.WAITING) {
                Thread.sleep(1);
            }
            row("pinned-state", t.getState());
            LockSupport.unpark(t);
            t.join(30_000);
            row("pinned-park", out[0]);
        }
        row("end", "ok");
    }
}
