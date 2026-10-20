// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 85, lane L6 -- review probe: JDK 25 ThreadGroup
// semantics, setPriority / setDaemon on started, terminated and virtual
// threads, the group uncaught-exception chain, Object.wait's argument checks
// and nanos rounding, Thread.sleep(Duration), and Thread.join with an
// interrupt. Every answer is JDK 25 bytecode over a handful of VM natives
// (`Thread.getThreads`, `holdsLock`, `wait0`, `sleepNanos0`, `eetop` /
// `threadStatus`), so this pins those natives against HotSpot in both modes.
//
// Expected output derived from JDK 25 java/lang/{Thread,ThreadGroup,Object}.java
// and HotSpot's `ObjectMonitor::wait` / `JVM_SleepNanos` (InterruptedException
// from `wait0` has no message; from `sleepNanos0`, "sleep interrupted").
//
// Run, each mode:
//   cratonvm L6W85ThreadApiShapes
//   cratonvm --compatible L6W85ThreadApiShapes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// fresh group: activeCount=0 activeGroupCount=0 parent=main maxPriority=10
// with a subgroup: activeGroupCount=1
// two live threads: activeCount=2 enumerate=[g1-a, g1-b] main recursive enumerate has g1-a=true
// after they ended: activeCount=0
// setMaxPriority(3): group=3 sub=3 newThread=3 setPriority(8)=3 setMaxPriority(15) keeps=3
// setPriority(11): IllegalArgumentException null
// started setDaemon: IllegalThreadStateException null
// terminated: setDaemon(true) ok isDaemon=true group=null state=TERMINATED
// virtual setDaemon(false): IllegalArgumentException 'false' not legal for virtual threads
// virtual setPriority(1): priority=5
// group uncaughtException: [group saw boom-1]
// own handler wins: [own saw boom-2]
// wait(-1) without the lock: IllegalArgumentException timeout value is negative
// wait(-1, 0): IllegalArgumentException timeoutMillis value is negative
// wait(0, -1): IllegalArgumentException nanosecond timeout value out of range
// wait(0, 1000000): IllegalArgumentException nanosecond timeout value out of range
// wait() without the lock: IllegalMonitorStateException current thread is not owner
// wait(0, 1) is a timed wait: true
// pre-interrupted wait(10): InterruptedException null interrupted-after=false
// sleep(Duration -5ms): returned
// sleep(-1): IllegalArgumentException timeout value is negative
// sleep(0, 1000000): IllegalArgumentException nanosecond timeout value out of range
// pre-interrupted sleep(Duration 1s): InterruptedException sleep interrupted interrupted-after=false
// join(Duration) unstarted: IllegalThreadStateException Thread not started
// join() unstarted: returned
// join(-1): IllegalArgumentException timeout value is negative
// join(Duration 20ms) on a live thread: false
// pre-interrupted join(5000): InterruptedException null interrupted-after=false
// HOTSPOT_EXPECTED_END

import java.time.Duration;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CountDownLatch;

public class L6W85ThreadApiShapes {
    interface Body {
        void run() throws Exception;
    }

    static String outcome(Body b) {
        try {
            b.run();
            return "returned";
        } catch (Exception e) {
            return e.getClass().getSimpleName() + " " + e.getMessage();
        }
    }

    static void await(CountDownLatch l) {
        try {
            l.await();
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
    }

    public static void main(String[] args) throws Exception {
        ThreadGroup main = Thread.currentThread().getThreadGroup();
        ThreadGroup g = new ThreadGroup("g1");
        System.out.println("fresh group: activeCount=" + g.activeCount()
                + " activeGroupCount=" + g.activeGroupCount()
                + " parent=" + g.getParent().getName()
                + " maxPriority=" + g.getMaxPriority());
        ThreadGroup sub = new ThreadGroup(g, "sub");
        System.out.println("with a subgroup: activeGroupCount=" + g.activeGroupCount());

        CountDownLatch hold = new CountDownLatch(1);
        Thread a = new Thread(g, () -> await(hold), "g1-a");
        Thread b = new Thread(g, () -> await(hold), "g1-b");
        a.start();
        b.start();
        Thread[] arr = new Thread[8];
        int n = g.enumerate(arr);
        List<String> names = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            names.add(arr[i].getName());
        }
        Collections.sort(names);
        Thread[] all = new Thread[main.activeCount() + 16];
        int m = main.enumerate(all, true);
        boolean hasA = Arrays.asList(all).subList(0, m).contains(a);
        System.out.println("two live threads: activeCount=" + g.activeCount()
                + " enumerate=" + names + " main recursive enumerate has g1-a=" + hasA);

        String startedDaemon = outcome(() -> a.setDaemon(true));
        hold.countDown();
        a.join();
        b.join();
        System.out.println("after they ended: activeCount=" + g.activeCount());

        g.setMaxPriority(3);
        int groupMax = g.getMaxPriority();
        Thread p = new Thread(g, () -> {}, "p");
        int newPrio = p.getPriority();
        p.setPriority(8);
        int set8 = p.getPriority();
        g.setMaxPriority(15);
        System.out.println("setMaxPriority(3): group=" + groupMax +" sub=" + sub.getMaxPriority()
                + " newThread=" + newPrio + " setPriority(8)=" + set8
                + " setMaxPriority(15) keeps=" + g.getMaxPriority());
        System.out.println("setPriority(11): " + outcome(() -> p.setPriority(11)));
        System.out.println("started setDaemon: " + startedDaemon);
        a.setDaemon(true);
        System.out.println("terminated: setDaemon(true) ok isDaemon=" + a.isDaemon()
                + " group=" + a.getThreadGroup() + " state=" + a.getState());

        Thread v = Thread.ofVirtual().unstarted(() -> {});
        System.out.println("virtual setDaemon(false): " + outcome(() -> v.setDaemon(false)));
        v.setPriority(1);
        System.out.println("virtual setPriority(1): priority=" + v.getPriority());

        List<String> seen = Collections.synchronizedList(new ArrayList<>());
        ThreadGroup recording = new ThreadGroup("recording") {
            @Override
            public void uncaughtException(Thread t, Throwable e) {
                seen.add("group saw " + e.getMessage());
            }
        };
        Thread u1 = new Thread(recording, () -> {
            throw new RuntimeException("boom-1");
        }, "u1");
        u1.start();
        u1.join();
        System.out.println("group uncaughtException: " + seen);
        seen.clear();
        Thread u2 = new Thread(recording, () -> {
            throw new RuntimeException("boom-2");
        }, "u2");
        u2.setUncaughtExceptionHandler((t, e) -> seen.add("own saw " + e.getMessage()));
        u2.start();
        u2.join();
        System.out.println("own handler wins: " + seen);

        Object lock = new Object();
        System.out.println("wait(-1) without the lock: " + outcome(() -> lock.wait(-1)));
        System.out.println("wait(-1, 0): " + outcome(() -> {
            synchronized (lock) {
                lock.wait(-1, 0);
            }
        }));
        System.out.println("wait(0, -1): " + outcome(() -> {
            synchronized (lock) {
                lock.wait(0, -1);
            }
        }));
        System.out.println("wait(0, 1000000): " + outcome(() -> {
            synchronized (lock) {
                lock.wait(0, 1_000_000);
            }
        }));
        System.out.println("wait() without the lock: " + outcome(() -> lock.wait()));
        long t0 = System.nanoTime();
        synchronized (lock) {
            lock.wait(0, 1); // rounds up to wait(1): timed, not forever
        }
        System.out.println("wait(0, 1) is a timed wait: " + (System.nanoTime() - t0 < 5_000_000_000L));
        Thread.currentThread().interrupt();
        String w = outcome(() -> {
            synchronized (lock) {
                lock.wait(10);
            }
        });
        System.out.println("pre-interrupted wait(10): " + w + " interrupted-after=" + Thread.interrupted());

        System.out.println("sleep(Duration -5ms): " + outcome(() -> Thread.sleep(Duration.ofMillis(-5))));
        System.out.println("sleep(-1): " + outcome(() -> Thread.sleep(-1)));
        System.out.println("sleep(0, 1000000): " + outcome(() -> Thread.sleep(0, 1_000_000)));
        Thread.currentThread().interrupt();
        String s = outcome(() -> Thread.sleep(Duration.ofSeconds(1)));
        System.out.println("pre-interrupted sleep(Duration 1s): " + s + " interrupted-after=" + Thread.interrupted());

        Thread unstarted = new Thread(() -> {});
        System.out.println("join(Duration) unstarted: " + outcome(() -> unstarted.join(Duration.ofMillis(10))));
        System.out.println("join() unstarted: " + outcome(unstarted::join));
        CountDownLatch hold2 = new CountDownLatch(1);
        Thread live = new Thread(() -> await(hold2), "live");
        live.start();
        System.out.println("join(-1): " + outcome(() -> live.join(-1)));
        System.out.println("join(Duration 20ms) on a live thread: " + live.join(Duration.ofMillis(20)));
        Thread.currentThread().interrupt();
        String j = outcome(() -> live.join(5000));
        System.out.println("pre-interrupted join(5000): " + j + " interrupted-after=" + Thread.interrupted());
        hold2.countDown();
        live.join();
    }
}
