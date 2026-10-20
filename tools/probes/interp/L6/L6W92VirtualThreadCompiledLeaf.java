// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L6 -- compiled leaf callees on JDK
// virtual threads (`CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY`, default off; page
// i91-L6-virtual-thread-code-is-never-compiled). The output must be the same
// with the switch off and on, and HotSpot 25's (which compiles these callees
// whatever thread runs them):
// * a hot array-sum leaf on 200 virtual threads answers the platform sum
//   (a[i] = i*7 % 1000 permutes 0..999, so one call is 499500);
// * an ArithmeticException / ArrayIndexOutOfBoundsException raised inside a
//   hot leaf on a virtual thread keeps HotSpot's messages and its two top
//   frames (the leaf, then its caller);
// * virtual threads that alternate hot leaf calls with Thread.sleep,
//   Thread.yield, LockSupport.park and a contended monitor on ONE carrier
//   all finish (each yield happens with no compiled frame on the stack);
// * Thread.currentThread() inside the loop is still the virtual thread.
//
// Run, each mode, with the switch off and on (the output must not change):
//   cratonvm -Djdk.virtualThreadScheduler.parallelism=1 L6W92VirtualThreadCompiledLeaf
//   CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY=1 cratonvm -Djdk.virtualThreadScheduler.parallelism=1 L6W92VirtualThreadCompiledLeaf
//   (and both with --compatible; CRATONVM_VTHREAD_MONITOR_UNMOUNT=0 too)
//
// HOTSPOT_EXPECTED_BEGIN
// sum on virtual threads: 19980000000 platform: 19980000000
// arithmetic: java.lang.ArithmeticException / by zero top=div,caller
// bounds: java.lang.ArrayIndexOutOfBoundsException Index 5 out of bounds for length 3 top=get,caller
// interleaved: done=4 virtual=true
// monitor: done=2 counter=4000
// HOTSPOT_EXPECTED_END

import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.locks.LockSupport;

public class L6W92VirtualThreadCompiledLeaf {
    // The leaves: no invoke, no monitor op, not synchronized.
    static long sum(int[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static int div(int a, int b) {
        return a / b;
    }

    static int get(int[] a, int i) {
        return a[i];
    }

    static int caller(int a, int b) {
        return div(a, b);
    }

    static int callerGet(int[] a, int i) {
        return get(a, i);
    }

    static String top(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st[0].getMethodName() + "," + st[1].getMethodName();
    }

    public static void main(String[] args) throws Exception {
        if (System.getProperty("jdk.virtualThreadScheduler.parallelism") == null) {
            System.setProperty("jdk.virtualThreadScheduler.parallelism", "1");
        }
        int[] data = new int[1000];
        for (int i = 0; i < data.length; i++) {
            data[i] = i * 7 % 1000;
        }

        // A hot leaf on many virtual threads, against a platform thread.
        AtomicLong vtotal = new AtomicLong();
        Thread[] vts = new Thread[200];
        for (int t = 0; t < vts.length; t++) {
            vts[t] = Thread.ofVirtual().start(() -> {
                long s = 0;
                for (int k = 0; k < 200; k++) {
                    s += sum(data);
                }
                vtotal.addAndGet(s);
            });
        }
        for (Thread t : vts) {
            t.join();
        }
        AtomicLong ptotal = new AtomicLong();
        Thread p = Thread.ofPlatform().start(() -> {
            long s = 0;
            for (int k = 0; k < 40_000; k++) {
                s += sum(data);
            }
            ptotal.set(s);
        });
        p.join();
        System.out.println("sum on virtual threads: " + vtotal.get() + " platform: " + ptotal.get());

        // Exceptions raised in a hot leaf on a virtual thread.
        AtomicReference<String> arith = new AtomicReference<>();
        AtomicReference<String> bounds = new AtomicReference<>();
        Thread ex = Thread.ofVirtual().start(() -> {
            int[] small = {1, 2, 3};
            int acc = 0;
            for (int k = 0; k < 20_000; k++) {
                acc += caller(k, 3) + callerGet(small, k % 3);
            }
            try {
                acc += caller(acc, 0);
            } catch (ArithmeticException e) {
                arith.set(e.getClass().getName() + " " + e.getMessage() + " top=" + top(e));
            }
            try {
                acc += callerGet(small, 5);
            } catch (ArrayIndexOutOfBoundsException e) {
                bounds.set(e.getClass().getName() + " " + e.getMessage() + " top=" + top(e));
            }
        });
        ex.join();
        System.out.println("arithmetic: " + arith.get());
        System.out.println("bounds: " + bounds.get());

        // Hot leaf calls between every kind of yield, on one carrier.
        AtomicInteger done = new AtomicInteger();
        AtomicInteger allVirtual = new AtomicInteger();
        Thread[] mixed = new Thread[4];
        for (int t = 0; t < mixed.length; t++) {
            final int kind = t;
            mixed[t] = Thread.ofVirtual().start(() -> {
                boolean virtual = true;
                for (int k = 0; k < 200; k++) {
                    for (int j = 0; j < 20; j++) {
                        sum(data);
                    }
                    virtual &= Thread.currentThread().isVirtual();
                    try {
                        switch (kind) {
                            case 0 -> Thread.sleep(1);
                            case 1 -> Thread.yield();
                            case 2 -> LockSupport.parkNanos(100_000);
                            default -> Thread.sleep(0, 500);
                        }
                    } catch (InterruptedException e) {
                        throw new RuntimeException(e);
                    }
                }
                if (virtual) {
                    allVirtual.incrementAndGet();
                }
                done.incrementAndGet();
            });
        }
        for (Thread t : mixed) {
            t.join(TimeUnit.SECONDS.toMillis(60));
        }
        System.out.println("interleaved: done=" + done.get() + " virtual=" + (allVirtual.get() == mixed.length));

        // A contended monitor between hot leaf calls (the monitorenter unmounts
        // under CRATONVM_VTHREAD_MONITOR_UNMOUNT; no compiled frame is below it).
        Object lock = new Object();
        AtomicInteger mdone = new AtomicInteger();
        int[] counter = new int[1];
        Thread[] lockers = new Thread[2];
        for (int t = 0; t < lockers.length; t++) {
            lockers[t] = Thread.ofVirtual().start(() -> {
                for (int k = 0; k < 2000; k++) {
                    long s = sum(data);
                    synchronized (lock) {
                        counter[0] += (s == 499500L) ? 1 : 0;
                        if (k % 100 == 0) {
                            Thread.yield();
                        }
                    }
                }
                mdone.incrementAndGet();
            });
        }
        for (Thread t : lockers) {
            t.join(TimeUnit.SECONDS.toMillis(60));
        }
        synchronized (lock) {
            System.out.println("monitor: done=" + mdone.get() + " counter=" + counter[0]);
        }
        System.exit(0);
    }
}
