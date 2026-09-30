// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 25, lane L7: a virtual thread that unmounts inside
// a lambda body or a method-reference target keeps every frame of its
// continuation, and the remounted target's return value reaches the SAM call
// converted as the lambda proxy converts it.
//
// CratonVM serves a lambda proxy's SAM call through a nested Rust call into
// the impl method (`try_lambda_dispatch` -> `try_invoke_cached_lambda_impl` ->
// `run_pushed_frame_to_completion`). That runner popped every frame above its
// own on a continuation yield, so the remount resumed the frame that made the
// SAM call past its invoke, as if the callee had returned. A bound method
// reference (`latch::await`, `queue::take`) takes that runner from its first
// call; a static lambda body takes it once a previous call recorded its owner
// (`lambda_global_impl_owner`) -- here, the `warm` rows run the same lambda on
// a platform thread first.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L7W25ContinuationBoundaries
//
// Compare against HotSpot 25 run with
// `-XX:+UnlockExperimentalVMOptions -XX:-VMContinuations` (CratonVM's virtual
// threads are `BoundVirtualThread`s; see `L7W23Interrupts`; without the flag
// only `warm virtual interrupted` differs, message `null`). With that flag
// HotSpot 25 prints exactly:
//     warm platform done
//     warm virtual parked, unparked, done
//     warm virtual interrupted java.lang.InterruptedException: sleep interrupted
//     bound latch interrupted java.lang.InterruptedException: null flag=false
//     bound latch counted down returned flag=false
//     bound take value x
//     discarded take returned depth=12
//     boxed barrier 1 java.lang.Integer
//     unboxed future 42
//
// CratonVM wave 24 (host, `--jdk-only`, JIT and `--nojit`): the
// `L7W24VirtualInterrupts` rows `mid latch` / `mid queueTake` printed
// `returned flag=true prompt=true` and the `L7W23Interrupts` `virtual` rows
// printed `null` -- the same defect as this probe's `bound latch interrupted`
// and `warm virtual` rows (read from the code: `warm virtual` rows print
// `null`, the `bound` rows `returned` / `null`, `boxed barrier` a raw int or
// an internal error).
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.Callable;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.CyclicBarrier;
import java.util.concurrent.locks.LockSupport;

public class L7W25ContinuationBoundaries {
    interface Body {
        void run() throws Throwable;
    }

    interface IntCall {
        int call() throws Exception;
    }

    static String outcome(Body b) {
        try {
            b.run();
            return "returned";
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    /** One lambda expression, so every call returns the same proxy class. */
    static Runnable task(String[] out, int mode) {
        return () -> {
            if (mode == 1) {
                LockSupport.park();
                out[0] = "parked, unparked, done";
            } else if (mode == 2) {
                out[0] = outcome(() -> Thread.sleep(60_000));
            } else {
                out[0] = "done";
            }
        };
    }

    static void awaitBlocked(Thread t) throws Exception {
        long end = System.nanoTime() + 5_000_000_000L;
        while (t.getState() != Thread.State.WAITING && t.getState() != Thread.State.TIMED_WAITING
                && System.nanoTime() < end) {
            Thread.sleep(2);
        }
        Thread.sleep(20);
    }

    static int depth(int n) {
        return n == 0 ? 0 : 1 + depth(n - 1);
    }

    public static void main(String[] args) throws Exception {
        // Warm the lambda on a platform thread: its owner is recorded, and the
        // virtual runs below take the cached nested-call path.
        String[] out = new String[1];
        Thread p = new Thread(task(out, 0));
        p.start();
        p.join();
        System.out.println("warm platform " + out[0]);

        out[0] = null;
        Thread v = Thread.ofVirtual().start(task(out, 1));
        awaitBlocked(v);
        LockSupport.unpark(v);
        v.join(10_000);
        System.out.println("warm virtual " + out[0]);

        out[0] = null;
        v = Thread.ofVirtual().start(task(out, 2));
        awaitBlocked(v);
        v.interrupt();
        v.join(10_000);
        System.out.println("warm virtual interrupted " + out[0]);

        // A bound method reference: the impl (`CountDownLatch.await`) is
        // entered through the nested call from its first dispatch.
        CountDownLatch never = new CountDownLatch(1);
        Body awaitNever = never::await;
        String[] r = new String[1];
        v = Thread.ofVirtual().start(() -> {
            String o = outcome(awaitNever);
            r[0] = o + " flag=" + Thread.currentThread().isInterrupted();
        });
        awaitBlocked(v);
        v.interrupt();
        v.join(10_000);
        System.out.println("bound latch interrupted " + r[0]);

        CountDownLatch once = new CountDownLatch(1);
        Body awaitOnce = once::await;
        r[0] = null;
        v = Thread.ofVirtual().start(() -> {
            String o = outcome(awaitOnce);
            r[0] = o + " flag=" + Thread.currentThread().isInterrupted();
        });
        awaitBlocked(v);
        once.countDown();
        v.join(10_000);
        System.out.println("bound latch counted down " + r[0]);

        // Reference to reference: the value passes unchanged.
        ArrayBlockingQueue<Object> q = new ArrayBlockingQueue<>(1);
        Callable<Object> take = q::take;
        r[0] = null;
        v = Thread.ofVirtual().start(() -> {
            try {
                r[0] = String.valueOf(take.call());
            } catch (Exception e) {
                r[0] = e.toString();
            }
        });
        awaitBlocked(v);
        q.put("x");
        v.join(10_000);
        System.out.println("bound take value " + r[0]);

        // A value-returning impl behind a `void` SAM: the value is dropped, and
        // the caller's operand stack stays balanced for the work after it.
        Body takeAndDrop = q::take;
        r[0] = null;
        v = Thread.ofVirtual().start(() -> {
            String o = outcome(takeAndDrop);
            r[0] = o + " depth=" + depth(12);
        });
        awaitBlocked(v);
        q.put("y");
        v.join(10_000);
        System.out.println("discarded take " + r[0]);

        // int impl behind an Object SAM: the proxy boxes it.
        CyclicBarrier barrier = new CyclicBarrier(2);
        Callable<Integer> arrive = barrier::await;
        Object[] boxed = new Object[1];
        v = Thread.ofVirtual().start(() -> {
            try {
                boxed[0] = arrive.call();
            } catch (Exception e) {
                boxed[0] = e;
            }
        });
        awaitBlocked(v);
        barrier.await();
        v.join(10_000);
        System.out.println("boxed barrier " + boxed[0]
                + (boxed[0] == null ? "" : " " + boxed[0].getClass().getName()));

        // Object impl behind an int SAM: the proxy casts and unboxes it.
        CompletableFuture<Integer> future = new CompletableFuture<>();
        IntCall get = future::get;
        int[] got = new int[1];
        r[0] = null;
        v = Thread.ofVirtual().start(() -> {
            try {
                got[0] = get.call();
                r[0] = "" + got[0];
            } catch (Exception e) {
                r[0] = e.toString();
            }
        });
        awaitBlocked(v);
        future.complete(42);
        v.join(10_000);
        System.out.println("unboxed future " + r[0]);
    }
}
