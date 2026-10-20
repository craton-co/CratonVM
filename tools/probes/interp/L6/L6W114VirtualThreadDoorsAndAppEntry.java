// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 114, lane L6 -- correctness of the two wave-114
// virtual-thread switches (page
// i113-orch-virtual-thread-frames-enter-only-leaf-bodies-compiled, "Wave 114"):
//   CRATONVM_VTHREAD_NONVIRTUAL_DOORS   the static / special fast doors on a
//                                        JDK virtual thread
//   CRATONVM_VTHREAD_COMPILED_APP_ENTRY an interpreted virtual-thread frame
//                                        enters compiled application bodies
//   CRATONVM_VTHREAD_LOCAL_CALL_COUNT    the per-entry invocation count
// The output must be identical with every switch off and on, in both modes,
// JIT and --nojit, and equal HotSpot 25's:
// * a contended static synchronized method and a synchronized instance
//   method called hot from 16 virtual threads on 2 carriers, parking while
//   holding neither, count exactly (the doors decline synchronized callees);
// * constructors on virtual threads: an empty-body chain, a trivial
//   constructor, a field-store constructor, a super chain -- the objects'
//   fields read back;
// * a hot application handler that parks INSIDE itself (pinned once while
//   compiled under the app-entry arm, then denied) keeps its locals and
//   result across 40 parks per thread;
// * an exception raised three static calls down, on a virtual thread after
//   warm-up: HotSpot's message and the four top frames by method name;
// * Thread.currentThread() read in a hot static helper is the caller's own
//   virtual thread before and after parks;
// * a static recursion 200 deep on a virtual thread answers its sum.
//
// Run, each mode (and --nojit), with all off and all on:
//   cratonvm -Djdk.virtualThreadScheduler.parallelism=2 L6W114VirtualThreadDoorsAndAppEntry
//   CRATONVM_VTHREAD_NONVIRTUAL_DOORS=1 CRATONVM_VTHREAD_COMPILED_APP_ENTRY=1 \
//     cratonvm -Djdk.virtualThreadScheduler.parallelism=2 L6W114VirtualThreadDoorsAndAppEntry
// Census: CRATONVM_DBG_VTHREAD_CENSUS=1 -- learned_pins stays a handful.
//
// HOTSPOT_EXPECTED_BEGIN
// monitors: static=32000 instance=32000
// ctors: empty=4000 trivial=4000 fields=12000 chain=8000
// blocking: sum=true parks=640
// exception: message=/ by zero top=divide,middle,outer,raise
// identity: mismatches=0
// recursion: sum=20100
// HOTSPOT_EXPECTED_END

import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

public class L6W114VirtualThreadDoorsAndAppEntry {
    static int staticCount;

    static synchronized void bumpStatic() {
        staticCount++;
    }

    static final class Counter {
        int n;

        synchronized void bump() {
            n++;
        }
    }

    static final class Empty {
        Empty() {
        }
    }

    static final class Trivial {
    }

    static final class Point {
        final int x;
        final int y;
        final int z;

        Point(int x, int y, int z) {
            this.x = x;
            this.y = y;
            this.z = z;
        }
    }

    static class Base {
        final int a;

        Base(int a) {
            this.a = a;
        }
    }

    static final class Derived extends Base {
        final int b;

        Derived(int a, int b) {
            super(a);
            this.b = b;
        }
    }

    static void join(Thread[] ts) throws InterruptedException {
        for (Thread t : ts) {
            t.join();
        }
    }

    static void monitors() throws InterruptedException {
        Counter c = new Counter();
        Thread[] ts = new Thread[16];
        for (int k = 0; k < ts.length; k++) {
            ts[k] = Thread.ofVirtual().start(() -> {
                for (int round = 0; round < 4; round++) {
                    for (int i = 0; i < 500; i++) {
                        bumpStatic();
                        c.bump();
                    }
                    LockSupport.parkNanos(1_000);
                }
            });
        }
        join(ts);
        System.out.println("monitors: static=" + staticCount + " instance=" + c.n);
    }

    static void ctors() throws InterruptedException {
        AtomicLong empty = new AtomicLong();
        AtomicLong trivial = new AtomicLong();
        AtomicLong fields = new AtomicLong();
        AtomicLong chain = new AtomicLong();
        Thread[] ts = new Thread[8];
        for (int k = 0; k < ts.length; k++) {
            ts[k] = Thread.ofVirtual().start(() -> {
                long e = 0, t = 0, f = 0, ch = 0;
                for (int round = 0; round < 2; round++) {
                    for (int i = 0; i < 250; i++) {
                        if (new Empty() != null) {
                            e++;
                        }
                        if (new Trivial() != null) {
                            t++;
                        }
                        Point p = new Point(1, 1, 1);
                        f += p.x + p.y + p.z;
                        Derived d = new Derived(1, 1);
                        ch += d.a + d.b;
                    }
                    LockSupport.parkNanos(1_000);
                }
                empty.addAndGet(e);
                trivial.addAndGet(t);
                fields.addAndGet(f);
                chain.addAndGet(ch);
            });
        }
        join(ts);
        System.out.println("ctors: empty=" + empty + " trivial=" + trivial + " fields=" + fields
                + " chain=" + chain);
    }

    static long helper(long v, int i) {
        return (v * 31 + i) & 0xffff;
    }

    /** Hot application handler with its park inside. */
    static long handle(int seed, int n, AtomicInteger parks) {
        long a = helper(seed, n);
        long b = helper(a, n + 1);
        LockSupport.parkNanos(1_000);
        parks.incrementAndGet();
        return helper(a + b, n + 2) + a + b;
    }

    static long handlePlain(int seed, int n) {
        long a = helper(seed, n);
        long b = helper(a, n + 1);
        return helper(a + b, n + 2) + a + b;
    }

    static void blocking() throws InterruptedException {
        AtomicInteger parks = new AtomicInteger();
        AtomicInteger good = new AtomicInteger();
        Thread[] ts = new Thread[16];
        for (int k = 0; k < ts.length; k++) {
            final int seed = k;
            ts[k] = Thread.ofVirtual().start(() -> {
                long got = 0;
                long want = 0;
                for (int n = 0; n < 40; n++) {
                    got += handle(seed, n, parks);
                    want += handlePlain(seed, n);
                }
                if (got == want) {
                    good.incrementAndGet();
                }
            });
        }
        join(ts);
        System.out.println("blocking: sum=" + (good.get() == ts.length) + " parks=" + parks);
    }

    static int divide(int a, int b) {
        return a / b;
    }

    static int middle(int a, int b) {
        return divide(a, b) + 1;
    }

    static int outer(int a, int b) {
        return middle(a, b) + 1;
    }

    static int raise(int a, int b) {
        return outer(a, b) + 1;
    }

    static void exception() throws InterruptedException {
        String[] line = new String[1];
        Thread t = Thread.ofVirtual().start(() -> {
            int acc = 0;
            for (int i = 0; i < 3000; i++) {
                acc += raise(i, 1);
                if ((i & 255) == 0) {
                    LockSupport.parkNanos(1_000);
                }
            }
            try {
                acc += raise(1, 0);
            } catch (ArithmeticException e) {
                StackTraceElement[] st = e.getStackTrace();
                line[0] = "exception: message=" + e.getMessage() + " top=" + st[0].getMethodName()
                        + "," + st[1].getMethodName() + "," + st[2].getMethodName() + ","
                        + st[3].getMethodName();
            }
            if (acc == 42) {
                System.out.println("unreachable");
            }
        });
        t.join();
        System.out.println(line[0]);
    }

    static Thread whoAmI(int i) {
        return (i & 1) == 0 ? Thread.currentThread() : Thread.currentThread();
    }

    static void identity() throws InterruptedException {
        AtomicInteger mismatches = new AtomicInteger();
        Thread[] ts = new Thread[8];
        for (int k = 0; k < ts.length; k++) {
            ts[k] = Thread.ofVirtual().start(() -> {
                Thread me = Thread.currentThread();
                for (int round = 0; round < 5; round++) {
                    for (int i = 0; i < 400; i++) {
                        if (whoAmI(i) != me) {
                            mismatches.incrementAndGet();
                        }
                    }
                    LockSupport.parkNanos(1_000);
                }
            });
        }
        join(ts);
        System.out.println("identity: mismatches=" + mismatches);
    }

    static int sumTo(int n) {
        return n == 0 ? 0 : n + sumTo(n - 1);
    }

    static void recursion() throws InterruptedException {
        int[] out = new int[1];
        Thread t = Thread.ofVirtual().start(() -> {
            int s = 0;
            for (int i = 0; i < 50; i++) {
                s = sumTo(200);
                if ((i & 15) == 0) {
                    LockSupport.parkNanos(1_000);
                }
            }
            out[0] = s;
        });
        t.join();
        System.out.println("recursion: sum=" + out[0]);
    }

    public static void main(String[] args) throws Exception {
        monitors();
        ctors();
        blocking();
        exception();
        identity();
        recursion();
    }
}
