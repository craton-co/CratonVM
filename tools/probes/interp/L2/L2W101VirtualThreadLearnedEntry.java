// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L2 -- correctness of compiled code on
// JDK virtual threads for ANY non-synchronized body, learned off by its first
// pin (CRATONVM_VTHREAD_COMPILED_LEARNED, default off; page
// i91-L6-virtual-thread-code-is-never-compiled, proposal
// i100-L6-proposal-learned-compiled-entry-on-virtual-threads). The output
// must be identical with the switch off and on, in both modes, JIT and
// --nojit, and equal HotSpot 25's:
// * non-leaf helpers (virtual, interface and static calls, string work) hot
//   on 200 virtual threads that park between rounds answer the platform sum;
// * a hot non-leaf helper whose LAST call blocks in a SynchronousQueue
//   hand-off: on ONE carrier the partner must still run (a compiled frame
//   under the hand-off pins; the pinned park must take the carrier
//   compensation, or done stays below 2 and the probe hangs);
// * Thread.currentThread() and a ThreadLocal read inside a hot compiled
//   helper stay the virtual thread's own across parks (0 mismatches);
// * an ArithmeticException raised two calls down a hot helper is caught by
//   the helper's own handler on some calls and escapes on others, with
//   HotSpot's message and top frames;
// * a contended synchronized block inside a hot helper, 8 virtual threads on
//   one carrier, counts exactly;
// * a hot helper whose receiver profile breaks (a new subclass after warm-up,
//   a deopt) and that then parks still answers the right value;
// * Thread.sleep inside a hot helper, three times per thread, completes.
//
// Run, each mode, with the switch off and on (the output must not change):
//   cratonvm -Djdk.virtualThreadScheduler.parallelism=1 L2W101VirtualThreadLearnedEntry
//   CRATONVM_VTHREAD_COMPILED_LEARNED=1 cratonvm -Djdk.virtualThreadScheduler.parallelism=1 L2W101VirtualThreadLearnedEntry
//   (both with --compatible and --nojit; and with CRATONVM_DBG_VTHREAD_CENSUS=1
//   the learned_pins row must stay small -- a few per blocking method)
//
// HOTSPOT_EXPECTED_BEGIN
// helpers: virtual=platform true
// hand-off: done=2 taken=300
// identity: mismatches=0
// exceptions: caught=150 escaped=50 message=/ by zero top=divide,middle
// monitor: count=80000
// deopt: before=true after=true
// sleep: done=8
// HOTSPOT_EXPECTED_END

import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

public class L2W101VirtualThreadLearnedEntry {
    interface Shape {
        long area(int k);
    }

    static class Square implements Shape {
        final int side;

        Square(int side) {
            this.side = side;
        }

        @Override
        public long area(int k) {
            return (long) side * side + k;
        }
    }

    static class Rect implements Shape {
        final int w;
        final int h;

        Rect(int w, int h) {
            this.w = w;
            this.h = h;
        }

        @Override
        public long area(int k) {
            return (long) w * h - k;
        }
    }

    static class Mixer {
        long state;

        Mixer(long seed) {
            state = seed;
        }

        long next(int v) {
            state = state * 6364136223846793005L + v;
            return state >>> 33;
        }
    }

    // A non-leaf helper: virtual, interface and static calls, a String.
    static long work(Mixer m, Shape[] shapes, int i) {
        long s = m.next(i);
        s += shapes[i & 3].area(i);
        s += hashOf("k" + (i & 15));
        return s;
    }

    static int hashOf(String s) {
        return s.hashCode();
    }

    static Shape[] shapes() {
        return new Shape[] {new Square(3), new Rect(2, 5), new Square(7), new Rect(4, 4)};
    }

    static long task(int seed) {
        Mixer m = new Mixer(seed);
        Shape[] sh = shapes();
        long acc = 0;
        for (int round = 0; round < 20; round++) {
            for (int i = 0; i < 200; i++) {
                acc += work(m, sh, i);
            }
            LockSupport.parkNanos(1_000);
        }
        return acc;
    }

    static long run(boolean virtual, int threads) throws InterruptedException {
        AtomicLong total = new AtomicLong();
        Thread[] ts = new Thread[threads];
        for (int k = 0; k < threads; k++) {
            final int seed = k;
            Runnable r = () -> total.addAndGet(task(seed));
            ts[k] = virtual ? Thread.ofVirtual().start(r) : Thread.ofPlatform().start(r);
        }
        for (Thread t : ts) {
            t.join();
        }
        return total.get();
    }

    // Hand-off: a hot non-leaf helper whose last call blocks.
    static long relayCompute(int i) {
        return hashOf("r" + i) & 0xff;
    }

    static long relay(SynchronousQueue<Integer> q, boolean block, int v) throws InterruptedException {
        long s = 0;
        for (int i = 0; i < 20; i++) {
            s += relayCompute(i);
        }
        if (block) {
            if (v >= 0) {
                q.put(v);
            } else {
                s += q.take();
            }
        }
        return s;
    }

    static void handOff() throws InterruptedException {
        SynchronousQueue<Integer> q = new SynchronousQueue<>();
        AtomicInteger done = new AtomicInteger();
        AtomicInteger taken = new AtomicInteger();
        // Warm the helper on virtual threads (it never blocks here).
        Thread warm = Thread.ofVirtual().start(() -> {
            try {
                for (int i = 0; i < 20_000; i++) {
                    relay(q, false, 0);
                }
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        warm.join();
        Thread consumer = Thread.ofVirtual().start(() -> {
            try {
                for (int i = 0; i < 300; i++) {
                    relay(q, true, -1);
                    taken.incrementAndGet();
                }
                done.incrementAndGet();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        Thread producer = Thread.ofVirtual().start(() -> {
            try {
                for (int i = 0; i < 300; i++) {
                    relay(q, true, i);
                }
                done.incrementAndGet();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        consumer.join();
        producer.join();
        System.out.println("hand-off: done=" + done.get() + " taken=" + taken.get());
    }

    // Identity: Thread.currentThread() and a ThreadLocal inside a hot helper.
    static final ThreadLocal<Integer> TL = new ThreadLocal<>();

    static int checkIdentity(Thread expected, int expectedTl) {
        int bad = 0;
        for (int i = 0; i < 50; i++) {
            if (Thread.currentThread() != expected) {
                bad++;
            }
            Integer v = TL.get();
            if (v == null || v != expectedTl) {
                bad++;
            }
        }
        return bad;
    }

    static void identity() throws InterruptedException {
        AtomicInteger bad = new AtomicInteger();
        Thread[] ts = new Thread[50];
        for (int k = 0; k < ts.length; k++) {
            final int id = k;
            ts[k] = Thread.ofVirtual().unstarted(() -> {
                TL.set(id);
                Thread me = Thread.currentThread();
                for (int round = 0; round < 40; round++) {
                    bad.addAndGet(checkIdentity(me, id));
                    LockSupport.parkNanos(1_000);
                }
            });
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("identity: mismatches=" + bad.get());
    }

    // Exceptions two calls down a hot helper.
    static int divide(int a, int b) {
        return a / b;
    }

    static int middle(int a, int b) {
        return divide(a, b) + 1;
    }

    static int guarded(int a, int b) {
        try {
            return middle(a, b);
        } catch (ArithmeticException e) {
            return -1;
        }
    }

    static void exceptions() throws InterruptedException {
        AtomicInteger caught = new AtomicInteger();
        AtomicInteger escaped = new AtomicInteger();
        String[] message = new String[1];
        String[] top = new String[1];
        Thread t = Thread.ofVirtual().start(() -> {
            for (int i = 0; i < 20_000; i++) {
                guarded(i, 1 + (i & 7));
            }
            for (int i = 0; i < 200; i++) {
                if ((i & 3) != 0) {
                    if (guarded(i, 0) == -1) {
                        caught.incrementAndGet();
                    }
                } else {
                    try {
                        middle(i, 0);
                    } catch (ArithmeticException e) {
                        escaped.incrementAndGet();
                        message[0] = e.getMessage();
                        StackTraceElement[] st = e.getStackTrace();
                        top[0] = st[0].getMethodName() + "," + st[1].getMethodName();
                    }
                }
                if ((i & 31) == 0) {
                    LockSupport.parkNanos(1_000);
                }
            }
        });
        t.join();
        System.out.println("exceptions: caught=" + caught.get() + " escaped=" + escaped.get()
                + " message=" + message[0] + " top=" + top[0]);
    }

    // A contended synchronized block inside a hot helper.
    static final Object LOCK = new Object();
    static int counter;

    static void bump(int n) {
        for (int i = 0; i < n; i++) {
            synchronized (LOCK) {
                counter++;
            }
        }
    }

    static void monitor() throws InterruptedException {
        Thread[] ts = new Thread[8];
        for (int k = 0; k < ts.length; k++) {
            ts[k] = Thread.ofVirtual().start(() -> {
                for (int round = 0; round < 100; round++) {
                    bump(100);
                    if ((round & 7) == 0) {
                        Thread.yield();
                    }
                }
            });
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("monitor: count=" + counter);
    }

    // A deopt (a new receiver class after warm-up), then a park.
    static class Base {
        int val(int i) {
            return i;
        }
    }

    static class Late extends Base {
        @Override
        int val(int i) {
            return i * 2;
        }
    }

    static long sumVals(Base b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += b.val(i);
        }
        LockSupport.parkNanos(1_000);
        return s;
    }

    static void deopt() throws InterruptedException {
        boolean[] ok = new boolean[2];
        Thread t = Thread.ofVirtual().start(() -> {
            Base b = new Base();
            boolean good = true;
            for (int i = 0; i < 3_000; i++) {
                good &= sumVals(b, 100) == 4950;
            }
            ok[0] = good;
            Base late = new Late();
            boolean good2 = true;
            for (int i = 0; i < 50; i++) {
                good2 &= sumVals(late, 100) == 9900;
                good2 &= sumVals(b, 100) == 4950;
            }
            ok[1] = good2;
        });
        t.join();
        System.out.println("deopt: before=" + ok[0] + " after=" + ok[1]);
    }

    // Thread.sleep inside a hot helper.
    static long sleeper(int round) throws InterruptedException {
        long s = 0;
        for (int i = 0; i < 100; i++) {
            s += hashOf("s" + i);
        }
        if (round % 1000 == 999) {
            Thread.sleep(1);
        }
        return s;
    }

    static void sleep() throws InterruptedException {
        AtomicInteger done = new AtomicInteger();
        Thread[] ts = new Thread[8];
        for (int k = 0; k < ts.length; k++) {
            ts[k] = Thread.ofVirtual().start(() -> {
                try {
                    for (int r = 0; r < 3000; r++) {
                        sleeper(r);
                    }
                    done.incrementAndGet();
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            });
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("sleep: done=" + done.get());
    }

    public static void main(String[] args) throws Exception {
        long v = run(true, 200);
        long p = run(false, 200);
        System.out.println("helpers: virtual=platform " + (v == p));
        handOff();
        identity();
        exceptions();
        monitor();
        deopt();
        sleep();
    }
}
