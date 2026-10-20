// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 93, lane L6 -- stage 2 of compiled code on JDK
// virtual threads (page i91-L6-virtual-thread-code-is-never-compiled):
// instance leaves through the virtual / interface / special arms
// (CRATONVM_VTHREAD_COMPILED_INSTANCE_LEAF, default off) and static callers
// of static leaves (CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS, default off). The
// output must be the same with the switches off and on, and HotSpot 25's:
// * hot instance and interface leaves on 200 virtual threads answer the
//   platform sum (a[i] = i*7 % 1000 permutes 0..999, so one call is 499500);
// * an ArithmeticException / ArrayIndexOutOfBoundsException raised inside a
//   hot instance leaf keeps HotSpot's message and its two top frames;
// * an interface call site that alternates a leaf receiver with a receiver
//   whose body blocks in a SynchronousQueue hand-off (a yield) still unmounts
//   on ONE carrier: a compiled frame under the hand-off would pin the only
//   carrier and the partner could never run (done would stay below 2);
// * a static caller of two static leaves, hot, and a static caller whose
//   second callee blocks in the hand-off, both finish on one carrier;
// * an invokespecial (super) leaf and a private leaf answer the platform sum.
//
// Run, each mode, with the switches off and on (the output must not change):
//   cratonvm -Djdk.virtualThreadScheduler.parallelism=1 L6W93VirtualThreadCompiledInstanceLeaf
//   CRATONVM_VTHREAD_COMPILED_INSTANCE_LEAF=1 CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS=1 cratonvm -Djdk.virtualThreadScheduler.parallelism=1 L6W93VirtualThreadCompiledInstanceLeaf
//   (and both with --compatible; CRATONVM_VTHREAD_MONITOR_UNMOUNT=0 too)
//
// HOTSPOT_EXPECTED_BEGIN
// instance on virtual threads: 19980000000 interface: 19980000000 platform: 19980000000
// arithmetic: java.lang.ArithmeticException / by zero top=div,caller
// bounds: java.lang.ArrayIndexOutOfBoundsException Index 5 out of bounds for length 3 top=get,callerGet
// polymorphic hand-off: done=2 taken=200
// callers: twice=4995000000 hand-off done=2 sum=99900000
// special: super=999000000 private=999000000 virtual=true
// HOTSPOT_EXPECTED_END

import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;

public class L6W93VirtualThreadCompiledInstanceLeaf {
    interface Summing {
        long sum(int[] a);
    }

    // Instance leaves: no invoke, no monitor op, not synchronized.
    static class Summer implements Summing {
        final int start;

        Summer(int start) {
            this.start = start;
        }

        @Override
        public long sum(int[] a) {
            long s = 0;
            for (int i = start; i < a.length; i++) {
                s += a[i];
            }
            return s;
        }

        int div(int a, int b) {
            return a / b;
        }

        int get(int[] a, int i) {
            return a[i];
        }

        int caller(int a, int b) {
            return div(a, b);
        }

        int callerGet(int[] a, int i) {
            return get(a, i);
        }

        private long priv(int[] a) {
            long s = 0;
            for (int i = 0; i < a.length; i++) {
                s += a[i];
            }
            return s;
        }

        long viaPrivate(int[] a) {
            return priv(a);
        }
    }

    // `super.sum` is an invokespecial of the leaf above.
    static final class Derived extends Summer {
        Derived() {
            super(0);
        }

        @Override
        public long sum(int[] a) {
            return super.sum(a);
        }
    }

    // One interface site, two receivers: a leaf, and a body that blocks.
    interface Step {
        long step(int[] a, int k) throws InterruptedException;
    }

    static final class LeafStep implements Step {
        @Override
        public long step(int[] a, int k) {
            long s = 0;
            for (int i = 0; i < a.length; i++) {
                s += a[i];
            }
            return s;
        }
    }

    static final class HandoffStep implements Step {
        final SynchronousQueue<Integer> queue;

        HandoffStep(SynchronousQueue<Integer> queue) {
            this.queue = queue;
        }

        @Override
        public long step(int[] a, int k) throws InterruptedException {
            queue.put(k);
            return 0;
        }
    }

    // Static leaves and callers of them.
    static long sum(int[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static long twice(int[] a) {
        return sum(a) + sum(a);
    }

    static void putStatic(SynchronousQueue<Integer> q, int v) throws InterruptedException {
        q.put(v);
    }

    // Two invokestatic sites; the second callee is not a leaf.
    static long sumAndPut(int[] a, SynchronousQueue<Integer> q, int v) throws InterruptedException {
        long s = sum(a);
        putStatic(q, v);
        return s;
    }

    static String top(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st[0].getMethodName() + "," + st[1].getMethodName();
    }

    static void joinAll(Thread[] ts) throws InterruptedException {
        for (Thread t : ts) {
            t.join(TimeUnit.SECONDS.toMillis(60));
        }
    }

    public static void main(String[] args) throws Exception {
        if (System.getProperty("jdk.virtualThreadScheduler.parallelism") == null) {
            System.setProperty("jdk.virtualThreadScheduler.parallelism", "1");
        }
        int[] data = new int[1000];
        for (int i = 0; i < data.length; i++) {
            data[i] = i * 7 % 1000;
        }
        Summer summer = new Summer(0);
        Summing summing = summer;

        // Hot instance and interface leaves on many virtual threads.
        AtomicLong itotal = new AtomicLong();
        AtomicLong ftotal = new AtomicLong();
        Thread[] vts = new Thread[200];
        for (int t = 0; t < vts.length; t++) {
            vts[t] = Thread.ofVirtual().start(() -> {
                long s = 0;
                long f = 0;
                for (int k = 0; k < 200; k++) {
                    s += summer.sum(data);
                    f += summing.sum(data);
                }
                itotal.addAndGet(s);
                ftotal.addAndGet(f);
            });
        }
        joinAll(vts);
        AtomicLong ptotal = new AtomicLong();
        Thread p = Thread.ofPlatform().start(() -> {
            long s = 0;
            for (int k = 0; k < 40_000; k++) {
                s += summer.sum(data);
            }
            ptotal.set(s);
        });
        p.join();
        System.out.println("instance on virtual threads: " + itotal.get() + " interface: " + ftotal.get()
                + " platform: " + ptotal.get());

        // Exceptions raised in a hot instance leaf on a virtual thread.
        AtomicReference<String> arith = new AtomicReference<>();
        AtomicReference<String> bounds = new AtomicReference<>();
        Thread ex = Thread.ofVirtual().start(() -> {
            int[] small = {1, 2, 3};
            int acc = 0;
            for (int k = 0; k < 20_000; k++) {
                acc += summer.caller(k, 3) + summer.callerGet(small, k % 3);
            }
            try {
                acc += summer.caller(acc, 0);
            } catch (ArithmeticException e) {
                arith.set(e.getClass().getName() + " " + e.getMessage() + " top=" + top(e));
            }
            try {
                acc += summer.callerGet(small, 5);
            } catch (ArrayIndexOutOfBoundsException e) {
                bounds.set(e.getClass().getName() + " " + e.getMessage() + " top=" + top(e));
            }
        });
        ex.join();
        System.out.println("arithmetic: " + arith.get());
        System.out.println("bounds: " + bounds.get());

        // One interface site: a hot leaf receiver, then a receiver that
        // blocks in a hand-off the partner (on the same carrier) completes.
        SynchronousQueue<Integer> queue = new SynchronousQueue<>();
        Step[] steps = {new LeafStep(), new HandoffStep(queue)};
        AtomicInteger pdone = new AtomicInteger();
        AtomicInteger taken = new AtomicInteger();
        Thread[] pair = new Thread[2];
        pair[0] = Thread.ofVirtual().start(() -> {
            try {
                for (int k = 0; k < 200; k++) {
                    for (int j = 0; j < 20; j++) {
                        steps[0].step(data, k);
                    }
                    for (Step s : steps) {
                        s.step(data, k);
                    }
                }
                pdone.incrementAndGet();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        pair[1] = Thread.ofVirtual().start(() -> {
            try {
                for (int k = 0; k < 200; k++) {
                    for (int j = 0; j < 20; j++) {
                        steps[0].step(data, k);
                    }
                    if (queue.take() == k) {
                        taken.incrementAndGet();
                    }
                }
                pdone.incrementAndGet();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        joinAll(pair);
        System.out.println("polymorphic hand-off: done=" + pdone.get() + " taken=" + taken.get());

        // Static callers of static leaves.
        AtomicLong twiceTotal = new AtomicLong();
        Thread[] callers = new Thread[50];
        for (int t = 0; t < callers.length; t++) {
            callers[t] = Thread.ofVirtual().start(() -> {
                long s = 0;
                for (int k = 0; k < 100; k++) {
                    s += twice(data);
                }
                twiceTotal.addAndGet(s);
            });
        }
        joinAll(callers);
        SynchronousQueue<Integer> q2 = new SynchronousQueue<>();
        AtomicInteger cdone = new AtomicInteger();
        AtomicLong csum = new AtomicLong();
        Thread[] cpair = new Thread[2];
        cpair[0] = Thread.ofVirtual().start(() -> {
            try {
                long s = 0;
                for (int k = 0; k < 200; k++) {
                    s += sumAndPut(data, q2, k);
                }
                csum.set(s);
                cdone.incrementAndGet();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        cpair[1] = Thread.ofVirtual().start(() -> {
            try {
                for (int k = 0; k < 200; k++) {
                    q2.take();
                }
                cdone.incrementAndGet();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        joinAll(cpair);
        System.out.println("callers: twice=" + twiceTotal.get() + " hand-off done=" + cdone.get()
                + " sum=" + csum.get());

        // invokespecial (super) and private leaves.
        Derived derived = new Derived();
        AtomicLong superTotal = new AtomicLong();
        AtomicLong privTotal = new AtomicLong();
        AtomicInteger allVirtual = new AtomicInteger();
        Thread sp = Thread.ofVirtual().start(() -> {
            long s = 0;
            long q = 0;
            boolean virtual = true;
            for (int k = 0; k < 2000; k++) {
                s += derived.sum(data);
                q += summer.viaPrivate(data);
                virtual &= Thread.currentThread().isVirtual();
            }
            superTotal.set(s);
            privTotal.set(q);
            allVirtual.set(virtual ? 1 : 0);
        });
        sp.join();
        System.out.println("special: super=" + superTotal.get() + " private=" + privTotal.get()
                + " virtual=" + (allVirtual.get() == 1));
        System.exit(0);
    }
}
