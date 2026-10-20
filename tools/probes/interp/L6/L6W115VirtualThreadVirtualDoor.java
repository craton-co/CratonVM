// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L6 -- correctness of
// CRATONVM_VTHREAD_VIRTUAL_DOOR (the virtual / interface invoke fast door on
// a mounted JDK virtual thread; proposal
// i114-L6-proposal-the-virtual-door-on-virtual-threads, "Wave 115").
// The output must be identical with the switch off and on (alone, and with
// CRATONVM_VTHREAD_NONVIRTUAL_DOORS=1 and CRATONVM_VTHREAD_COMPILED_APP_ENTRY=1),
// in both modes, JIT and --nojit, and equal HotSpot 25's:
// * a warm monomorphic invokevirtual and a megamorphic-ish (4 receivers)
//   invokeinterface site, hot on 16 virtual threads, sum exactly;
// * a synchronized instance method called hot from 16 virtual threads on one
//   shared receiver while another thread holds its monitor for a while (the
//   door declines synchronized callees on a virtual thread; the contended
//   enter unmounts or pins, and the count is exact);
// * a trivial getter, an empty override and a default interface method;
// * an interface method that parks INSIDE itself keeps its locals across
//   parks;
// * an exception thrown two virtual calls down: message and the top frames;
// * a lambda called through its interface on a virtual thread (the lambda
//   door declines there);
// * a private nestmate invokevirtual (the hand-off keeps its own switch).
//
// Run, each mode (and --nojit), with the switch off and on:
//   cratonvm -Djdk.virtualThreadScheduler.parallelism=2 L6W115VirtualThreadVirtualDoor
//   CRATONVM_VTHREAD_VIRTUAL_DOOR=1 \
//     cratonvm -Djdk.virtualThreadScheduler.parallelism=2 L6W115VirtualThreadVirtualDoor
//   CRATONVM_VTHREAD_VIRTUAL_DOOR=1 CRATONVM_VTHREAD_NONVIRTUAL_DOORS=1 \
//   CRATONVM_VTHREAD_COMPILED_APP_ENTRY=1 \
//     cratonvm -Djdk.virtualThreadScheduler.parallelism=2 L6W115VirtualThreadVirtualDoor
// Door census: CRATONVM_DBG_FIELD_SITE=1 names "synchronized callee on a
// virtual thread" among the virtual door's declines with the switch on.
//
// HOTSPOT_EXPECTED_BEGIN
// mono: sum=4638400
// iface: sum=23200000
// monitor: count=32000 heldBy=main
// small: getter=48000 empty=16000 dflt=64000
// parking: ok=16 parks=640
// exception: message=bad 7 top=fail,middle,outer
// lambda: sum=2160000
// private: sum=1599376
// HOTSPOT_EXPECTED_END

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.IntUnaryOperator;
import java.util.concurrent.locks.LockSupport;

public class L6W115VirtualThreadVirtualDoor {
    static final int THREADS = 16;

    static class Adder {
        int add(int a, int b) {
            return a + b;
        }
    }

    interface Shape {
        int area(int k);

        default int twice(int k) {
            return area(k) * 2;
        }
    }

    static final class Sq implements Shape {
        public int area(int k) {
            return k * k;
        }
    }

    static final class Rect implements Shape {
        public int area(int k) {
            return k * (k + 1);
        }
    }

    static final class Tri implements Shape {
        public int area(int k) {
            return (k * (k + 1)) / 2;
        }
    }

    static final class Unit implements Shape {
        public int area(int k) {
            return 1;
        }
    }

    static final class Counter {
        int n;

        synchronized void bump() {
            n++;
        }
    }

    static class Base {
        int v = 3;

        int get() {
            return v;
        }

        void hook() {}
    }

    static final class Derived extends Base {
        @Override
        void hook() {}
    }

    interface Parker {
        long work(int seed, int parks);
    }

    static final class ParkingWorker implements Parker {
        public long work(int seed, int parks) {
            long a = seed;
            long b = seed * 3L;
            for (int i = 0; i < parks; i++) {
                a += i;
                LockSupport.parkNanos(1_000L);
                b += a ^ i;
            }
            return a * 31 + b;
        }
    }

    static long expectedWork(int seed, int parks) {
        long a = seed;
        long b = seed * 3L;
        for (int i = 0; i < parks; i++) {
            a += i;
            b += a ^ i;
        }
        return a * 31 + b;
    }

    static final class Thrower {
        int fail(int x) {
            throw new IllegalStateException("bad " + x);
        }

        int middle(int x) {
            return fail(x) + 1;
        }

        int outer(int x) {
            return middle(x) + 1;
        }
    }

    private int privateBump(int x) {
        return x + 1;
    }

    static void runAll(int n, IntTask task) throws InterruptedException {
        Thread[] ts = new Thread[n];
        for (int t = 0; t < n; t++) {
            final int id = t;
            ts[t] = Thread.ofVirtual().unstarted(() -> task.run(id));
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
    }

    interface IntTask {
        void run(int id);
    }

    public static void main(String[] args) throws Exception {
        // Monomorphic invokevirtual.
        AtomicLong mono = new AtomicLong();
        runAll(THREADS, id -> {
            Adder adder = new Adder();
            long s = 0;
            for (int i = 0; i < 20_000; i++) {
                s += adder.add(i % 30, 0);
                if (i % 2_000 == 0) {
                    Thread.yield();
                }
            }
            mono.addAndGet(s);
        });
        System.out.println("mono: sum=" + mono.get());

        // invokeinterface over four receivers (poly entries).
        Shape[] shapes = {new Sq(), new Rect(), new Tri(), new Unit()};
        AtomicLong iface = new AtomicLong();
        runAll(THREADS, id -> {
            long s = 0;
            for (int i = 0; i < 20_000; i++) {
                s += shapes[i & 3].area(i % 20);
                if (i % 5_000 == 0) {
                    Thread.yield();
                }
            }
            iface.addAndGet(s);
        });
        System.out.println("iface: sum=" + iface.get());

        // A synchronized instance method, contended: main holds the monitor
        // while the virtual threads start.
        Counter counter = new Counter();
        CountDownLatch started = new CountDownLatch(THREADS);
        Thread[] ts = new Thread[THREADS];
        String heldBy;
        synchronized (counter) {
            for (int t = 0; t < THREADS; t++) {
                ts[t] = Thread.ofVirtual().start(() -> {
                    started.countDown();
                    for (int i = 0; i < 2_000; i++) {
                        counter.bump();
                        if (i % 500 == 0) {
                            Thread.yield();
                        }
                    }
                });
            }
            started.await();
            Thread.sleep(20);
            heldBy = Thread.holdsLock(counter) ? Thread.currentThread().getName() : "?";
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("monitor: count=" + counter.n + " heldBy=" + heldBy);

        // Trivial getter, empty override, default method.
        AtomicLong getter = new AtomicLong();
        AtomicInteger empty = new AtomicInteger();
        AtomicLong dflt = new AtomicLong();
        runAll(THREADS, id -> {
            Base b = new Derived();
            Shape u = new Unit();
            long g = 0;
            long d = 0;
            int e = 0;
            for (int i = 0; i < 1_000; i++) {
                g += b.get();
                b.hook();
                e++;
                d += u.twice(i) + u.twice(i + 1);
            }
            getter.addAndGet(g);
            empty.addAndGet(e);
            dflt.addAndGet(d);
        });
        System.out.println("small: getter=" + getter.get() + " empty=" + empty.get() + " dflt=" + dflt.get());

        // An interface method that parks inside itself.
        AtomicInteger ok = new AtomicInteger();
        AtomicInteger parks = new AtomicInteger();
        Parker parker = new ParkingWorker();
        runAll(THREADS, id -> {
            boolean good = true;
            for (int r = 0; r < 4; r++) {
                int seed = id * 100 + r;
                if (parker.work(seed, 10) != expectedWork(seed, 10)) {
                    good = false;
                }
                parks.addAndGet(10);
            }
            if (good) {
                ok.incrementAndGet();
            }
        });
        System.out.println("parking: ok=" + ok.get() + " parks=" + parks.get());

        // An exception thrown two virtual calls down, after warm-up.
        String[] exc = new String[1];
        runAll(1, id -> {
            Thrower th = new Thrower();
            for (int i = 0; i < 3_000; i++) {
                try {
                    th.outer(i);
                } catch (IllegalStateException ignored) {
                }
            }
            try {
                th.outer(7);
            } catch (IllegalStateException ex) {
                StackTraceElement[] st = ex.getStackTrace();
                exc[0] = "message=" + ex.getMessage() + " top=" + st[0].getMethodName() + ","
                        + st[1].getMethodName() + "," + st[2].getMethodName();
            }
        });
        System.out.println("exception: " + exc[0]);

        // A lambda through its interface.
        AtomicLong lam = new AtomicLong();
        runAll(THREADS, id -> {
            int k = 3;
            IntUnaryOperator op = x -> x * k;
            long s = 0;
            for (int i = 0; i < 10_000; i++) {
                s += op.applyAsInt(i % 10);
            }
            lam.addAndGet(s);
        });
        System.out.println("lambda: sum=" + lam.get());

        // A private nestmate invokevirtual.
        AtomicLong priv = new AtomicLong();
        L6W115VirtualThreadVirtualDoor self = new L6W115VirtualThreadVirtualDoor();
        runAll(THREADS, id -> {
            long s = 0;
            for (int i = 0; i < 10_000; i++) {
                s += self.privateBump(i % 19);
            }
            priv.addAndGet(s);
        });
        System.out.println("private: sum=" + priv.get());
    }
}
