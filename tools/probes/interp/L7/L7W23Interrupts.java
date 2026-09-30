// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L7 (bug hunt): `Thread.interrupt()` and
// the blocking primitives, and the ownership checks of `wait` / `notify`.
//
// For each primitive the row reports what the blocked call did when another
// thread interrupted it (the exception class and message, or "returned"), and
// whether the interrupt flag was still set afterwards. The `pre` rows
// interrupt BEFORE the call. The `own` rows call `wait` / `notify` /
// `notifyAll` without owning the monitor.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L7W23Interrupts
//
// Compare against HotSpot 25 run with
// `-XX:+UnlockExperimentalVMOptions -XX:-VMContinuations`: CratonVM's
// `ContinuationSupport.isSupported()` is false, so its virtual threads are
// `BoundVirtualThread`s, whose `sleep` goes through `sleep0` ("sleep
// interrupted"); with continuations HotSpot's `VirtualThread.sleepNanos`
// throws a message-less InterruptedException instead (the only row that
// differs). With that flag HotSpot 25 prints exactly:
//     sleep java.lang.InterruptedException: sleep interrupted flag=false
//     wait java.lang.InterruptedException: null flag=false
//     waitMs java.lang.InterruptedException: null flag=false
//     join java.lang.InterruptedException: null flag=false
//     joinMs java.lang.InterruptedException: null flag=false
//     park returned flag=true
//     parkNanos returned flag=true
//     latch java.lang.InterruptedException: null flag=false
//     queueTake java.lang.InterruptedException: null flag=false
//     pre sleep java.lang.InterruptedException: sleep interrupted flag=false
//     pre sleep0 java.lang.InterruptedException: sleep interrupted flag=false
//     pre wait java.lang.InterruptedException: null flag=false
//     pre join java.lang.InterruptedException: null flag=false
//     pre park returned flag=true
//     pre latch java.lang.InterruptedException: null flag=false
//     pre ownWait java.lang.IllegalMonitorStateException: current thread is not owner flag=true
//     interrupted() true then false
//     own wait java.lang.IllegalMonitorStateException: current thread is not owner
//     own waitMs java.lang.IllegalMonitorStateException: current thread is not owner
//     own notify java.lang.IllegalMonitorStateException: current thread is not owner
//     own notifyAll java.lang.IllegalMonitorStateException: current thread is not owner
//     own waitNeg java.lang.IllegalArgumentException: timeout value is negative
//     own waitNanos java.lang.IllegalArgumentException: nanosecond timeout value out of range
//     virtual sleep java.lang.InterruptedException: sleep interrupted flag=false
//     virtual park returned flag=true
//     virtual latch java.lang.InterruptedException: null flag=false
//
// CratonVM (read from the code, wave 23): `pre ownWait` answered
// InterruptedException with the flag cleared before the wave-23 fix
// (`NativeContextImpl::monitor_wait` checked the interrupt first); the three
// `virtual` rows print `null` after the 10 s join (page
// `interpreter-L7-an-interrupt-does-not-reach-an-unmounted-virtual-thread`,
// fixed in wave 24).
// CratonVM wave 23 as run by the orchestrator: `sleep` printed
// `InterruptedException: null` and `pre sleep0` printed `returned flag=true`
// (both fixed in wave 24: the `Thread.sleep` natives check the interrupt
// before the duration and throw HotSpot's "sleep interrupted" on every path;
// see `L7W24SleepInterrupts`).
// CratonVM wave 24 as run on the host: the three `virtual` rows still printed
// `null`. The Runnable lambda of `interrupted` had already run on nine
// platform threads, so the lambda door had recorded its owner and entered it
// through the nested `run_pushed_frame_to_completion`, which popped the
// continuation's frames on the yield: the remount resumed `Thread.runWith`
// past `op.run()` and the virtual thread ended without writing its result.
// Fixed in wave 25 (lane L7; `L7W25ContinuationBoundaries`, rows `warm`).
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;

public class L7W23Interrupts {
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

    /** Runs `body` on a new thread, interrupts it once it blocks, reports. */
    static void interrupted(String name, boolean virtual, Body body) throws Exception {
        String[] result = new String[1];
        Runnable r = () -> {
            String o = outcome(body);
            result[0] = o + " flag=" + Thread.currentThread().isInterrupted();
        };
        Thread t = virtual ? Thread.ofVirtual().unstarted(r) : new Thread(r);
        t.start();
        long end = System.nanoTime() + 5_000_000_000L;
        while (t.getState() != Thread.State.WAITING && t.getState() != Thread.State.TIMED_WAITING
                && System.nanoTime() < end) {
            Thread.sleep(2);
        }
        Thread.sleep(20);
        t.interrupt();
        t.join(10_000);
        System.out.println((virtual ? "virtual " : "") + name + " " + result[0]);
    }

    /** Interrupts the calling thread, then runs `body` on it. */
    static void pre(String name, Body body) {
        Thread.currentThread().interrupt();
        String o = outcome(body);
        System.out.println("pre " + name + " " + o + " flag=" + Thread.interrupted());
    }

    public static void main(String[] args) throws Exception {
        Object lock = new Object();
        Thread forever = new Thread(() -> {
            try {
                Thread.sleep(60_000);
            } catch (InterruptedException e) {
            }
        });
        forever.setDaemon(true);
        forever.start();

        interrupted("sleep", false, () -> Thread.sleep(60_000));
        interrupted("wait", false, () -> {
            synchronized (lock) {
                lock.wait();
            }
        });
        interrupted("waitMs", false, () -> {
            synchronized (lock) {
                lock.wait(60_000);
            }
        });
        interrupted("join", false, forever::join);
        interrupted("joinMs", false, () -> forever.join(60_000));
        interrupted("park", false, LockSupport::park);
        interrupted("parkNanos", false, () -> LockSupport.parkNanos(60_000_000_000L));
        CountDownLatch never = new CountDownLatch(1);
        interrupted("latch", false, never::await);
        ArrayBlockingQueue<Object> q = new ArrayBlockingQueue<>(1);
        interrupted("queueTake", false, q::take);

        pre("sleep", () -> Thread.sleep(60_000));
        pre("sleep0", () -> Thread.sleep(0));
        pre("wait", () -> {
            synchronized (lock) {
                lock.wait();
            }
        });
        pre("join", forever::join);
        pre("park", LockSupport::park);
        pre("latch", never::await);
        // Interrupted AND not the owner: the ownership check comes first.
        pre("ownWait", lock::wait);

        Thread.currentThread().interrupt();
        boolean first = Thread.interrupted();
        boolean second = Thread.interrupted();
        System.out.println("interrupted() " + first + " then " + second);

        System.out.println("own wait " + outcome(lock::wait));
        System.out.println("own waitMs " + outcome(() -> lock.wait(10)));
        System.out.println("own notify " + outcome(lock::notify));
        System.out.println("own notifyAll " + outcome(lock::notifyAll));
        System.out.println("own waitNeg " + outcome(() -> {
            synchronized (lock) {
                lock.wait(-1);
            }
        }));
        System.out.println("own waitNanos " + outcome(() -> {
            synchronized (lock) {
                lock.wait(1, 1_000_000);
            }
        }));

        interrupted("sleep", true, () -> Thread.sleep(60_000));
        interrupted("park", true, LockSupport::park);
        interrupted("latch", true, never::await);
    }
}
