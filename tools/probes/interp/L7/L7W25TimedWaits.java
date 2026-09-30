// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 25, lane L7: sub-millisecond `TimeUnit.sleep`
// durations are slept, and a `LockSupport` permit ends a TIMED park on a
// virtual thread (found in wave 24, not pinned down then).
//
// `micro sleeps`: forty `MICROSECONDS.sleep(900)` must take at least 36 ms.
// `--compatible` served `TimeUnit.sleep` with a native that truncated to whole
// milliseconds, so each one slept zero. (`--jdk-only` runs the JDK bytecode,
// `Thread.sleep(ms, ns)`, and already matched.)
// `nano sleep interrupted`: a pre-interrupted one-nanosecond sleep throws.
// `... parkNanos after unpark`: a thread that holds a permit returns from
// `parkNanos(5 s)` at once. A virtual thread used to wait out the whole
// timeout: `suspend_runtime` honoured a sticky permit for untimed yields only.
// `virtual sleep after unpark`: a permit does not end a `Thread.sleep`.
// `... interrupted sleep`: the interrupt that ends a sleep also leaves a
// `LockSupport` permit (HotSpot unparks the `Parker`), so the next `parkNanos`
// returns at once; a virtual thread's resubmit used to take that permit.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L7W25TimedWaits
//
// HotSpot 25 (with or without `-XX:+UnlockExperimentalVMOptions
// -XX:-VMContinuations`) prints exactly:
//     micro sleeps slept=true
//     nano sleep interrupted java.lang.InterruptedException: sleep interrupted flag=false
//     platform parkNanos after unpark prompt=true
//     virtual parkNanos after unpark prompt=true
//     virtual sleep after unpark full=true
//     platform interrupted sleep IE then parkNanos prompt=true
//     virtual interrupted sleep IE then parkNanos prompt=true
//
// CratonVM before wave 25 (read from the code): `micro sleeps slept=false`
// under `--compatible`; `virtual parkNanos after unpark prompt=false` and
// `virtual interrupted sleep IE then parkNanos prompt=false` in both modes.
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;

public class L7W25TimedWaits {
    public static void main(String[] args) throws Exception {
        long t0 = System.nanoTime();
        for (int i = 0; i < 40; i++) {
            TimeUnit.MICROSECONDS.sleep(900);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("micro sleeps slept=" + (ms >= 36));

        Thread.currentThread().interrupt();
        String o;
        try {
            TimeUnit.NANOSECONDS.sleep(1);
            o = "returned";
        } catch (InterruptedException e) {
            o = e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println("nano sleep interrupted " + o + " flag=" + Thread.interrupted());

        System.out.println("platform parkNanos after unpark prompt=" + parkAfterUnpark());

        boolean[] prompt = new boolean[1];
        Thread v = Thread.ofVirtual().start(() -> prompt[0] = parkAfterUnpark());
        v.join(20_000);
        System.out.println("virtual parkNanos after unpark prompt=" + prompt[0]);

        boolean[] full = new boolean[1];
        v = Thread.ofVirtual().start(() -> {
            LockSupport.unpark(Thread.currentThread());
            long s = System.nanoTime();
            try {
                Thread.sleep(300);
            } catch (InterruptedException e) {
                return;
            }
            full[0] = (System.nanoTime() - s) / 1_000_000 >= 299;
            // consume the permit the sleep kept
            LockSupport.park();
        });
        v.join(20_000);
        System.out.println("virtual sleep after unpark full=" + full[0]);

        System.out.println("platform interrupted sleep " + interruptedSleepThenPark(false));
        System.out.println("virtual interrupted sleep " + interruptedSleepThenPark(true));
    }

    /**
     * HotSpot's interrupt unparks the `Parker` as well, and a sleep does not
     * consume that permit: the `parkNanos` after the InterruptedException
     * returns at once.
     */
    static String interruptedSleepThenPark(boolean virtual) throws Exception {
        String[] r = new String[1];
        Runnable body = () -> {
            try {
                Thread.sleep(60_000);
                r[0] = "returned";
            } catch (InterruptedException e) {
                r[0] = "IE";
            }
            long s = System.nanoTime();
            LockSupport.parkNanos(3_000_000_000L);
            r[0] += " then parkNanos prompt=" + ((System.nanoTime() - s) / 1_000_000 < 1_500);
        };
        Thread t = virtual ? Thread.ofVirtual().unstarted(body) : new Thread(body);
        t.start();
        long end = System.nanoTime() + 5_000_000_000L;
        while (t.getState() != Thread.State.TIMED_WAITING && System.nanoTime() < end) {
            Thread.sleep(2);
        }
        Thread.sleep(20);
        t.interrupt();
        t.join(20_000);
        return r[0];
    }

    static boolean parkAfterUnpark() {
        LockSupport.unpark(Thread.currentThread());
        long s = System.nanoTime();
        LockSupport.parkNanos(5_000_000_000L);
        return (System.nanoTime() - s) / 1_000_000 < 2_000;
    }
}
