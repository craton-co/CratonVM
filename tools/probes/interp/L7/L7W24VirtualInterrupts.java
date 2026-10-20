// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 24, lane L7: `Thread.interrupt()` and
// `LockSupport.unpark` reaching an UNMOUNTED virtual thread (page
// `interpreter-L7-an-interrupt-does-not-reach-an-unmounted-virtual-thread`).
//
// Each `mid` row starts a virtual thread that blocks, waits until it reports
// WAITING / TIMED_WAITING (i.e. its continuation has yielded), interrupts it,
// and prints what the blocked call did, the interrupt flag afterwards, and
// whether it ended well before its 60 s duration. The `unpark` row
// `LockSupport.unpark`s a virtual thread in a 400 ms `Thread.sleep`: a sleep
// does not end on an unpark, so it must still sleep at least 400 ms, and the
// permit survives the sleep, so a `LockSupport.park()` after it returns. The
// `pre` rows interrupt the virtual thread before the call.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L7W24VirtualInterrupts
//
// Compare against HotSpot 25 run with
// `-XX:+UnlockExperimentalVMOptions -XX:-VMContinuations` (CratonVM's
// `ContinuationSupport.isSupported()` is false, so its virtual threads are
// `BoundVirtualThread`s; see `L7W23Interrupts`). With that flag HotSpot 25
// prints exactly:
//     mid sleep java.lang.InterruptedException: sleep interrupted flag=false prompt=true
//     mid sleepNanos java.lang.InterruptedException: sleep interrupted flag=false prompt=true
//     mid park returned flag=true prompt=true
//     mid parkNanos returned flag=true prompt=true
//     mid latch java.lang.InterruptedException: null flag=false prompt=true
//     mid latchTimed java.lang.InterruptedException: null flag=false prompt=true
//     mid queueTake java.lang.InterruptedException: null flag=false prompt=true
//     pre sleep java.lang.InterruptedException: sleep interrupted flag=false
//     pre sleep0 java.lang.InterruptedException: sleep interrupted flag=false
//     pre park returned flag=true
//     pre latch java.lang.InterruptedException: null flag=false
//     unpark sleep returned flag=false full=true then park returned
//     after interrupt, next sleep returned flag=false
//
// CratonVM before wave 24 (read from the code): every `mid` row printed
// `null` after the 10 s join (the interrupt set a flag no unmounted
// continuation read, and a woken one resumed after the native as if it had
// returned); `unpark sleep` printed `full=false` (`unpark_virtual` resubmits
// any parked continuation, and the remount skipped the rest of the sleep);
// `pre sleep0` returned with flag=true (the interrupt check sat inside
// `millis > 0`).
// CratonVM wave 24 as run on the host (`--jdk-only`, JIT and `--nojit`): every
// row matched except `mid latch` and `mid queueTake`, which printed
// `returned flag=true prompt=true`. `never::await` / `q::take` are bound
// method references, which the lambda door enters through a nested
// `run_pushed_frame_to_completion`; that runner popped the continuation's
// frames on the yield, so the remount resumed `outcome` past `b.run()`.
// Fixed in wave 25 (lane L7; `L7W25ContinuationBoundaries`).
import java.time.Duration;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;

public class L7W24VirtualInterrupts {
    interface Body {
        void run() throws Throwable;
    }

    static String outcome(Body b) {
        try {
            b.run();
            return "returned";
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static Thread startBlocked(Runnable r) throws Exception {
        Thread t = Thread.ofVirtual().unstarted(r);
        t.start();
        long end = System.nanoTime() + 5_000_000_000L;
        while (t.getState() != Thread.State.WAITING && t.getState() != Thread.State.TIMED_WAITING
                && System.nanoTime() < end) {
            Thread.sleep(2);
        }
        Thread.sleep(20);
        return t;
    }

    static void mid(String name, Body body) throws Exception {
        String[] result = new String[1];
        Thread t = startBlocked(() -> {
            long start = System.nanoTime();
            String o = outcome(body);
            long ms = (System.nanoTime() - start) / 1_000_000;
            result[0] = o + " flag=" + Thread.currentThread().isInterrupted()
                    + " prompt=" + (ms < 30_000);
        });
        t.interrupt();
        t.join(10_000);
        System.out.println("mid " + name + " " + result[0]);
    }

    static void pre(String name, Body body) throws Exception {
        String[] result = new String[1];
        Thread t = Thread.ofVirtual().start(() -> {
            Thread.currentThread().interrupt();
            String o = outcome(body);
            result[0] = o + " flag=" + Thread.interrupted();
        });
        t.join(10_000);
        System.out.println("pre " + name + " " + result[0]);
    }

    public static void main(String[] args) throws Exception {
        mid("sleep", () -> Thread.sleep(60_000));
        mid("sleepNanos", () -> Thread.sleep(Duration.ofSeconds(60)));
        mid("park", LockSupport::park);
        mid("parkNanos", () -> LockSupport.parkNanos(60_000_000_000L));
        CountDownLatch never = new CountDownLatch(1);
        mid("latch", never::await);
        mid("latchTimed", () -> never.await(60, TimeUnit.SECONDS));
        ArrayBlockingQueue<Object> q = new ArrayBlockingQueue<>(1);
        mid("queueTake", q::take);

        pre("sleep", () -> Thread.sleep(60_000));
        pre("sleep0", () -> Thread.sleep(0));
        pre("park", LockSupport::park);
        pre("latch", never::await);

        String[] result = new String[1];
        Thread sleeper = startBlocked(() -> {
            long start = System.nanoTime();
            String o = outcome(() -> Thread.sleep(400));
            long ms = (System.nanoTime() - start) / 1_000_000;
            // The unpark's permit outlives the sleep: this park returns at
            // once (without it the join below times out and prints null).
            LockSupport.park();
            result[0] = o + " flag=" + Thread.currentThread().isInterrupted()
                    + " full=" + (ms >= 399) + " then park returned";
        });
        LockSupport.unpark(sleeper);
        sleeper.join(10_000);
        System.out.println("unpark sleep " + result[0]);

        // The interrupt is consumed by the throw: the next sleep runs to its end.
        Thread again = Thread.ofVirtual().start(() -> {
            outcome(() -> {
                Thread.currentThread().interrupt();
                Thread.sleep(10);
            });
            result[0] = outcome(() -> Thread.sleep(10)) + " flag="
                    + Thread.currentThread().isInterrupted();
        });
        again.join(10_000);
        System.out.println("after interrupt, next sleep " + result[0]);
    }
}
