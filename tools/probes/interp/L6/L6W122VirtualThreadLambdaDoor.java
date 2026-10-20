// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 122, lane L6 -- correctness of
// CRATONVM_VTHREAD_LAMBDA_DOOR (the interpreted lambda door on a JDK virtual
// thread, behind the virtual door; proposal
// i114-L6-proposal-the-virtual-door-on-virtual-threads, "Wave 122").
// Hot lambda calls through their interfaces on 16 virtual threads with parks
// between them: capturing and non-capturing, a long capture, a lambda whose
// body parks, a failing replayed checkcast (ClassCastException at the call),
// an exception thrown in the body, and a method reference.
// The output must be identical with the switch off and on, in both modes,
// JIT and --nojit, and equal HotSpot 25's.
//
// Run, each mode (and --nojit):
//   cratonvm -Djdk.virtualThreadScheduler.parallelism=2 L6W122VirtualThreadLambdaDoor
//   CRATONVM_VTHREAD_LAMBDA_DOOR=1 cratonvm -Djdk.virtualThreadScheduler.parallelism=2 L6W122VirtualThreadLambdaDoor
// Door census: CRATONVM_DBG_FIELD_SITE=1 with the switch on shows lambda-door
// hits on the virtual rows.
//
// HOTSPOT_EXPECTED_BEGIN
// plain: sum=11999200
// capture: sum=9598800
// long-capture: sum=8000000008000000
// parking: sum=2280000
// cast: caught=16 sum=32
// throw: caught=16 message=boom-7
// method-ref: sum=1984000
// HOTSPOT_EXPECTED_END

import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;
import java.util.function.Function;
import java.util.function.IntBinaryOperator;
import java.util.function.IntUnaryOperator;
import java.util.function.LongUnaryOperator;

public class L6W122VirtualThreadLambdaDoor {
    static final int THREADS = 16;

    interface Task {
        void run(int id) throws Exception;
    }

    static void runAll(Task task) throws InterruptedException {
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int id = t;
            ts[t] = Thread.ofVirtual().unstarted(() -> {
                try {
                    task.run(id);
                } catch (Exception e) {
                    throw new RuntimeException(e);
                }
            });
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
    }

    static int twice(int x) {
        return x * 2;
    }

    public static void main(String[] args) throws Exception {
        AtomicLong plain = new AtomicLong();
        runAll(id -> {
            IntUnaryOperator op = x -> x + 1;
            long s = 0;
            for (int i = 0; i < 50_000; i++) {
                s += op.applyAsInt(i % 29);
                if (i % 10_000 == 0) {
                    LockSupport.parkNanos(1_000);
                }
            }
            plain.addAndGet(s);
        });
        System.out.println("plain: sum=" + plain.get());

        AtomicLong capture = new AtomicLong();
        runAll(id -> {
            int k = 3;
            int j = 4;
            IntBinaryOperator op = (a, b) -> a * k + b * j;
            long s = 0;
            for (int i = 0; i < 20_000; i++) {
                s += op.applyAsInt(i % 13, i % 7);
                if (i % 5_000 == 0) {
                    LockSupport.parkNanos(1_000);
                }
            }
            capture.addAndGet(s);
        });
        System.out.println("capture: sum=" + capture.get());

        AtomicLong longCapture = new AtomicLong();
        runAll(id -> {
            long base = 1_000_000_000L;
            LongUnaryOperator op = x -> x + base;
            long s = 0;
            for (int i = 0; i < 500_000; i++) {
                s += op.applyAsLong(1);
                if (i % 100_000 == 0) {
                    LockSupport.parkNanos(1_000);
                }
            }
            longCapture.addAndGet(s);
        });
        System.out.println("long-capture: sum=" + longCapture.get());

        AtomicLong parking = new AtomicLong();
        runAll(id -> {
            IntUnaryOperator op = x -> {
                if (x % 100 == 0) {
                    LockSupport.parkNanos(1_000);
                }
                return x + 3;
            };
            long s = 0;
            for (int i = 0; i < 1_000; i++) {
                s += op.applyAsInt(i % 300);
            }
            parking.addAndGet(s);
        });
        System.out.println("parking: sum=" + parking.get());

        AtomicInteger castCaught = new AtomicInteger();
        AtomicLong castSum = new AtomicLong();
        runAll(id -> {
            @SuppressWarnings({"unchecked", "rawtypes"})
            Function<Object, Integer> raw = (Function) (Function<String, Integer>) String::length;
            long s = 0;
            for (int i = 0; i < 2_000; i++) {
                s += raw.apply("ab") - 2;
            }
            try {
                raw.apply(Integer.valueOf(5));
            } catch (ClassCastException e) {
                castCaught.incrementAndGet();
            }
            castSum.addAndGet(s + 2);
        });
        System.out.println("cast: caught=" + castCaught.get() + " sum=" + castSum.get());

        AtomicInteger thrown = new AtomicInteger();
        String[] message = {""};
        runAll(id -> {
            IntUnaryOperator op = x -> {
                if (x == 7) {
                    throw new IllegalStateException("boom-" + x);
                }
                return x;
            };
            for (int i = 0; i < 3_000; i++) {
                op.applyAsInt(i % 7);
            }
            try {
                op.applyAsInt(7);
            } catch (IllegalStateException e) {
                thrown.incrementAndGet();
                synchronized (message) {
                    message[0] = e.getMessage();
                }
            }
        });
        System.out.println("throw: caught=" + thrown.get() + " message=" + message[0]);

        AtomicLong ref = new AtomicLong();
        runAll(id -> {
            IntUnaryOperator op = L6W122VirtualThreadLambdaDoor::twice;
            long s = 0;
            for (int i = 0; i < 4_000; i++) {
                s += op.applyAsInt(i % 32);
                if (i % 1_000 == 0) {
                    LockSupport.parkNanos(1_000);
                }
            }
            ref.addAndGet(s);
        });
        System.out.println("method-ref: sum=" + ref.get());
    }
}
