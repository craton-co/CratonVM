// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.time.Duration;

/**
 * SBR round 5, lane {@code defects}: every {@code Thread.sleep} entry point returns, sleeps at
 * least what it was asked and throws what HotSpot throws. Positive control for
 * {@code Thread.sleep0(J)V} reading its argument as NANOS: on a JDK 21 boot image under
 * {@code --jdk-only} (java.home unset or a 21), {@code sleep(50)} asked for 50,000,000 ms and never
 * returned. Run it with the JDK 21 image too (no {@code JAVA_HOME}), not only 25.
 */
public final class Sbr5DefectsSleep {
    static void line(String k, Object v) {
        System.out.println(k + " = " + v);
    }

    static boolean within(long startNanos, long minMs, long maxMs) {
        long ms = (System.nanoTime() - startNanos) / 1_000_000;
        return ms >= minMs && ms < maxMs;
    }

    public static void main(String[] a) throws Exception {
        long t = System.nanoTime();
        Thread.sleep(50);
        line("sleep(50)", within(t, 50, 5000));
        t = System.nanoTime();
        Thread.sleep(0);
        line("sleep(0)", within(t, 0, 5000));
        t = System.nanoTime();
        Thread.sleep(20, 500_000);
        line("sleep(20,500000)", within(t, 20, 5000));
        t = System.nanoTime();
        Thread.sleep(Duration.ofMillis(30));
        line("sleep(Duration 30ms)", within(t, 30, 5000));
        t = System.nanoTime();
        Thread.sleep(Duration.ofNanos(1_500_000));
        line("sleep(Duration 1.5ms)", within(t, 1, 5000));
        try {
            Thread.sleep(-1);
        } catch (IllegalArgumentException e) {
            line("sleep(-1)", e.getMessage());
        }
        // interrupted while sleeping: a long sleep ends promptly with the HotSpot message
        Thread main = Thread.currentThread();
        Thread waker = new Thread(() -> {
            try {
                Thread.sleep(100);
            } catch (InterruptedException e) {
                // not expected
            }
            main.interrupt();
        });
        waker.start();
        t = System.nanoTime();
        try {
            Thread.sleep(60_000);
            line("interrupt", "NOT interrupted");
        } catch (InterruptedException e) {
            line("interrupt", e.getMessage() + " prompt=" + within(t, 0, 10_000) + " flag=" + main.isInterrupted());
        }
        waker.join();
        // pre-interrupted: every duration throws, including zero
        main.interrupt();
        try {
            Thread.sleep(0);
            line("pre-interrupted sleep(0)", "returned");
        } catch (InterruptedException e) {
            line("pre-interrupted sleep(0)", e.getMessage() + " flag=" + main.isInterrupted());
        }
        // virtual thread sleep
        Thread vt = Thread.ofVirtual().start(() -> {
            try {
                Thread.sleep(40);
                line("virtual sleep(40)", "ok");
            } catch (InterruptedException e) {
                line("virtual sleep(40)", "interrupted");
            }
        });
        vt.join();
        line("done", true);
    }
}
