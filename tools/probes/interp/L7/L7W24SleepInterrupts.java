// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 24, lane L7: every `Thread.sleep` overload on an
// interrupted platform thread -- the interrupt raised BEFORE the call (`pre`)
// and DURING it (`mid`) -- plus the state a sleeping thread reports.
//
// Each row prints the exception class and message (or "returned"), the
// interrupt flag afterwards, and for the `mid` rows whether the sleep ended
// promptly (well before its 60 s duration).
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L7W24SleepInterrupts
//
// HotSpot 25 prints exactly:
//     pre sleep(0) java.lang.InterruptedException: sleep interrupted flag=false
//     pre sleep(1) java.lang.InterruptedException: sleep interrupted flag=false
//     pre sleep(0,0) java.lang.InterruptedException: sleep interrupted flag=false
//     pre sleep(0,1) java.lang.InterruptedException: sleep interrupted flag=false
//     pre sleep(Duration.ZERO) java.lang.InterruptedException: sleep interrupted flag=false
//     pre sleep(Duration.ofMillis(1)) java.lang.InterruptedException: sleep interrupted flag=false
//     pre TimeUnit.sleep(0) returned flag=true
//     sleep(0) returned flag=false
//     mid sleep(ms) java.lang.InterruptedException: sleep interrupted flag=false prompt=true
//     mid sleep(ms,ns) java.lang.InterruptedException: sleep interrupted flag=false prompt=true
//     mid sleep(Duration) java.lang.InterruptedException: sleep interrupted flag=false prompt=true
//     mid TimeUnit.sleep java.lang.InterruptedException: sleep interrupted flag=false prompt=true
//     state sleep(ms) TIMED_WAITING
//     state sleep(Duration) TIMED_WAITING
//
// CratonVM before this fix (read from the code): the `pre` zero rows
// returned with flag=true (the interrupt check sat inside `millis > 0` /
// `nanos > 0`); `mid sleep(ms)` printed `InterruptedException: null`; the
// `mid sleep(ms,ns)` / `mid sleep(Duration)` rows went through `sleepNanos0`,
// one uninterruptible `std::thread::sleep` of the whole 60 s, so they printed
// `null` after the 10 s join; `state sleep(Duration)` said `WAITING`.
import java.time.Duration;
import java.util.concurrent.TimeUnit;

public class L7W24SleepInterrupts {
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

    static void pre(String name, Body body) {
        Thread.currentThread().interrupt();
        String o = outcome(body);
        System.out.println("pre " + name + " " + o + " flag=" + Thread.interrupted());
    }

    static Thread startAndAwaitTimedWait(Runnable r) throws Exception {
        Thread t = new Thread(r);
        t.start();
        long end = System.nanoTime() + 5_000_000_000L;
        while (t.getState() != Thread.State.TIMED_WAITING && System.nanoTime() < end) {
            Thread.sleep(2);
        }
        Thread.sleep(20);
        return t;
    }

    static void mid(String name, Body body) throws Exception {
        String[] result = new String[1];
        Thread t = startAndAwaitTimedWait(() -> {
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

    static void state(String name, Body body) throws Exception {
        Thread t = startAndAwaitTimedWait(() -> outcome(body));
        System.out.println("state " + name + " " + t.getState());
        t.interrupt();
        t.join(10_000);
    }

    public static void main(String[] args) throws Exception {
        pre("sleep(0)", () -> Thread.sleep(0));
        pre("sleep(1)", () -> Thread.sleep(1));
        pre("sleep(0,0)", () -> Thread.sleep(0, 0));
        pre("sleep(0,1)", () -> Thread.sleep(0, 1));
        pre("sleep(Duration.ZERO)", () -> Thread.sleep(Duration.ZERO));
        pre("sleep(Duration.ofMillis(1))", () -> Thread.sleep(Duration.ofMillis(1)));
        // `TimeUnit.sleep` does nothing for a non-positive timeout.
        pre("TimeUnit.sleep(0)", () -> TimeUnit.MILLISECONDS.sleep(0));
        System.out.println("sleep(0) " + outcome(() -> Thread.sleep(0))
                + " flag=" + Thread.currentThread().isInterrupted());

        mid("sleep(ms)", () -> Thread.sleep(60_000));
        mid("sleep(ms,ns)", () -> Thread.sleep(60_000, 1));
        mid("sleep(Duration)", () -> Thread.sleep(Duration.ofSeconds(60)));
        mid("TimeUnit.sleep", () -> TimeUnit.SECONDS.sleep(60));

        state("sleep(ms)", () -> Thread.sleep(60_000));
        state("sleep(Duration)", () -> Thread.sleep(Duration.ofSeconds(60)));
    }
}
