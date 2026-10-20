// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L6 -- behavioural probe for
// docs/internal/fixed-bugs/interpreter-i74-L6-interrupt-before-start-is-not-seen-by-park-or-sleep-FIXED-20261111.md:
// an interrupt given before Thread.start() is recorded in the Thread.interrupted
// field only (JDK 25 interrupt() sets it, interrupt0 finds no live thread).
// HotSpot's JavaThread::is_interrupted reads that field, so the started
// thread's first park returns at once (the status stays set), its first sleep
// throws InterruptedException("sleep interrupted") and clears it, and its first
// Object.wait throws InterruptedException and clears it.
//
// Before the fix the VM flag was created false at start and nothing seeded it
// from the field: the park ran its whole 2 s, the sleep slept 2 s.
//
// Run, each mode:
//   cratonvm L6W79InterruptBeforeStart
//   cratonvm --compatible L6W79InterruptBeforeStart
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// park: fast=true interrupted-after=true
// sleep: fast=true threw=java.lang.InterruptedException: sleep interrupted interrupted-after=false
// wait: fast=true threw=java.lang.InterruptedException interrupted-after=false
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.locks.LockSupport;

public class L6W79InterruptBeforeStart {
    static final long LONG_NS = 2_000_000_000L;
    static final long FAST_NS = 1_000_000_000L;

    interface Body {
        String run() throws Exception;
    }

    static String row(String name, Body body) throws Exception {
        String[] out = new String[1];
        Thread t = new Thread(() -> {
            long t0 = System.nanoTime();
            String what;
            try {
                what = body.run();
            } catch (Exception e) {
                what = "threw=" + e.getClass().getName()
                        + (e.getMessage() == null ? "" : ": " + e.getMessage());
            }
            boolean fast = System.nanoTime() - t0 < FAST_NS;
            out[0] = name + ": fast=" + fast + (what.isEmpty() ? "" : " " + what)
                    + " interrupted-after=" + Thread.currentThread().isInterrupted();
        }, "w79-" + name);
        t.interrupt(); // before start: lands in the Thread.interrupted field only
        t.start();
        t.join();
        return out[0];
    }

    public static void main(String[] args) throws Exception {
        System.out.println(row("park", () -> {
            LockSupport.parkNanos(LONG_NS);
            return "";
        }));
        System.out.println(row("sleep", () -> {
            Thread.sleep(LONG_NS / 1_000_000L);
            return "slept";
        }));
        Object lock = new Object();
        System.out.println(row("wait", () -> {
            synchronized (lock) {
                lock.wait(LONG_NS / 1_000_000L);
            }
            return "waited";
        }));
        System.out.println("done");
    }
}
