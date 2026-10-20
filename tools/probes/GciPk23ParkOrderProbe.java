// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.time.Duration;
import java.util.concurrent.locks.LockSupport;

/**
 * gci i23/pk (finding 5 of gci-i22rv-i21-default-path-review-findings): a
 * park that returns on a pending interrupt still consumes the permit, as
 * HotSpot 25's `Parker::park` takes `_counter` before its interrupt test (and
 * `VirtualThread.park` runs `getAndSetParkPermit(false) || interrupted`).
 * Before the fix the VM tested the interrupt first and kept the permit, so
 * the next park returned at once.
 *
 *   GciPk23ParkOrderProbe platform|virtual
 *
 * Each case: set up the state, `park()` (returns at once), clear the
 * interrupt, then `parkNanos(300 ms)`; the second park either waited
 * (>= 150 ms) or returned at once.
 *   both:           interrupt + unpark   -> HotSpot: waited (the permit was consumed)
 *   permit-only:    unpark               -> waited (control)
 *   interrupt-only: interrupt            -> waited (control)
 *
 * stdout (deterministic, the same on HotSpot):
 *   pk23 <mode> both second=waited
 *   pk23 <mode> permit-only second=waited
 *   pk23 <mode> interrupt-only second=waited
 * stderr: the measured milliseconds.
 */
public class GciPk23ParkOrderProbe {
    static final long SECOND_NS = 300_000_000L;
    static final long WAITED_MS = 150L;

    static String runCase(String name) {
        Thread me = Thread.currentThread();
        if (!name.equals("permit-only")) {
            me.interrupt();
        }
        if (!name.equals("interrupt-only")) {
            LockSupport.unpark(me);
        }
        LockSupport.park();
        boolean wasInterrupted = Thread.interrupted();
        long s = System.nanoTime();
        LockSupport.parkNanos(SECOND_NS);
        long ms = (System.nanoTime() - s) / 1_000_000L;
        // Leave no permit behind for the next case, whatever happened.
        LockSupport.unpark(me);
        LockSupport.park();
        Thread.interrupted();
        System.err.println("[pk23] " + name + " firstInterrupted=" + wasInterrupted + " secondMs=" + ms);
        return ms >= WAITED_MS ? "waited" : "at-once";
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "platform";
        String[] cases = {"both", "permit-only", "interrupt-only"};
        String[] answers = new String[cases.length];
        Runnable body = () -> {
            for (int i = 0; i < cases.length; i++) {
                answers[i] = runCase(cases[i]);
            }
        };
        if (mode.equals("virtual")) {
            Thread vt = Thread.ofVirtual().name("pk23-vt").start(body);
            if (!vt.join(Duration.ofSeconds(30))) {
                System.out.println("pk23 virtual hung");
                return;
            }
        } else {
            Thread t = new Thread(body, "pk23-platform");
            t.start();
            t.join();
        }
        for (int i = 0; i < cases.length; i++) {
            System.out.println("pk23 " + mode + " " + cases[i] + " second=" + answers[i]);
        }
    }
}
